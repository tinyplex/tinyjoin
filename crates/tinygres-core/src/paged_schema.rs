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

pub(crate) struct AddColumnPlan {
    pub base_revision: u64,
    pub header: (Vec<u8>, Vec<u8>),
    pub old_schema: TableSchema,
    pub new_schema: TableSchema,
    pub old_tree: DroppedTree,
    pub new_tree_id: TreeId,
    pub row_count: usize,
    pub fixed_work_bytes: usize,
}

pub(crate) struct PublishedColumnAddition {
    pub revision: u64,
    pub schema: TableSchema,
    pub tree_id: TreeId,
    pub root_page_id: Option<PageId>,
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

/// Rewrites one rooted table through a bounded candidate cursor, or publishes a metadata-only
/// schema record for an empty table. Existing index trees remain byte-logically unchanged because
/// an appended non-key column cannot change any of their entries.
pub(crate) fn publish_added_column<D: PageDevice>(
    pager: &mut Pager<D>,
    plan: AddColumnPlan,
) -> Result<PublishedColumnAddition> {
    let applied_journal_sequence = pager.applied_journal_sequence();
    let catalog_root = pager
        .catalog_root_page_id()
        .ok_or_else(|| storage_corrupt("ALTER TABLE requires an existing paged catalog root"))?;
    let column = plan.new_schema.columns.last().ok_or_else(|| {
        storage_corrupt("ALTER TABLE prospective schema is missing its appended column")
    })?;
    let default_work = column
        .default
        .as_ref()
        .map(crate::storage::estimated_value_bytes)
        .transpose()?
        .unwrap_or(16);
    let column_name_work = column
        .name
        .len()
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(64))
        .ok_or_else(alter_work_limit)?;
    let mut transaction = pager.begin_write()?;
    let result = (|| {
        let mut new_root_page_id = None;
        let mut rewritten_count = 0usize;
        if let Some(old_root_page_id) = plan.old_tree.root_page_id {
            let mut rows = Btree::cursor_in_transaction(
                &mut transaction,
                old_root_page_id,
                plan.old_tree.tree_id,
            )?;
            while let Some((primary_key, value)) = rows.next_in_transaction(&mut transaction)? {
                ensure_alter_decode_work(plan.fixed_work_bytes, &primary_key, &value)?;
                let mut row = validated_candidate_row(&plan.old_schema, &primary_key, &value)?;
                let old_row_work = crate::storage::estimated_row_bytes(&row)?;
                ensure_alter_transform_work(
                    plan.fixed_work_bytes,
                    primary_key.len(),
                    value.len(),
                    old_row_work,
                    default_work,
                    column_name_work,
                )?;
                let added_value = column.default.clone().unwrap_or(serde_json::Value::Null);
                if row.insert(column.name.clone(), added_value).is_some() {
                    return Err(storage_corrupt(format!(
                        "Stored row in `{}` already contains new column `{}`",
                        plan.old_schema.name, column.name
                    )));
                }
                let row = normalize_row(&plan.new_schema, row)?;
                let encoded_primary_key = encode_primary_key(&plan.new_schema, &row)?;
                if encoded_primary_key != primary_key {
                    return Err(storage_corrupt(format!(
                        "ALTER TABLE changed a primary key in `{}`",
                        plan.old_schema.name
                    )));
                }
                let new_row_work = crate::storage::estimated_row_bytes(&row)?;
                let encoded_row_work = crate::storage::logical_row_encoded_bytes(&row)?
                    .checked_add(8)
                    .ok_or_else(alter_work_limit)?;
                // Semantic row transformation intentionally precedes the physical work limit so
                // legacy row-size/type errors retain their ordering. The exact encoded length is
                // computed without allocation, then bounded before the codec constructs bytes.
                ensure_alter_row_work(
                    plan.fixed_work_bytes,
                    primary_key.len(),
                    value.len(),
                    old_row_work,
                    new_row_work,
                    encoded_row_work,
                )?;
                let encoded_row = encode_row(&row)?;
                debug_assert_eq!(encoded_row.len(), encoded_row_work);
                let root = match new_root_page_id {
                    Some(root) => root,
                    None => Btree::create(&mut transaction, plan.new_tree_id)?,
                };
                new_root_page_id = Some(Btree::upsert(
                    &mut transaction,
                    root,
                    plan.new_tree_id,
                    &primary_key,
                    &encoded_row,
                )?);
                rewritten_count = rewritten_count
                    .checked_add(1)
                    .ok_or_else(|| storage_corrupt("ALTER TABLE row count overflowed"))?;
            }
            if rewritten_count != plan.row_count {
                return Err(storage_corrupt(format!(
                    "Table `{}` catalog count {} does not match {rewritten_count} rewritten rows",
                    plan.old_schema.name, plan.row_count
                )));
            }
            Btree::reclaim(&mut transaction, old_root_page_id, plan.old_tree.tree_id)?;
        } else if plan.row_count != 0 {
            return Err(storage_corrupt(format!(
                "Rootless table `{}` has {} rows",
                plan.old_schema.name, plan.row_count
            )));
        }

        let revision = plan
            .base_revision
            .checked_add(1)
            .ok_or_else(|| EngineError::new("REVISION_OVERFLOW", "Database revision overflowed"))?;
        let (table_key, table_value) = encode_catalog_table_record(&CatalogTableRecord {
            schema: plan.new_schema.clone(),
            tree_id: plan.new_tree_id,
            root_page_id: new_root_page_id,
            row_count: plan.row_count as u64,
        })?;
        let catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &plan.header.0,
            &plan.header.1,
        )?;
        let catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &table_key,
            &table_value,
        )?;
        Ok((revision, catalog_root, new_root_page_id))
    })();
    let (revision, catalog_root, root_page_id) = match result {
        Ok(result) => result,
        Err(error) => {
            transaction.abort();
            return Err(error);
        }
    };
    transaction.commit(revision, applied_journal_sequence, Some(catalog_root))?;
    Ok(PublishedColumnAddition {
        revision,
        schema: plan.new_schema,
        tree_id: plan.new_tree_id,
        root_page_id,
    })
}

