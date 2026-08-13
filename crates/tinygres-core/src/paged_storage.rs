use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashSet},
};

use crate::{
    Btree, EngineError, InMemoryStorage, IndexDefinition, PageDevice, PageId, Pager, Result, Row,
    StorageReader, TableSchema, TreeId, VisitControl, VisitOutcome,
    paged_codec::{
        CATALOG_TREE_ID, CatalogHeader, CatalogIndexRecord, CatalogKey, CatalogTableRecord,
        FIRST_USER_TREE_ID, decode_catalog_header_record, decode_catalog_index_record,
        decode_catalog_key, decode_catalog_table_record, decode_row, encode_catalog_header_record,
        encode_catalog_index_record, encode_catalog_table_record, encode_primary_key, encode_row,
        encode_secondary_index_entry_key, encode_secondary_index_prefix,
        secondary_index_entry_matches_prefix, secondary_index_primary_key,
        secondary_index_primary_key_for_definition,
    },
    storage::normalize_row,
};

/// A read-only relational view over the crash-safe paged B-tree store.
///
/// This first slice deliberately exposes no mutation contract. [`Self::from_in_memory`] is a
/// deterministic one-shot importer used to prove the on-disk layout and query path; normal SQL
/// writes continue to use [`InMemoryStorage`] until page-native row deltas are implemented.
pub struct PagedStorage<D: PageDevice> {
    pager: RefCell<Pager<D>>,
    revision: u64,
    tables: BTreeMap<String, PagedTable>,
    indexes: BTreeMap<String, PagedIndex>,
}

#[derive(Clone, Debug)]
struct PagedTable {
    schema: TableSchema,
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    row_count: usize,
}

#[derive(Clone, Debug)]
struct PagedIndex {
    definition: IndexDefinition,
    tree_id: TreeId,
    root_page_id: Option<PageId>,
}

impl<D: PageDevice> PagedStorage<D> {
    /// Opens and validates a previously published paged database.
    pub fn open(device: D) -> Result<Self> {
        let mut pager = Pager::open_or_create(device)?;
        let revision = pager.database_revision();
        let Some(catalog_root_page_id) = pager.catalog_root_page_id() else {
            if revision != 0 || pager.active_metadata().superblock.live_data_page_count != 0 {
                return Err(storage_corrupt(
                    "A database without a catalog root must have revision zero and no live data pages",
                ));
            }
            return Ok(Self {
                pager: RefCell::new(pager),
                revision,
                tables: BTreeMap::new(),
                indexes: BTreeMap::new(),
            });
        };

        let (tables, indexes) = load_and_validate_catalog(&mut pager, catalog_root_page_id)?;
        Ok(Self {
            pager: RefCell::new(pager),
            revision,
            tables,
            indexes,
        })
    }

