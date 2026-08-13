use crate::paged_codec::{
    CATALOG_TREE_ID, CatalogIndexRecord, CatalogTableRecord, encode_catalog_index_record,
    encode_catalog_table_record, encode_primary_key, encode_row, encode_secondary_index_entry_key,
    encode_secondary_index_prefix, secondary_index_entry_matches_prefix,
};
use crate::storage::normalize_row;
use crate::{
    Btree, EngineError, IndexDefinition, PageDevice, PageId, Pager, Result, Row, TableSchema,
    TreeId,
};

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

pub(crate) struct ReplacementIndex {
    pub definition: IndexDefinition,
    pub tree_id: TreeId,
    pub old_tree: Option<DroppedTree>,
}

pub(crate) struct TableReplacementPlan {
    pub revision: u64,
    pub header: (Vec<u8>, Vec<u8>),
    pub schema: TableSchema,
    pub rows: Vec<Row>,
    pub table_tree_id: TreeId,
    pub old_table: Option<DroppedTree>,
    pub indexes: Vec<ReplacementIndex>,
}

pub(crate) struct PublishedIndex {
    pub definition: IndexDefinition,
    pub tree_id: TreeId,
    pub root_page_id: Option<PageId>,
    pub entry_count: usize,
}

pub(crate) struct PublishedTableReplacement {
    pub schema: TableSchema,
    pub table_root_page_id: Option<PageId>,
    pub row_count: usize,
    pub indexes: Vec<PublishedIndex>,
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

/// Builds a fresh table and its affected indexes before replacing their catalog records.
///
/// The public snapshot boundary currently supplies a buffered `Vec<Row>`. Within the pager
/// candidate, however, each index is rebuilt by a bounded cursor over the freshly built table;
/// rows are never duplicated into an index-building collection.
pub(crate) fn publish_table_replacement<D: PageDevice>(
    pager: &mut Pager<D>,
    plan: TableReplacementPlan,
) -> Result<PublishedTableReplacement> {
    let applied_journal_sequence = pager.applied_journal_sequence();
    let existing_catalog_root = pager.catalog_root_page_id();
    let mut transaction = pager.begin_write()?;
    let result = (|| {
        let mut table_root_page_id = None;
        for row in &plan.rows {
            let key = encode_primary_key(&plan.schema, row)?;
            let value = encode_row(row)?;
            let root = match table_root_page_id {
                Some(root) => root,
                None => Btree::create(&mut transaction, plan.table_tree_id)?,
            };
            table_root_page_id = Some(Btree::upsert(
                &mut transaction,
                root,
                plan.table_tree_id,
                &key,
                &value,
            )?);
        }

        let mut published_indexes = Vec::with_capacity(plan.indexes.len());
        for index in &plan.indexes {
            let mut index_root_page_id = None;
            let mut entry_count = 0usize;
            if let Some(table_root_page_id) = table_root_page_id {
                let mut rows = Btree::cursor_in_transaction(
                    &mut transaction,
                    table_root_page_id,
                    plan.table_tree_id,
                )?;
                while let Some((primary_key, value)) = rows.next_in_transaction(&mut transaction)? {
                    let row = validated_candidate_row(&plan.schema, &primary_key, &value)?;
                    let Some(index_key) =
                        encode_secondary_index_entry_key(&plan.schema, &index.definition, &row)?
                    else {
                        continue;
                    };
                    if index.definition.unique {
                        let prefix =
                            encode_secondary_index_prefix(&plan.schema, &index.definition, &row)?
                                .expect("an encoded index entry always has a prefix");
                        if let Some(root) = index_root_page_id {
                            let mut existing = Btree::cursor_from_in_transaction(
                                &mut transaction,
                                root,
                                index.tree_id,
                                &prefix,
                            )?;
                            if let Some((existing_key, existing_value)) =
                                existing.next_in_transaction(&mut transaction)?
                            {
                                if !existing_value.is_empty() {
                                    return Err(storage_corrupt(format!(
                                        "New secondary index `{}` contains a non-empty value",
                                        index.definition.name
                                    )));
                                }
                                if secondary_index_entry_matches_prefix(&existing_key, &prefix) {
                                    return Err(EngineError::constraint_violation(format!(
                                        "Index `{}` would contain duplicate values",
                                        index.definition.name
                                    )));
                                }
                            }
                        }
                    }
                    let root = match index_root_page_id {
                        Some(root) => root,
                        None => Btree::create(&mut transaction, index.tree_id)?,
                    };
                    index_root_page_id = Some(Btree::upsert(
                        &mut transaction,
                        root,
                        index.tree_id,
                        &index_key,
                        &[],
                    )?);
                    entry_count = entry_count
                        .checked_add(1)
                        .ok_or_else(|| storage_corrupt("The index entry count overflowed"))?;
                }
            }
            published_indexes.push((index_root_page_id, entry_count));
        }

        // Reclaim child indexes before their table. If any later tree is malformed, aborting the
        // pager candidate discards all earlier bitmap changes.
        for index in &plan.indexes {
            if let Some(old_tree) = &index.old_tree
                && let Some(root_page_id) = old_tree.root_page_id
            {
                Btree::reclaim(&mut transaction, root_page_id, old_tree.tree_id)?;
            }
        }
        if let Some(old_table) = &plan.old_table
            && let Some(root_page_id) = old_table.root_page_id
        {
            Btree::reclaim(&mut transaction, root_page_id, old_table.tree_id)?;
        }

        let mut catalog_root = match existing_catalog_root {
            Some(root) => root,
            None => Btree::create(&mut transaction, CATALOG_TREE_ID)?,
        };
        catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &plan.header.0,
            &plan.header.1,
        )?;
        let (key, value) = encode_catalog_table_record(&CatalogTableRecord {
            schema: plan.schema.clone(),
            tree_id: plan.table_tree_id,
            root_page_id: table_root_page_id,
            row_count: plan.rows.len() as u64,
        })?;
        catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &key,
            &value,
        )?;
        for (index, (root_page_id, entry_count)) in plan.indexes.iter().zip(&published_indexes) {
            let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
                definition: index.definition.clone(),
                tree_id: index.tree_id,
                root_page_id: *root_page_id,
                entry_count: *entry_count as u64,
            })?;
            catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &key,
                &value,
            )?;
        }
        Ok((catalog_root, table_root_page_id, published_indexes))
    })();
    let (catalog_root, table_root_page_id, indexes) = match result {
        Ok(result) => result,
        Err(error) => {
            transaction.abort();
            return Err(error);
        }
    };
    transaction.commit(plan.revision, applied_journal_sequence, Some(catalog_root))?;
    let row_count = plan.rows.len();
    let indexes = plan
        .indexes
        .into_iter()
        .zip(indexes)
        .map(|(index, (root_page_id, entry_count))| PublishedIndex {
            definition: index.definition,
            tree_id: index.tree_id,
            root_page_id,
            entry_count,
        })
        .collect();
    Ok(PublishedTableReplacement {
        schema: plan.schema,
        table_root_page_id,
        row_count,
        indexes,
    })
}

fn validated_candidate_row(schema: &TableSchema, key: &[u8], value: &[u8]) -> Result<Row> {
    let row = crate::paged_codec::decode_row(value)?;
    let normalized = normalize_row(schema, row.clone()).map_err(|error| {
        storage_corrupt(format!(
            "Candidate row in `{}` does not match its schema: {}",
            schema.name, error.message
        ))
    })?;
    if normalized != row || encode_primary_key(schema, &row)? != key {
        return Err(storage_corrupt(format!(
            "Candidate row in `{}` does not match its encoded table entry",
            schema.name
        )));
    }
    Ok(row)
}

fn storage_corrupt(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_CORRUPT", message)
}
