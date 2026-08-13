use crate::paged_codec::CATALOG_TREE_ID;
use crate::{Btree, EngineError, PageDevice, PageId, Pager, Result, TreeId};

pub(crate) struct DroppedTree {
    pub tree_id: TreeId,
    pub root_page_id: Option<PageId>,
}

pub(crate) struct SchemaDropPlan {
    pub revision: u64,
    pub header: (Vec<u8>, Vec<u8>),
    pub catalog_keys: Vec<Vec<u8>>,
    pub trees: Vec<DroppedTree>,
}

/// Reclaims complete relational trees and removes their catalog records in one pager generation.
///
/// Each tree is fully validated by [`Btree::reclaim`] before any of its pages are marked free.
/// Catalog changes happen only after every tree validates, and an error aborts the candidate so
/// neither earlier frees nor catalog copy-on-write pages can become visible.
pub(crate) fn publish_schema_drop<D: PageDevice>(
    pager: &mut Pager<D>,
    plan: SchemaDropPlan,
) -> Result<()> {
    let applied_journal_sequence = pager.applied_journal_sequence();
    let catalog_root = pager.catalog_root_page_id().ok_or_else(|| {
        EngineError::new(
            "STORAGE_CORRUPT",
            "A non-empty paged catalog must have a published root",
        )
    })?;
    let mut transaction = pager.begin_write()?;
    let result = (|| {
        for tree in &plan.trees {
            if let Some(root_page_id) = tree.root_page_id {
                Btree::reclaim(&mut transaction, root_page_id, tree.tree_id)?;
            }
        }

        let mut catalog_root = catalog_root;
        for key in &plan.catalog_keys {
            let (next_root, removed) =
                Btree::delete(&mut transaction, catalog_root, CATALOG_TREE_ID, key)?;
            if !removed {
                return Err(EngineError::new(
                    "STORAGE_CORRUPT",
                    "A dropped schema object is missing from the paged catalog",
                ));
            }
            catalog_root = next_root.ok_or_else(|| {
                EngineError::new(
                    "STORAGE_CORRUPT",
                    "Dropping a schema object removed the catalog header",
                )
            })?;
        }
        Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &plan.header.0,
            &plan.header.1,
        )
    })();
    let catalog_root = match result {
        Ok(catalog_root) => catalog_root,
        Err(error) => {
            transaction.abort();
            return Err(error);
        }
    };
    transaction.commit(plan.revision, applied_journal_sequence, Some(catalog_root))
}