    /// Imports an in-memory database into one atomic paged generation.
    ///
    /// Tree IDs and insertion order are deterministic: tables and indexes are ordered by name,
    /// with all table trees allocated before index trees. The target device must not already hold
    /// a published catalog.
    pub fn from_in_memory(device: D, source: &InMemoryStorage) -> Result<Self> {
        let mut pager = Pager::open_or_create(device)?;
        if pager.catalog_root_page_id().is_some()
            || pager.database_revision() != 0
            || pager.active_metadata().superblock.live_data_page_count != 0
        {
            return Err(EngineError::new(
                "DATABASE_NOT_EMPTY",
                "Paged import requires a device without a published database",
            ));
        }

        let table_inputs = source
            .table_names()
            .map(|name| {
                Ok(TableInput {
                    schema: source.table_schema(name)?,
                    rows: source.table_rows(name)?.values().cloned().collect(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let index_inputs = source
            .index_names()
            .map(|name| {
                source.index_definition(name).ok_or_else(|| {
                    storage_corrupt(format!("In-memory index `{name}` has no definition"))
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let tree_count = table_inputs
            .len()
            .checked_add(index_inputs.len())
            .ok_or_else(|| limit_error("The catalog tree count overflowed"))?;
        let next_tree_id = FIRST_USER_TREE_ID
            .checked_add(
                u64::try_from(tree_count)
                    .map_err(|_| limit_error("The catalog contains too many trees"))?,
            )
            .ok_or_else(|| limit_error("The catalog tree ID range is exhausted"))?;
        let table_count = u32::try_from(table_inputs.len())
            .map_err(|_| limit_error("The catalog contains too many tables"))?;
        let index_count = u32::try_from(index_inputs.len())
            .map_err(|_| limit_error("The catalog contains too many indexes"))?;

        let revision = source.revision();
        let mut transaction = pager.begin_write()?;
        let mut next_id = FIRST_USER_TREE_ID;
        let mut table_records = Vec::with_capacity(table_inputs.len());
        for input in &table_inputs {
            let tree_id = next_id;
            next_id = next_id
                .checked_add(1)
                .ok_or_else(|| limit_error("The catalog tree ID range is exhausted"))?;
            let mut root = None;
            let mut encoded_keys = HashSet::with_capacity(input.rows.len());
            for row in &input.rows {
                let normalized = normalize_row(&input.schema, row.clone())?;
                if normalized != *row {
                    return Err(storage_corrupt(format!(
                        "In-memory row in `{}` is not normalized to its schema",
                        input.schema.name
                    )));
                }
                let key = encode_primary_key(&input.schema, row)?;
                if !encoded_keys.insert(key.clone()) {
                    return Err(EngineError::constraint_violation(format!(
                        "Rows in `{}` collide after canonical primary-key encoding",
                        input.schema.name
                    )));
                }
                let current = match root {
                    Some(root) => root,
                    None => Btree::create(&mut transaction, tree_id)?,
                };
                root = Some(Btree::upsert(
                    &mut transaction,
                    current,
                    tree_id,
                    &key,
                    &encode_row(row)?,
                )?);
            }
            table_records.push(CatalogTableRecord {
                schema: input.schema.clone(),
                tree_id,
                root_page_id: root,
                row_count: input.rows.len() as u64,
            });
        }

        let table_by_name = table_inputs
            .iter()
            .map(|input| (input.schema.name.as_str(), input))
            .collect::<BTreeMap<_, _>>();
        let mut index_records = Vec::with_capacity(index_inputs.len());
        for definition in &index_inputs {
            let tree_id = next_id;
            next_id = next_id
                .checked_add(1)
                .ok_or_else(|| limit_error("The catalog tree ID range is exhausted"))?;
            let table = table_by_name
                .get(definition.table.as_str())
                .ok_or_else(|| {
                    storage_corrupt(format!(
                        "Index `{}` references missing table `{}`",
                        definition.name, definition.table
                    ))
                })?;
            let mut root = None;
            let mut entry_count = 0_u64;
            let mut unique_prefixes = HashSet::new();
            for row in &table.rows {
                let Some(key) = encode_secondary_index_entry_key(&table.schema, definition, row)?
                else {
                    continue;
                };
                if definition.unique {
                    let prefix = encode_secondary_index_prefix(&table.schema, definition, row)?
                        .expect("an encoded index entry always has a prefix");
                    if !unique_prefixes.insert(prefix) {
                        return Err(EngineError::constraint_violation(format!(
                            "Index `{}` would contain duplicate values",
                            definition.name
                        )));
                    }
                }
                let current = match root {
                    Some(root) => root,
                    None => Btree::create(&mut transaction, tree_id)?,
                };
                root = Some(Btree::upsert(
                    &mut transaction,
                    current,
                    tree_id,
                    &key,
                    &[],
                )?);
                entry_count += 1;
            }
            index_records.push(CatalogIndexRecord {
                definition: definition.clone(),
                tree_id,
                root_page_id: root,
                entry_count,
            });
        }
        debug_assert_eq!(next_id, next_tree_id);

        let mut catalog_root = Btree::create(&mut transaction, CATALOG_TREE_ID)?;
        let (key, value) = encode_catalog_header_record(&CatalogHeader {
            next_tree_id,
            table_count,
            index_count,
        })?;
        catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &key,
            &value,
        )?;
        for record in &table_records {
            let (key, value) = encode_catalog_table_record(record)?;
            catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &key,
                &value,
            )?;
        }
        for record in &index_records {
            let (key, value) = encode_catalog_index_record(record)?;
            catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &key,
                &value,
            )?;
        }
        transaction.commit(revision, 0, Some(catalog_root))?;

        let (tables, indexes) = load_and_validate_catalog(&mut pager, catalog_root)?;
        Ok(Self {
            pager: RefCell::new(pager),
            revision,
            tables,
            indexes,
        })
    }

    pub fn into_device(self) -> D {
        self.pager.into_inner().into_device()
    }
}

struct TableInput {
    schema: TableSchema,
    rows: Vec<Row>,
}

impl<D: PageDevice> StorageReader for PagedStorage<D> {
    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let Some(root) = table.root_page_id else {
            return Ok(VisitOutcome::Complete);
        };
        let mut cursor = Btree::cursor(&mut self.pager.borrow_mut(), root, table.tree_id)?;
        loop {
            let next = cursor.next(&mut self.pager.borrow_mut())?;
            let Some((key, value)) = next else {
                break;
            };
            let row = validated_row(&table.schema, &key, &value)?;
            if visitor(&row)? == VisitControl::Stop {
                return Ok(VisitOutcome::Stopped);
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn table_row_count(&self, table: &str) -> Result<usize> {
        self.tables
            .get(table)
            .map(|table| table.row_count)
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let encoded_key = encode_primary_key(&table.schema, key)?;
        let Some(root) = table.root_page_id else {
            return Ok(None);
        };
        Btree::get(
            &mut self.pager.borrow_mut(),
            root,
            table.tree_id,
            &encoded_key,
        )?
        .map(|value| validated_row(&table.schema, &encoded_key, &value))
        .transpose()
    }

    fn index_definition(&self, name: &str) -> Option<IndexDefinition> {
        self.indexes.get(name).map(|index| index.definition.clone())
    }

    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>> {
        if !self.tables.contains_key(table) {
            return Err(EngineError::table_not_found(table));
        }
        Ok(self
            .indexes
            .values()
            .filter(|index| index.definition.table == table)
            .map(|index| index.definition.clone())
            .collect())
    }

    fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        let table_data = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let Some(index) = self
            .indexes
            .values()
            .find(|index| index.definition.table == table && index.definition.columns == columns)
        else {
            return Ok(None);
        };
        let Some(prefix) =
            encode_secondary_index_prefix(&table_data.schema, &index.definition, key)?
        else {
            return Ok(Some(VisitOutcome::Complete));
        };
        let Some(index_root) = index.root_page_id else {
            return Ok(Some(VisitOutcome::Complete));
        };
        let table_root = table_data.root_page_id.ok_or_else(|| {
            storage_corrupt(format!(
                "Non-empty index `{}` references empty table `{table}`",
                index.definition.name
            ))
        })?;

        let mut cursor = Btree::cursor_from(
            &mut self.pager.borrow_mut(),
            index_root,
            index.tree_id,
            &prefix,
        )?;
        loop {
            let next = cursor.next(&mut self.pager.borrow_mut())?;
            let Some((entry_key, value)) = next else {
                break;
            };
            if !secondary_index_entry_matches_prefix(&entry_key, &prefix) {
                break;
            }
            if !value.is_empty() {
                return Err(storage_corrupt(format!(
                    "Secondary index `{}` contains a non-empty value",
                    index.definition.name
                )));
            }
            let primary_key = secondary_index_primary_key_for_definition(
                &table_data.schema,
                &index.definition,
                &entry_key,
            )?;
            if secondary_index_primary_key(&entry_key, &prefix)? != primary_key {
                return Err(storage_corrupt(format!(
                    "Secondary index `{}` tuple boundary is inconsistent",
                    index.definition.name
                )));
            }
            let row_value = Btree::get(
                &mut self.pager.borrow_mut(),
                table_root,
                table_data.tree_id,
                primary_key,
            )?
            .ok_or_else(|| {
                storage_corrupt(format!(
                    "Secondary index `{}` contains a dangling primary key",
                    index.definition.name
                ))
            })?;
            let row = validated_row(&table_data.schema, primary_key, &row_value)?;
            if encode_secondary_index_entry_key(&table_data.schema, &index.definition, &row)?
                .as_deref()
                != Some(entry_key.as_slice())
            {
                return Err(storage_corrupt(format!(
                    "Secondary index `{}` entry does not match its table row",
                    index.definition.name
                )));
            }
            if visitor(&row)? == VisitControl::Stop {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    fn table_schema(&self, table: &str) -> Result<TableSchema> {
        self.tables
            .get(table)
            .map(|table| table.schema.clone())
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

fn load_and_validate_catalog<D: PageDevice>(
    pager: &mut Pager<D>,
    catalog_root_page_id: PageId,
) -> Result<(BTreeMap<String, PagedTable>, BTreeMap<String, PagedIndex>)> {
    let mut header = None;
    let mut table_records = BTreeMap::new();
    let mut index_records = BTreeMap::new();
    let mut cursor = Btree::cursor(pager, catalog_root_page_id, CATALOG_TREE_ID)?;
    while let Some((key, value)) = cursor.next(pager)? {
        match decode_catalog_key(&key)? {
            CatalogKey::Header => {
                if header
                    .replace(decode_catalog_header_record(&key, &value)?)
                    .is_some()
                {
                    return Err(storage_corrupt("The catalog contains multiple headers"));
                }
            }
            CatalogKey::Table(name) => {
                let record = decode_catalog_table_record(&key, &value)?;
                if table_records.insert(name.clone(), record).is_some() {
                    return Err(storage_corrupt(format!(
                        "The catalog contains table `{name}` more than once"
                    )));
                }
            }
            CatalogKey::Index(name) => {
                let record = decode_catalog_index_record(&key, &value)?;
                if index_records.insert(name.clone(), record).is_some() {
                    return Err(storage_corrupt(format!(
                        "The catalog contains index `{name}` more than once"
                    )));
                }
            }
        }
    }
    let header = header.ok_or_else(|| storage_corrupt("The catalog header is missing"))?;
    if header.table_count as usize != table_records.len()
        || header.index_count as usize != index_records.len()
    {
        return Err(storage_corrupt(format!(
            "Catalog header counts ({}, {}) do not match its table and index records ({}, {})",
            header.table_count,
            header.index_count,
            table_records.len(),
            index_records.len()
        )));
    }

    let mut tree_ids = BTreeSet::from([CATALOG_TREE_ID]);
    for record in table_records.values() {
        validate_catalog_tree_id(record.tree_id, header.next_tree_id, &mut tree_ids)?;
    }
    for record in index_records.values() {
        validate_catalog_tree_id(record.tree_id, header.next_tree_id, &mut tree_ids)?;
        validate_index_against_catalog(&record.definition, &table_records)?;
    }
    let expected_next_tree_id = FIRST_USER_TREE_ID
        .checked_add((table_records.len() + index_records.len()) as u64)
        .ok_or_else(|| storage_corrupt("The catalog tree ID range overflowed"))?;
    if header.next_tree_id != expected_next_tree_id {
        return Err(storage_corrupt(format!(
            "Catalog next tree ID {} does not match its {} trees",
            header.next_tree_id,
            table_records.len() + index_records.len()
        )));
    }
    if tree_ids.len() != 1 + table_records.len() + index_records.len() {
        return Err(storage_corrupt("Catalog tree IDs are not unique"));
    }

    for record in table_records.values() {
        validate_table_tree(pager, record)?;
    }
    for record in index_records.values() {
        validate_index_tree(pager, record, &table_records)?;
    }

    let tables = table_records
        .into_iter()
        .map(|(name, record)| {
            let row_count = usize::try_from(record.row_count)
                .map_err(|_| storage_corrupt("A table row count cannot fit in memory"))?;
            Ok((
                name,
                PagedTable {
                    schema: record.schema,
                    tree_id: record.tree_id,
                    root_page_id: record.root_page_id,
                    row_count,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let indexes = index_records
        .into_iter()
        .map(|(name, record)| {
            Ok((
                name,
                PagedIndex {
                    definition: record.definition,
                    tree_id: record.tree_id,
                    root_page_id: record.root_page_id,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok((tables, indexes))
}

fn validate_catalog_tree_id(
    tree_id: TreeId,
    next_tree_id: TreeId,
    seen: &mut BTreeSet<TreeId>,
) -> Result<()> {
    if !(FIRST_USER_TREE_ID..next_tree_id).contains(&tree_id) {
        return Err(storage_corrupt(format!(
            "Catalog tree ID {tree_id} is outside its declared range"
        )));
    }
    if !seen.insert(tree_id) {
        return Err(storage_corrupt(format!(
            "Catalog tree ID {tree_id} is used more than once"
        )));
    }
    Ok(())
}

fn validate_index_against_catalog(
    definition: &IndexDefinition,
    tables: &BTreeMap<String, CatalogTableRecord>,
) -> Result<()> {
    let table = tables.get(&definition.table).ok_or_else(|| {
        storage_corrupt(format!(
            "Index `{}` references missing table `{}`",
            definition.name, definition.table
        ))
    })?;
    if table.schema.columns.is_empty() {
        return Err(storage_corrupt(format!(
            "Index `{}` references a table without a typed catalog",
            definition.name
        )));
    }
    for column in &definition.columns {
        let column = table
            .schema
            .columns
            .iter()
            .find(|candidate| candidate.name == *column)
            .ok_or_else(|| {
                storage_corrupt(format!(
                    "Index `{}` references missing column `{column}`",
                    definition.name
                ))
            })?;
        if matches!(
            column.data_type,
            crate::ColumnType::Float | crate::ColumnType::Json
        ) {
            return Err(storage_corrupt(format!(
                "Index `{}` uses an unsupported column type",
                definition.name
            )));
        }
    }
    Ok(())
}

fn validate_table_tree<D: PageDevice>(
    pager: &mut Pager<D>,
    record: &CatalogTableRecord,
) -> Result<()> {
    let Some(root) = record.root_page_id else {
        return if record.row_count == 0 {
            Ok(())
        } else {
            Err(storage_corrupt(format!(
                "Table `{}` claims {} rows without a tree root",
                record.schema.name, record.row_count
            )))
        };
    };
    let mut count = 0_u64;
    let mut cursor = Btree::cursor(pager, root, record.tree_id)?;
    while let Some((key, value)) = cursor.next(pager)? {
        validated_row(&record.schema, &key, &value)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| storage_corrupt("A table row count overflowed"))?;
    }
    if count != record.row_count {
        return Err(storage_corrupt(format!(
            "Table `{}` catalog count {} does not match its {count} rows",
            record.schema.name, record.row_count
        )));
    }
    Ok(())
}

fn validate_index_tree<D: PageDevice>(
    pager: &mut Pager<D>,
    record: &CatalogIndexRecord,
    tables: &BTreeMap<String, CatalogTableRecord>,
) -> Result<()> {
    let table = tables.get(&record.definition.table).ok_or_else(|| {
        storage_corrupt(format!(
            "Index `{}` references missing table `{}`",
            record.definition.name, record.definition.table
        ))
    })?;
    let expected_entry_count = expected_index_entry_count(pager, table, &record.definition)?;
    if record.entry_count != expected_entry_count {
        return Err(storage_corrupt(format!(
            "Index `{}` catalog count {} does not match its {expected_entry_count} eligible table rows",
            record.definition.name, record.entry_count
        )));
    }
    let Some(root) = record.root_page_id else {
        return Ok(());
    };
    let table_root = table.root_page_id.ok_or_else(|| {
        storage_corrupt(format!(
            "Non-empty index `{}` references an empty table",
            record.definition.name
        ))
    })?;
    let mut count = 0_u64;
    let mut unique_prefixes = HashSet::new();
    let mut cursor = Btree::cursor(pager, root, record.tree_id)?;
    while let Some((entry_key, value)) = cursor.next(pager)? {
        if !value.is_empty() {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` contains a non-empty value",
                record.definition.name
            )));
        }
        let primary_key = secondary_index_primary_key_for_definition(
            &table.schema,
            &record.definition,
            &entry_key,
        )?;
        let row_value =
            Btree::get(pager, table_root, table.tree_id, primary_key)?.ok_or_else(|| {
                storage_corrupt(format!(
                    "Secondary index `{}` contains a dangling primary key",
                    record.definition.name
                ))
            })?;
        let row = validated_row(&table.schema, primary_key, &row_value)?;
        let expected = encode_secondary_index_entry_key(&table.schema, &record.definition, &row)?;
        if expected.as_deref() != Some(entry_key.as_slice()) {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` entry does not match its table row",
                record.definition.name
            )));
        }
        if record.definition.unique {
            let prefix = encode_secondary_index_prefix(&table.schema, &record.definition, &row)?
                .ok_or_else(|| {
                    storage_corrupt(format!(
                        "Unique index `{}` contains a NULL entry",
                        record.definition.name
                    ))
                })?;
            if !unique_prefixes.insert(prefix) {
                return Err(storage_corrupt(format!(
                    "Unique index `{}` contains duplicate values",
                    record.definition.name
                )));
            }
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| storage_corrupt("An index entry count overflowed"))?;
    }
    if count != record.entry_count {
        return Err(storage_corrupt(format!(
            "Index `{}` catalog count {} does not match its {count} entries",
            record.definition.name, record.entry_count
        )));
    }
    Ok(())
}

fn expected_index_entry_count<D: PageDevice>(
    pager: &mut Pager<D>,
    table: &CatalogTableRecord,
    definition: &IndexDefinition,
) -> Result<u64> {
    let Some(root) = table.root_page_id else {
        return Ok(0);
    };
    let mut count = 0_u64;
    let mut cursor = Btree::cursor(pager, root, table.tree_id)?;
    while let Some((key, value)) = cursor.next(pager)? {
        let row = validated_row(&table.schema, &key, &value)?;
        if encode_secondary_index_entry_key(&table.schema, definition, &row)?.is_some() {
            count = count
                .checked_add(1)
                .ok_or_else(|| storage_corrupt("An expected index entry count overflowed"))?;
        }
    }
    Ok(count)
}

fn validated_row(schema: &TableSchema, key: &[u8], value: &[u8]) -> Result<Row> {
    let row = decode_row(value)?;
    let normalized = normalize_row(schema, row.clone()).map_err(|error| {
        storage_corrupt(format!(
            "Stored row in `{}` does not match its schema: {}",
            schema.name, error.message
        ))
    })?;
    if normalized != row {
        return Err(storage_corrupt(format!(
            "Stored row in `{}` is not normalized",
            schema.name
        )));
    }
    if encode_primary_key(schema, &row).map_err(as_storage_corruption)? != key {
        return Err(storage_corrupt(format!(
            "Stored row in `{}` does not match its B-tree key",
            schema.name
        )));
    }
    Ok(row)
}

fn limit_error(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_LIMIT", message)
}

fn storage_corrupt(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_CORRUPT", message)
}

fn as_storage_corruption(error: EngineError) -> EngineError {
    storage_corrupt(error.message)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        ColumnDefinition, ColumnType, Engine, MemoryPageDevice, QueryResult, StorageDriver,
    };

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    fn schema(name: &str, columns: &[(&str, ColumnType, bool)]) -> TableSchema {
        TableSchema {
            name: name.to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: columns
                .iter()
                .map(|(name, data_type, nullable)| ColumnDefinition {
                    name: (*name).to_owned(),
                    data_type: *data_type,
                    nullable: *nullable,
                    default: None,
                })
                .collect(),
        }
    }

    fn source() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(schema(
                "authors",
                &[
                    ("id", ColumnType::Integer, false),
                    ("name", ColumnType::Text, false),
                ],
            ))
            .unwrap();
        storage
            .define_table(schema(
                "posts",
                &[
                    ("id", ColumnType::Integer, false),
                    ("author_id", ColumnType::Integer, false),
                    ("state", ColumnType::Text, true),
                    ("rank", ColumnType::Integer, false),
                ],
            ))
            .unwrap();
        storage
            .replace_table(
                "authors",
                vec![
                    row(json!({"id": 1, "name": "Ada"})),
                    row(json!({"id": 2, "name": "Lin"})),
                ],
            )
            .unwrap();
        storage
            .replace_table(
                "posts",
                vec![
                    row(json!({"id": 10, "author_id": 1, "state": "draft", "rank": 2})),
                    row(json!({"id": 11, "author_id": 1, "state": "live", "rank": 1})),
                    row(json!({"id": 12, "author_id": 2, "state": null, "rank": 3})),
                ],
            )
            .unwrap();
        storage
            .define_index(IndexDefinition {
                name: "posts_state_rank".to_owned(),
                table: "posts".to_owned(),
                columns: vec!["state".to_owned(), "rank".to_owned()],
                unique: false,
            })
            .unwrap();
        storage
            .define_index(IndexDefinition {
                name: "posts_author".to_owned(),
                table: "posts".to_owned(),
                columns: vec!["author_id".to_owned()],
                unique: false,
            })
            .unwrap();
        storage
    }

    fn query_pair(source: &InMemoryStorage, sql: &str) -> (QueryResult, QueryResult) {
        let expected = Engine::new(source.clone()).query_sql(sql, &[]).unwrap();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), source).unwrap();
        let actual = Engine::new(paged).query_sql(sql, &[]).unwrap();
        (expected, actual)
    }

    #[test]
    fn builds_reopens_and_preserves_query_aggregate_and_join_results() {
        let source = source();
        for sql in [
            "SELECT id, state FROM posts ORDER BY id",
            "SELECT state, COUNT(*) AS rows FROM posts GROUP BY state ORDER BY state NULLS LAST",
            "SELECT p.id AS post_id, a.name AS author FROM posts p JOIN authors a ON p.author_id = a.id ORDER BY p.id",
        ] {
            let (expected, actual) = query_pair(&source, sql);
            assert_eq!(actual, expected, "{sql}");
        }

        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let device = paged.into_device();
        let reopened = PagedStorage::open(device).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(reopened.table_row_count("posts").unwrap(), 3);
        assert_eq!(
            Engine::new(reopened)
                .query_sql("SELECT id FROM posts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 10})),
                row(json!({"id": 11})),
                row(json!({"id": 12}))
            ]
        );
    }

    #[test]
    fn primary_and_secondary_lookup_cover_absent_empty_composite_and_null_keys() {
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        assert_eq!(
            paged
                .lookup_primary_key("posts", &row(json!({"id": 11})))
                .unwrap()
                .unwrap()["state"],
            "live"
        );
        assert!(
            paged
                .lookup_primary_key("posts", &row(json!({"id": 99})))
                .unwrap()
                .is_none()
        );
        assert!(
            paged
                .visit_index(
                    "posts",
                    &["missing".to_owned()],
                    &row(json!({"missing": "x"})),
                    &mut |_| Ok(VisitControl::Continue),
                )
                .unwrap()
                .is_none()
        );

        let mut ids = Vec::new();
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["state".to_owned(), "rank".to_owned()],
                    &row(json!({"state": "live", "rank": 1})),
                    &mut |row| {
                        ids.push(row["id"].as_i64().unwrap());
                        Ok(VisitControl::Continue)
                    },
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
        assert_eq!(ids, vec![11]);
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["state".to_owned(), "rank".to_owned()],
                    &row(json!({"state": "missing", "rank": 1})),
                    &mut |_| panic!("an absent key must not visit rows"),
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["state".to_owned(), "rank".to_owned()],
                    &row(json!({"state": null, "rank": 3})),
                    &mut |_| panic!("NULL equality must not visit rows"),
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
    }

    #[test]
    fn visitors_can_stop_without_materializing_the_remaining_table_or_index() {
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let mut table_visits = 0;
        assert_eq!(
            paged
                .visit_table("posts", &mut |_| {
                    table_visits += 1;
                    assert!(
                        paged
                            .lookup_primary_key("authors", &row(json!({"id": 1})))
                            .unwrap()
                            .is_some()
                    );
                    Ok(VisitControl::Stop)
                })
                .unwrap(),
            VisitOutcome::Stopped
        );
        assert_eq!(table_visits, 1);

        let mut index_visits = 0;
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["author_id".to_owned()],
                    &row(json!({"author_id": 1})),
                    &mut |_| {
                        index_visits += 1;
                        Ok(VisitControl::Stop)
                    },
                )
                .unwrap(),
            Some(VisitOutcome::Stopped)
        );
        assert_eq!(index_visits, 1);
    }

    #[test]
    fn validators_reject_nonzero_counts_without_tree_roots() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let table = CatalogTableRecord {
            schema: schema("missing_rows", &[("id", ColumnType::Integer, false)]),
            tree_id: FIRST_USER_TREE_ID,
            root_page_id: None,
            row_count: 1,
        };
        assert_eq!(
            validate_table_tree(&mut pager, &table).unwrap_err().code,
            "STORAGE_CORRUPT"
        );

        let index = CatalogIndexRecord {
            definition: IndexDefinition {
                name: "missing_entries".to_owned(),
                table: "missing_rows".to_owned(),
                columns: vec!["id".to_owned()],
                unique: false,
            },
            tree_id: FIRST_USER_TREE_ID + 1,
            root_page_id: None,
            entry_count: 1,
        };
        assert_eq!(
            validate_index_tree(&mut pager, &index, &BTreeMap::new())
                .unwrap_err()
                .code,
            "STORAGE_CORRUPT"
        );
    }

    #[test]
    fn empty_tables_and_indexes_reopen_without_allocated_roots() {
        let mut source = InMemoryStorage::default();
        source
            .define_table(schema(
                "empty",
                &[
                    ("id", ColumnType::Integer, false),
                    ("label", ColumnType::Text, true),
                ],
            ))
            .unwrap();
        source
            .define_index(IndexDefinition {
                name: "empty_label".to_owned(),
                table: "empty".to_owned(),
                columns: vec!["label".to_owned()],
                unique: false,
            })
            .unwrap();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        assert_eq!(paged.table_row_count("empty").unwrap(), 0);
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.scan_table("empty").unwrap(), Vec::<Row>::new());
        assert_eq!(
            reopened
                .visit_index(
                    "empty",
                    &["label".to_owned()],
                    &row(json!({"label": "none"})),
                    &mut |_| Ok(VisitControl::Continue),
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
    }

    #[test]
    fn reopening_rejects_catalog_count_mismatch() {
        let source = source();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let device = paged.into_device();
        let mut pager = Pager::open_or_create(device).unwrap();
        let root = pager.catalog_root_page_id().unwrap();
        let revision = pager.database_revision();
        let mut transaction = pager.begin_write().unwrap();
        let (key, value) = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: FIRST_USER_TREE_ID + 6,
            table_count: 3,
            index_count: 2,
        })
        .unwrap();
        let root = Btree::upsert(&mut transaction, root, CATALOG_TREE_ID, &key, &value).unwrap();
        transaction.commit(revision, 0, Some(root)).unwrap();
        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("a mismatched catalog count must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn rootless_live_pages_are_neither_an_empty_database_nor_an_import_target() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        Btree::create(&mut transaction, 99).unwrap();
        transaction.commit(0, 0, None).unwrap();
        assert_eq!(pager.active_metadata().superblock.live_data_page_count, 1);
        let device = pager.into_device();

        let error = match PagedStorage::open(device.clone()) {
            Ok(_) => panic!("rootless live pages must not open as an empty database"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");

        let error = match PagedStorage::from_in_memory(device, &source()) {
            Ok(_) => panic!("rootless live pages must not be overwritten by import"),
            Err(error) => error,
        };
        assert_eq!(error.code, "DATABASE_NOT_EMPTY");
    }

    #[test]
    fn reopening_rejects_an_index_missing_eligible_table_rows() {
        let source = source();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let index = paged.indexes.get("posts_author").unwrap().clone();
        let device = paged.into_device();
        let mut pager = Pager::open_or_create(device).unwrap();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let revision = pager.database_revision();
        let mut transaction = pager.begin_write().unwrap();
        let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
            definition: index.definition,
            tree_id: index.tree_id,
            root_page_id: None,
            entry_count: 0,
        })
        .unwrap();
        let catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &key,
            &value,
        )
        .unwrap();
        transaction.commit(revision, 0, Some(catalog_root)).unwrap();

        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("an incomplete secondary index must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }
}