fn ensure_alter_decode_work(fixed: usize, key: &[u8], encoded_value: &[u8]) -> Result<()> {
    // Candidate cursor key/value, serde's parsed row, canonical re-encoding performed by the row
    // decoder, and normalization's validation clone are simultaneously live before exact decoded
    // sizes are available. Structurally scan the encoded JSON body so compact container values do
    // not evade the bound through a constant byte multiplier.
    const ROW_HEADER_BYTES: usize = 8;
    let body = encoded_value.get(ROW_HEADER_BYTES..).ok_or_else(|| {
        storage_corrupt("Encoded ALTER source row is shorter than its record header")
    })?;
    let model_work = crate::storage::estimated_encoded_json_model_bytes(body)
        .map_err(|error| storage_corrupt(error.message))?;
    let bytes = fixed
        .checked_add(key.len())
        .and_then(|bytes| bytes.checked_add(encoded_value.len()))
        .and_then(|bytes| {
            model_work
                .checked_mul(2)
                .and_then(|models| bytes.checked_add(models))
        })
        // Canonical re-encoding performed by decode_row.
        .and_then(|bytes| bytes.checked_add(body.len()))
        .and_then(|bytes| bytes.checked_add(1024))
        .ok_or_else(alter_work_limit)?;
    ensure_alter_work_bytes(bytes)
}

fn ensure_alter_transform_work(
    fixed: usize,
    key: usize,
    encoded_value: usize,
    old_row: usize,
    default_value: usize,
    column_name: usize,
) -> Result<()> {
    // Charge the row retained from decode, its imminent default/name clones, and the later
    // normalize/schema-validation clone before retaining either appended value.
    let bytes = fixed
        .checked_add(key)
        .and_then(|bytes| bytes.checked_add(encoded_value))
        .and_then(|bytes| {
            old_row
                .checked_mul(3)
                .and_then(|rows| bytes.checked_add(rows))
        })
        .and_then(|bytes| {
            default_value
                .checked_mul(2)
                .and_then(|values| bytes.checked_add(values))
        })
        .and_then(|bytes| bytes.checked_add(column_name))
        .and_then(|bytes| bytes.checked_add(512))
        .ok_or_else(alter_work_limit)?;
    ensure_alter_work_bytes(bytes)
}

fn ensure_alter_row_work(
    fixed: usize,
    key: usize,
    old_value: usize,
    old_row: usize,
    new_row: usize,
    encoded_row: usize,
) -> Result<()> {
    let bytes = fixed
        .checked_add(key)
        .and_then(|bytes| bytes.checked_add(old_value))
        .and_then(|bytes| {
            old_row
                .checked_mul(2)
                .and_then(|row| bytes.checked_add(row))
        })
        .and_then(|bytes| {
            new_row
                .checked_mul(2)
                .and_then(|row| bytes.checked_add(row))
        })
        .and_then(|bytes| bytes.checked_add(encoded_row))
        .and_then(|bytes| bytes.checked_add(512))
        .ok_or_else(alter_work_limit)?;
    ensure_alter_work_bytes(bytes)
}

fn ensure_alter_work_bytes(bytes: usize) -> Result<()> {
    if bytes > 16 * 1024 * 1024 {
        Err(alter_work_limit())
    } else {
        Ok(())
    }
}

fn alter_work_limit() -> EngineError {
    EngineError::new(
        "TRANSACTION_TOO_LARGE",
        "ALTER TABLE cannot require more than 16777216 bytes of transient row work",
    )
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn alter_transient_preflights_have_exact_before_allocation_boundaries() {
        const LIMIT: usize = 16 * 1024 * 1024;
        // Decode: encoded record + two parsed/normalized models + canonical re-encoding.
        let small = crate::paged_codec::encode_row(
            json!({"id": 1}).as_object().expect("fixture is an object"),
        )
        .unwrap();
        ensure_alter_decode_work(0, &[], &small).unwrap();
        let compact_array = crate::paged_codec::encode_row(
            json!({"id": 1, "values": vec![serde_json::Value::Null; 190_000]})
                .as_object()
                .expect("fixture is an object"),
        )
        .unwrap();
        assert!(compact_array.len() < crate::paged_codec::MAX_PAGED_VALUE_BYTES);
        assert_eq!(
            ensure_alter_decode_work(0, &[], &compact_array)
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );

        // Transform: fixed + key + encoded + 3*old row + 2*default + name + 512.
        let old_row = (LIMIT - 512) / 3;
        ensure_alter_transform_work(0, 0, 0, old_row, 0, 0).unwrap();
        assert_eq!(
            ensure_alter_transform_work(0, 3, 0, old_row, 0, 0)
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );
    }
}
