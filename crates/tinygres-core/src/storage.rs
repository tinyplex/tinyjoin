#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ApplyOutcome, Change, ChangeBatch, ColumnDefinition, ColumnType, EngineError, IndexDefinition,
    Result, Row, TableSchema,
};

const MAX_COLUMNS: usize = 256;
pub(crate) const MAX_JSON_DEPTH: usize = 64;
pub(crate) const MAX_BATCH_CHANGES: usize = 100_000;
pub(crate) const MAX_LOGICAL_VALUE_BYTES: usize = crate::MAX_BTREE_VALUE_BYTES;
pub(crate) const MAX_LOGICAL_ROW_BYTES: usize = crate::MAX_BTREE_VALUE_BYTES - 8;
pub(crate) const MAX_STORAGE_KEY_BYTES: usize = crate::MAX_BTREE_KEY_BYTES;
const MAX_ROW_WRITE_CHANGES: usize = 200_000;
const MAX_ROW_WRITE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisitControl {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisitOutcome {
    Complete,
    Stopped,
}

/// Read-only relational storage used by query planning and execution.
///
/// Keeping this contract independent of mutation lets a page-backed reader
/// stream rows before its copy-on-write write path is complete.
pub trait StorageReader {
    /// Fails when the reader cannot safely answer from its current view.
    ///
    /// In-memory readers are always ready. Durable readers override this after an ambiguous
    /// publication so metadata-only no-op statements cannot accidentally report success.
    #[doc(hidden)]
    fn ensure_readable(&self) -> Result<()> {
        Ok(())
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<VisitOutcome>;
    fn table_row_count(&self, table: &str) -> Result<usize>;
    /// Compatibility collector for operators not yet converted to streaming execution.
    #[doc(hidden)]
    fn scan_table(&self, table: &str) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        self.visit_table(table, &mut |row| {
            rows.push(row.clone());
            Ok(VisitControl::Continue)
        })?;
        Ok(rows)
    }
    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>>;
    fn index_definition(&self, name: &str) -> Option<IndexDefinition>;
    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>>;
    fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>>;
    /// Compatibility collector for callers that need all matching index rows.
    #[doc(hidden)]
    fn lookup_index(&self, table: &str, columns: &[String], key: &Row) -> Result<Option<Vec<Row>>> {
        let mut rows = Vec::new();
        let outcome = self.visit_index(table, columns, key, &mut |row| {
            rows.push(row.clone());
            Ok(VisitControl::Continue)
        })?;
        Ok(outcome.map(|_| rows))
    }
    fn table_schema(&self, table: &str) -> Result<TableSchema>;
    fn revision(&self) -> u64;
}

pub trait StorageDriver: StorageReader {
    fn define_table(&mut self, schema: TableSchema) -> Result<()>;
    fn drop_table(&mut self, table: &str) -> Result<()>;
    fn add_column(&mut self, table: &str, column: ColumnDefinition) -> Result<()>;
    fn replace_table_snapshot(
        &mut self,
        schema: TableSchema,
        rows: Vec<Row>,
    ) -> Result<ApplyOutcome>;
    fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome>;
    fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome>;
    fn define_index(&mut self, definition: IndexDefinition) -> Result<()>;
    fn drop_index(&mut self, name: &str) -> Result<()>;
    /// Atomically applies an already planned row write-set without publishing a revision.
    ///
    /// Callers use this for one SQL statement, then publish exactly one revision only after the
    /// complete statement succeeds. Deletes and upserts are interpreted as one final-state write
    /// set, so primary-key and unique-index swaps do not depend on mutation order.
    #[doc(hidden)]
    fn apply_row_changes_unrevisioned(&mut self, changes: Vec<Change>) -> Result<()>;
    #[doc(hidden)]
    fn replace_table_unrevisioned(&mut self, table: &str, rows: Vec<Row>) -> Result<()>;
    #[doc(hidden)]
    fn advance_revision(&mut self) -> Result<u64>;
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryStorage {
    revision: u64,
    tables: BTreeMap<String, TableData>,
    indexes: BTreeMap<String, IndexData>,
    #[cfg(test)]
    scan_count: Cell<usize>,
    #[cfg(test)]
    lookup_count: Cell<usize>,
    #[cfg(test)]
    visited_row_count: Cell<usize>,
    #[cfg(test)]
    collector_count: Cell<usize>,
}

#[derive(Clone, Debug)]
struct TableData {
    schema: TableSchema,
    rows: BTreeMap<String, Row>,
}

#[derive(Clone, Debug)]
struct IndexData {
    definition: IndexDefinition,
    postings: BTreeMap<String, BTreeSet<String>>,
}

struct PreparedIndexChange {
    index: String,
    primary_key: String,
    old_key: Option<String>,
    new_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StorageSnapshot {
    revision: u64,
    tables: Vec<TableSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    indexes: Vec<IndexDefinition>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TableSnapshot {
    schema: TableSchema,
    rows: Vec<Row>,
}

impl InMemoryStorage {
    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        let snapshot = StorageSnapshot {
            revision: self.revision,
            tables: self
                .tables
                .values()
                .map(|table| TableSnapshot {
                    schema: table.schema.clone(),
                    rows: table.rows.values().cloned().collect(),
                })
                .collect(),
            indexes: self
                .indexes
                .values()
                .map(|index| index.definition.clone())
                .collect(),
        };
        crate::snapshot::encode(&snapshot)
    }

    pub fn import_snapshot(&mut self, bytes: &[u8]) -> Result<()> {
        let replacement = Self::from_snapshot(bytes)?;
        *self = replacement;
        Ok(())
    }

    fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        let snapshot: StorageSnapshot = crate::snapshot::decode(bytes)?;
        let mut tables = BTreeMap::new();

        for table in snapshot.tables {
            validate_schema(&table.schema).map_err(snapshot_validation_error)?;
            let table_name = table.schema.name.clone();
            let mut rows = BTreeMap::new();

            for row in table.rows {
                let row = normalize_row(&table.schema, row).map_err(snapshot_validation_error)?;
                let key = row_key(&table.schema, &row).map_err(snapshot_validation_error)?;
                if rows.insert(key, row).is_some() {
                    return Err(EngineError::invalid_snapshot(format!(
                        "Table `{table_name}` contains a duplicate primary key"
                    )));
                }
            }

            if tables
                .insert(
                    table_name.clone(),
                    TableData {
                        schema: table.schema,
                        rows,
                    },
                )
                .is_some()
            {
                return Err(EngineError::invalid_snapshot(format!(
                    "Snapshot contains table `{table_name}` more than once"
                )));
            }
        }

        let mut storage = Self {
            revision: snapshot.revision,
            tables,
            indexes: BTreeMap::new(),
            #[cfg(test)]
            scan_count: Cell::new(0),
            #[cfg(test)]
            lookup_count: Cell::new(0),
            #[cfg(test)]
            visited_row_count: Cell::new(0),
            #[cfg(test)]
            collector_count: Cell::new(0),
        };
        for definition in snapshot.indexes {
            storage
                .define_index(definition)
                .map_err(snapshot_validation_error)?;
        }
        Ok(storage)
    }

    #[cfg(test)]
    pub(crate) fn access_counts(&self) -> (usize, usize) {
        (self.scan_count.get(), self.lookup_count.get())
    }

    #[cfg(test)]
    pub(crate) fn visitor_counts(&self) -> (usize, usize) {
        (self.visited_row_count.get(), self.collector_count.get())
    }

    fn rebuilt_indexes_for_table(
        &self,
        table: &str,
        rows: &BTreeMap<String, Row>,
    ) -> Result<BTreeMap<String, IndexData>> {
        let mut indexes = self.indexes.clone();
        let schema = &self
            .tables
            .get(table)
            .expect("the rebuilt index table exists")
            .schema;
        for index in indexes.values_mut() {
            if index.definition.table == table {
                index.postings = build_postings(schema, &index.definition, rows)?;
            }
        }
        Ok(indexes)
    }

    pub(crate) fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }

    pub(crate) fn table_rows(&self, table: &str) -> Result<&BTreeMap<String, Row>> {
        self.tables
            .get(table)
            .map(|table| &table.rows)
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    /// Borrows a schema so the paged importer can enforce aggregate metadata bounds before clone.
    pub(crate) fn table_schema_ref(&self, table: &str) -> Result<&TableSchema> {
        self.tables
            .get(table)
            .map(|table| &table.schema)
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    pub(crate) fn index_names(&self) -> impl Iterator<Item = &str> {
        self.indexes.keys().map(String::as_str)
    }

    /// Borrows an index definition for bounded paged-import planning.
    pub(crate) fn index_definition_ref(&self, name: &str) -> Option<&IndexDefinition> {
        self.indexes.get(name).map(|index| &index.definition)
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

impl StorageDriver for InMemoryStorage {
    fn define_table(&mut self, schema: TableSchema) -> Result<()> {
        validate_schema(&schema)?;

        if let Some(existing) = self.tables.get(&schema.name) {
            if existing.schema == schema {
                return Ok(());
            }
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` is already defined with a different schema",
                schema.name
            )));
        }

        self.tables.insert(
            schema.name.clone(),
            TableData {
                schema,
                rows: BTreeMap::new(),
            },
        );
        Ok(())
    }

    fn drop_table(&mut self, table: &str) -> Result<()> {
        if self.tables.remove(table).is_none() {
            return Err(EngineError::table_not_found(table));
        }
        self.indexes
            .retain(|_, index| index.definition.table != table);
        Ok(())
    }

    fn add_column(&mut self, table: &str, column: ColumnDefinition) -> Result<()> {
        let current = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let schema = schema_with_added_column(&current.schema, &column)?;
        let value = column.default.clone().unwrap_or(Value::Null);
        let mut rows = BTreeMap::new();
        for (key, row) in &current.rows {
            let mut row = row.clone();
            row.insert(column.name.clone(), value.clone());
            let row = normalize_row(&schema, row)?;
            rows.insert(key.clone(), row);
        }

        let target = self
            .tables
            .get_mut(table)
            .expect("the altered table was resolved above");
        target.schema = schema;
        target.rows = rows;
        Ok(())
    }

    fn replace_table_snapshot(
        &mut self,
        schema: TableSchema,
        rows: Vec<Row>,
    ) -> Result<ApplyOutcome> {
        let table = schema.name.clone();
        let mut candidate = self.clone();
        candidate.define_table(schema)?;
        let outcome = candidate.replace_table(&table, rows)?;
        *self = candidate;
        Ok(outcome)
    }

    fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome> {
        self.replace_table_unrevisioned(table, rows)?;
        self.advance_revision()?;

        Ok(ApplyOutcome {
            revision: self.revision,
            tables: vec![table.to_owned()],
        })
    }

    fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome> {
        let schemas = self
            .tables
            .iter()
            .map(|(name, table)| (name.as_str(), &table.schema))
            .collect();
        let indexes = self
            .indexes
            .values()
            .map(|index| &index.definition)
            .collect::<Vec<_>>();
        preflight_change_batch(&batch.changes, &schemas, &indexes)?;
        if batch.changes.is_empty() {
            return Ok(ApplyOutcome {
                revision: self.revision,
                tables: vec![],
            });
        }

        // Apply to a clone first so a malformed later change cannot leave a partial batch.
        let mut candidate = self.clone();
        candidate.apply_row_changes_unrevisioned(batch.changes.clone())?;
        let changed_tables = batch
            .changes
            .iter()
            .map(|change| match change {
                Change::Upsert { table, .. } | Change::Delete { table, .. } => table.clone(),
            })
            .collect::<BTreeSet<_>>();

        candidate.revision = next_revision(self.revision)?;
        let outcome = ApplyOutcome {
            revision: candidate.revision,
            tables: changed_tables.into_iter().collect(),
        };
        *self = candidate;
        Ok(outcome)
    }

    fn define_index(&mut self, definition: IndexDefinition) -> Result<()> {
        validate_index_definition(&definition, &self.tables)?;
        if self.indexes.contains_key(&definition.name) {
            return Err(EngineError::index_already_exists(&definition.name));
        }
        let rows = &self
            .tables
            .get(&definition.table)
            .expect("index validation resolved the table")
            .rows;
        let schema = &self
            .tables
            .get(&definition.table)
            .expect("index validation resolved the table")
            .schema;
        let postings = build_postings(schema, &definition, rows)?;
        self.indexes.insert(
            definition.name.clone(),
            IndexData {
                definition,
                postings,
            },
        );
        Ok(())
    }

    fn drop_index(&mut self, name: &str) -> Result<()> {
        if self.indexes.remove(name).is_none() {
            return Err(EngineError::new(
                "INDEX_NOT_FOUND",
                format!("Index `{name}` is not defined"),
            ));
        }
        Ok(())
    }

    fn apply_row_changes_unrevisioned(&mut self, changes: Vec<Change>) -> Result<()> {
        if changes.len() > MAX_ROW_WRITE_CHANGES {
            return Err(row_write_limit_error(format!(
                "A row write-set cannot contain more than {MAX_ROW_WRITE_CHANGES} changes"
            )));
        }
        if changes.is_empty() {
            return Ok(());
        }

        // Collapse the input into its final value for each primary key before touching live
        // state. SQL UPDATE emits all moved-key deletes before its upserts; treating the whole
        // collection as a final write-set makes key and unique-index swaps order independent.
        let mut retained_bytes = 0usize;
        let mut tables = BTreeMap::<String, BTreeMap<String, Option<Row>>>::new();
        for change in changes {
            let (table, key, next) = match change {
                Change::Upsert { table, row } => {
                    let schema = &self
                        .tables
                        .get(&table)
                        .ok_or_else(|| EngineError::table_not_found(&table))?
                        .schema;
                    let row = normalize_row(schema, row)?;
                    let key = row_key(schema, &row)?;
                    (table, key, Some(row))
                }
                Change::Delete { table, key } => {
                    let schema = &self
                        .tables
                        .get(&table)
                        .ok_or_else(|| EngineError::table_not_found(&table))?
                        .schema;
                    validate_primary_key_values(schema, &key)?;
                    let key = row_key(schema, &key)?;
                    (table, key, None)
                }
            };

            let next_bytes = next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
            let charge = key
                .len()
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(next_bytes))
                .and_then(|bytes| bytes.checked_add(96))
                .ok_or_else(row_write_overflow_error)?;
            let table_changes = tables.entry(table).or_default();
            if let Some(previous) = table_changes.insert(key.clone(), next) {
                let previous_bytes = previous.as_ref().map_or(Ok(0), estimated_row_bytes)?;
                let previous_charge = key
                    .len()
                    .checked_mul(2)
                    .and_then(|bytes| bytes.checked_add(previous_bytes))
                    .and_then(|bytes| bytes.checked_add(96))
                    .ok_or_else(row_write_overflow_error)?;
                retained_bytes = retained_bytes
                    .checked_sub(previous_charge)
                    .ok_or_else(row_write_overflow_error)?;
            }
            retained_bytes = retained_bytes
                .checked_add(charge)
                .ok_or_else(row_write_overflow_error)?;
            ensure_row_write_bytes(retained_bytes)?;
        }

        // Precompute every old and new index entry. All validation and allocation that can fail
        // happens before the first live row or posting is changed.
        let mut index_changes = Vec::new();
        for (table_name, row_changes) in &tables {
            let table = self
                .tables
                .get(table_name)
                .expect("every changed table was resolved above");
            for index in self
                .indexes
                .values()
                .filter(|index| index.definition.table == *table_name)
            {
                let mut incoming = BTreeMap::<String, BTreeSet<String>>::new();
                for (primary_key, next) in row_changes {
                    let old_key = table
                        .rows
                        .get(primary_key)
                        .map(|row| index_key_from_stored_row(&table.schema, &index.definition, row))
                        .transpose()?
                        .flatten();
                    let new_key = next
                        .as_ref()
                        .map(|row| index_key(&table.schema, &index.definition, row))
                        .transpose()?
                        .flatten();

                    let old_key_bytes = old_key
                        .as_ref()
                        .map_or(Ok(0), |key| checked_row_write_mul(key.len(), 2))?;
                    let new_key_bytes = new_key
                        .as_ref()
                        .map_or(Ok(0), |key| checked_row_write_mul(key.len(), 2))?;
                    let charge = checked_row_write_add(
                        checked_row_write_add(
                            checked_row_write_mul(primary_key.len(), 2)?,
                            old_key_bytes,
                        )?,
                        checked_row_write_add(
                            new_key_bytes,
                            checked_row_write_add(index.definition.name.len(), 128)?,
                        )?,
                    )?;
                    retained_bytes = retained_bytes
                        .checked_add(charge)
                        .ok_or_else(row_write_overflow_error)?;
                    ensure_row_write_bytes(retained_bytes)?;

                    if let Some(new_key) = &new_key {
                        incoming
                            .entry(new_key.clone())
                            .or_default()
                            .insert(primary_key.clone());
                    }
                    index_changes.push(PreparedIndexChange {
                        index: index.definition.name.clone(),
                        primary_key: primary_key.clone(),
                        old_key,
                        new_key,
                    });
                }

                if index.definition.unique {
                    for (key, new_primary_keys) in incoming {
                        if new_primary_keys.len() > 1 {
                            return Err(unique_index_violation(&index.definition.name));
                        }
                        let has_unchanged_entry =
                            index.postings.get(&key).is_some_and(|existing| {
                                existing
                                    .iter()
                                    .any(|primary_key| !row_changes.contains_key(primary_key))
                            });
                        if has_unchanged_entry {
                            return Err(unique_index_violation(&index.definition.name));
                        }
                    }
                }
            }
        }

        // From here onward every operation is infallible. Remove stale postings first so swaps
        // are applied as one final state, then publish rows and their replacement postings.
        for change in &index_changes {
            let Some(old_key) = &change.old_key else {
                continue;
            };
            let index = self
                .indexes
                .get_mut(&change.index)
                .expect("the prepared index still exists");
            if let Some(postings) = index.postings.get_mut(old_key) {
                postings.remove(&change.primary_key);
                if postings.is_empty() {
                    index.postings.remove(old_key);
                }
            }
        }
        for (table_name, row_changes) in tables {
            let rows = &mut self
                .tables
                .get_mut(&table_name)
                .expect("every changed table still exists")
                .rows;
            for (primary_key, next) in row_changes {
                match next {
                    Some(row) => {
                        rows.insert(primary_key, row);
                    }
                    None => {
                        rows.remove(&primary_key);
                    }
                }
            }
        }
        for change in index_changes {
            let Some(new_key) = change.new_key else {
                continue;
            };
            self.indexes
                .get_mut(&change.index)
                .expect("the prepared index still exists")
                .postings
                .entry(new_key)
                .or_default()
                .insert(change.primary_key);
        }
        Ok(())
    }

    fn replace_table_unrevisioned(&mut self, table: &str, rows: Vec<Row>) -> Result<()> {
        let current = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let mut replacement = BTreeMap::new();

        for row in rows {
            let row = normalize_row(&current.schema, row)?;
            let key = row_key(&current.schema, &row)?;
            if replacement.insert(key, row).is_some() {
                return Err(EngineError::invalid_change(format!(
                    "Snapshot for `{table}` contains a duplicate primary key"
                )));
            }
        }

        let indexes = self.rebuilt_indexes_for_table(table, &replacement)?;
        self.tables
            .get_mut(table)
            .expect("table was checked above")
            .rows = replacement;
        self.indexes = indexes;
        Ok(())
    }

    fn advance_revision(&mut self) -> Result<u64> {
        self.revision = next_revision(self.revision)?;
        Ok(self.revision)
    }
}

/// Builds and validates the exact schema produced by `ALTER TABLE ADD COLUMN`.
///
/// Statement planning and every storage backend share this borrowed-schema helper so typed-catalog,
/// column-count, duplicate-name, default, and type errors retain one deterministic contract.
pub(crate) fn schema_with_added_column(
    current: &TableSchema,
    column: &ColumnDefinition,
) -> Result<TableSchema> {
    validate_added_column(current, column)?;
    let mut schema = current.clone();
    schema.columns.push(column.clone());
    // Keep the complete schema validator as a defense if either helper evolves independently.
    validate_schema(&schema)?;
    Ok(schema)
}

/// Validates an appended column without retaining a prospective schema clone.
///
/// The paged publisher uses this narrow seam to preserve logical error order before applying its
/// physical work limits; [`schema_with_added_column`] remains the owned-schema construction path.
pub(crate) fn validate_added_column(
    current: &TableSchema,
    column: &ColumnDefinition,
) -> Result<()> {
    let table = &current.name;
    if current.columns.is_empty() {
        return Err(EngineError::unsupported_sql(format!(
            "ALTER TABLE ADD COLUMN requires a typed table catalog for `{table}`"
        )));
    }
    if current.columns.len() >= MAX_COLUMNS {
        return Err(EngineError::invalid_schema(format!(
            "Table `{table}` cannot contain more than {MAX_COLUMNS} columns"
        )));
    }
    if current
        .columns
        .iter()
        .any(|existing| existing.name == column.name)
    {
        return Err(EngineError::column_already_exists(&column.name, table));
    }
    // Existing committed schemas have already passed this validator, but retaining the check keeps
    // this shared helper deterministic for direct storage callers and corrupted test fixtures.
    validate_schema(current)?;
    validate_catalog_name_bound(&column.name)
        .map_err(|error| EngineError::invalid_schema(error.message))?;
    if column.name.trim().is_empty() {
        return Err(EngineError::invalid_schema(format!(
            "Table `{table}` contains an empty column name"
        )));
    }
    if let Some(default) = &column.default {
        validate_json_value(default).map_err(|error| {
            EngineError::invalid_schema(format!(
                "Default for column `{}` is invalid: {}",
                column.name, error.message
            ))
        })?;
        validate_value(column, default, table).map_err(|error| {
            EngineError::invalid_schema(format!(
                "Default for column `{}` is invalid: {}",
                column.name, error.message
            ))
        })?;
    }
    Ok(())
}

impl StorageReader for InMemoryStorage {
    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        #[cfg(test)]
        self.scan_count.set(self.scan_count.get() + 1);
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        for row in table.rows.values() {
            #[cfg(test)]
            self.visited_row_count.set(self.visited_row_count.get() + 1);
            if visitor(row)? == VisitControl::Stop {
                return Ok(VisitOutcome::Stopped);
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn table_row_count(&self, table: &str) -> Result<usize> {
        self.tables
            .get(table)
            .map(|table| table.rows.len())
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    #[cfg(test)]
    fn scan_table(&self, table: &str) -> Result<Vec<Row>> {
        self.collector_count.set(self.collector_count.get() + 1);
        let mut rows = Vec::new();
        self.visit_table(table, &mut |row| {
            rows.push(row.clone());
            Ok(VisitControl::Continue)
        })?;
        Ok(rows)
    }

    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        #[cfg(test)]
        self.lookup_count.set(self.lookup_count.get() + 1);
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let key = row_key(&table.schema, key)?;
        Ok(table.rows.get(&key).cloned())
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
        #[cfg(test)]
        self.lookup_count.set(self.lookup_count.get() + 1);
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
        let Some(index_key) = index_lookup_key(&table_data.schema, &index.definition, key)? else {
            return Ok(Some(VisitOutcome::Complete));
        };
        for row in index
            .postings
            .get(&index_key)
            .into_iter()
            .flatten()
            .filter_map(|primary_key| table_data.rows.get(primary_key))
        {
            #[cfg(test)]
            self.visited_row_count.set(self.visited_row_count.get() + 1);
            if visitor(row)? == VisitControl::Stop {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    #[cfg(test)]
    fn lookup_index(&self, table: &str, columns: &[String], key: &Row) -> Result<Option<Vec<Row>>> {
        self.collector_count.set(self.collector_count.get() + 1);
        let mut rows = Vec::new();
        let outcome = self.visit_index(table, columns, key, &mut |row| {
            rows.push(row.clone());
            Ok(VisitControl::Continue)
        })?;
        Ok(outcome.map(|_| rows))
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

fn validate_index_definition(
    definition: &IndexDefinition,
    tables: &BTreeMap<String, TableData>,
) -> Result<()> {
    validate_index_definition_shape(definition)?;
    let table = tables
        .get(&definition.table)
        .ok_or_else(|| EngineError::table_not_found(&definition.table))?;
    validate_index_columns_for_schema(definition, &table.schema)
}

pub(crate) fn validate_index_definition_shape(definition: &IndexDefinition) -> Result<()> {
    validate_catalog_name_bound(&definition.name)
        .map_err(|error| EngineError::invalid_schema(error.message))?;
    validate_catalog_name_bound(&definition.table)
        .map_err(|error| EngineError::invalid_schema(error.message))?;
    if definition.name.trim().is_empty() {
        return Err(EngineError::invalid_schema("An index name cannot be empty"));
    }
    if definition.columns.is_empty() {
        return Err(EngineError::invalid_schema(format!(
            "Index `{}` must contain at least one column",
            definition.name
        )));
    }
    Ok(())
}

pub(crate) fn validate_index_columns_for_schema(
    definition: &IndexDefinition,
    schema: &TableSchema,
) -> Result<()> {
    debug_assert_eq!(definition.table, schema.name);
    if schema.columns.is_empty() {
        return Err(EngineError::unsupported_sql(format!(
            "Index `{}` requires a typed table catalog",
            definition.name
        )));
    }
    let mut seen = HashSet::new();
    for name in &definition.columns {
        validate_catalog_name_bound(name)
            .map_err(|error| EngineError::invalid_schema(error.message))?;
        if !seen.insert(name) {
            return Err(EngineError::invalid_schema(format!(
                "Index `{}` names column `{name}` more than once",
                definition.name
            )));
        }
        let column = schema
            .columns
            .iter()
            .find(|column| column.name == *name)
            .ok_or_else(|| EngineError::column_not_found(name, &definition.table))?;
        if matches!(column.data_type, ColumnType::Float | ColumnType::Json) {
            return Err(EngineError::unsupported_sql(format!(
                "Index `{}` cannot use {} column `{name}`; only boolean, integer, and text columns are supported",
                definition.name,
                column_type_name(column.data_type)
            )));
        }
    }
    Ok(())
}

fn build_postings(
    schema: &TableSchema,
    definition: &IndexDefinition,
    rows: &BTreeMap<String, Row>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut postings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (primary_key, row) in rows {
        // PostgreSQL's default UNIQUE semantics treat every key containing NULL
        // as distinct. NULL comparisons cannot use an equality lookup either.
        let Some(key) = validated_index_key(schema, definition, row)? else {
            continue;
        };
        let entries = postings.entry(key).or_default();
        if definition.unique && !entries.is_empty() {
            return Err(EngineError::constraint_violation(format!(
                "Index `{}` would contain duplicate values",
                definition.name
            )));
        }
        entries.insert(primary_key.clone());
    }
    Ok(postings)
}

fn index_key(
    schema: &TableSchema,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<String>> {
    validated_index_key(schema, definition, row)
}

pub(crate) fn validated_index_key(
    schema: &TableSchema,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<String>> {
    validate_secondary_storage_key_bound(schema, definition, row)?;
    index_key_from_stored_row(schema, definition, row)
}

fn index_key_from_stored_row(
    _schema: &TableSchema,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<String>> {
    let mut values = Vec::with_capacity(definition.columns.len());
    for column in &definition.columns {
        let value = row.get(column).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing indexed column `{column}`",
                definition.table
            ))
        })?;
        if value == &Value::Null {
            return Ok(None);
        }
        values.push(value);
    }
    serde_json::to_string(&values).map(Some).map_err(|error| {
        EngineError::invalid_change(format!("Could not encode index key: {error}"))
    })
}

fn index_lookup_key(
    schema: &TableSchema,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<String>> {
    let Some(bytes) = storage_tuple_bytes(schema, &definition.columns, row, true)? else {
        return Ok(None);
    };
    ensure_storage_key_bytes(checked_row_write_add(bytes, 1)?)?;
    index_key_from_stored_row(schema, definition, row)
}

pub(crate) fn validate_schema(schema: &TableSchema) -> Result<()> {
    validate_catalog_name_bound(&schema.name)
        .map_err(|error| EngineError::invalid_schema(error.message))?;
    if schema.name.trim().is_empty() {
        return Err(EngineError::invalid_schema("A table name cannot be empty"));
    }
    if schema.primary_key.is_empty() {
        return Err(EngineError::invalid_schema(format!(
            "Table `{}` must declare at least one primary-key column",
            schema.name
        )));
    }

    let mut columns = HashSet::new();
    for column in &schema.primary_key {
        validate_catalog_name_bound(column)
            .map_err(|error| EngineError::invalid_schema(error.message))?;
        if column.trim().is_empty() {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` contains an empty primary-key column",
                schema.name
            )));
        }
        if !columns.insert(column) {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` declares primary-key column `{column}` more than once",
                schema.name
            )));
        }
    }

    if schema.columns.is_empty() {
        return Ok(());
    }

    let mut catalog_columns = HashSet::new();
    for column in &schema.columns {
        validate_catalog_name_bound(&column.name)
            .map_err(|error| EngineError::invalid_schema(error.message))?;
        if column.name.trim().is_empty() {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` contains an empty column name",
                schema.name
            )));
        }
        if !catalog_columns.insert(column.name.as_str()) {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` declares column `{}` more than once",
                schema.name, column.name
            )));
        }
        if schema.primary_key.contains(&column.name) && column.nullable {
            return Err(EngineError::invalid_schema(format!(
                "Primary-key column `{}` in `{}` cannot be nullable",
                column.name, schema.name
            )));
        }
        if schema.primary_key.contains(&column.name) && column.data_type == ColumnType::Json {
            return Err(EngineError::invalid_schema(format!(
                "Primary-key column `{}` in `{}` cannot use JSON page storage keys",
                column.name, schema.name
            )));
        }
        if let Some(default) = &column.default {
            validate_json_value(default).map_err(|error| {
                EngineError::invalid_schema(format!(
                    "Default for column `{}` is invalid: {}",
                    column.name, error.message
                ))
            })?;
            validate_value(column, default, &schema.name).map_err(|error| {
                EngineError::invalid_schema(format!(
                    "Default for column `{}` is invalid: {}",
                    column.name, error.message
                ))
            })?;
        }
    }

    for column in &schema.primary_key {
        if !catalog_columns.contains(column.as_str()) {
            return Err(EngineError::invalid_schema(format!(
                "Primary-key column `{column}` is not declared in table `{}`",
                schema.name
            )));
        }
    }
    Ok(())
}

pub(crate) fn normalize_row(schema: &TableSchema, mut row: Row) -> Result<Row> {
    validate_row_value_limits(&row).map_err(|error| EngineError::invalid_change(error.message))?;
    if schema.columns.is_empty() {
        return Ok(row);
    }

    for name in row.keys() {
        if !schema.columns.iter().any(|column| column.name == *name) {
            return Err(EngineError::column_not_found(name, &schema.name));
        }
    }

    for column in &schema.columns {
        if !row.contains_key(&column.name) {
            let value = column.default.clone().unwrap_or(Value::Null);
            row.insert(column.name.clone(), value);
        }
        validate_value(
            column,
            row.get(&column.name)
                .expect("the catalog column was populated above"),
            &schema.name,
        )?;
    }
    validate_row_value_limits(&row).map_err(|error| EngineError::invalid_change(error.message))?;
    Ok(row)
}

/// Returns the canonical JSON body length already enforced by row normalization, without
/// allocating an encoded copy.
pub(crate) fn logical_row_encoded_bytes(row: &Row) -> Result<usize> {
    validate_row_value_limits(row)
}

/// Conservatively estimates the owned serde model produced from canonical JSON without parsing or
/// allocating that model. This is intentionally structural: compact arrays such as `[0,0,...]`
/// can expand far beyond any constant multiple suitable for string-heavy inputs. Every primitive
/// contributes at least 48 bytes. The ALTER caller retains twice this estimate, so an array of `n`
/// primitives is charged at least `96n`: enough for serde's geometrically grown parsed Vec (at
/// most `2n` 32-byte `Value` slots) plus the exact-`n` normalized clone. Strings, keys, nested
/// containers, and encoded buffers are charged separately on top.
pub(crate) fn estimated_encoded_json_model_bytes(bytes: &[u8]) -> Result<usize> {
    let mut work = 64usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'[' | b'{' => {
                work = checked_row_write_add(work, 64)?;
                index += 1;
            }
            b']' | b'}' | b',' | b':' | b' ' | b'\n' | b'\r' | b'\t' => index += 1,
            b'"' => {
                index += 1;
                let start = index;
                let mut escaped = false;
                while index < bytes.len() {
                    match bytes[index] {
                        b'"' if !escaped => break,
                        b'\\' if !escaped => escaped = true,
                        _ => escaped = false,
                    }
                    index += 1;
                }
                if index == bytes.len() {
                    return Err(EngineError::invalid_change(
                        "Encoded JSON contains an unterminated string",
                    ));
                }
                let encoded = index - start;
                work = checked_row_write_add(work, 64)?;
                work = checked_row_write_add(work, checked_row_write_mul(encoded, 2)?)?;
                index += 1;
            }
            byte if byte.is_ascii_digit() || byte == b'-' => {
                work = checked_row_write_add(work, 48)?;
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    index += 1;
                }
            }
            b't' if bytes[index..].starts_with(b"true") => {
                // Includes the array/map slot which owns this primitive. Vec growth may reserve
                // nearly twice its logical length, and callers can retain a normalized clone.
                work = checked_row_write_add(work, 48)?;
                index += 4;
            }
            b'f' if bytes[index..].starts_with(b"false") => {
                work = checked_row_write_add(work, 48)?;
                index += 5;
            }
            b'n' if bytes[index..].starts_with(b"null") => {
                work = checked_row_write_add(work, 48)?;
                index += 4;
            }
            _ => {
                return Err(EngineError::invalid_change(
                    "Encoded JSON contains an invalid token",
                ));
            }
        }
    }
    Ok(work)
}

fn validate_value(column: &ColumnDefinition, value: &Value, table: &str) -> Result<()> {
    if value == &Value::Null {
        if column.nullable {
            return Ok(());
        }
        return Err(EngineError::constraint_violation(format!(
            "Column `{}` in `{table}` cannot be null",
            column.name
        )));
    }

    let valid = match column.data_type {
        ColumnType::Boolean => value.is_boolean(),
        ColumnType::Integer => is_javascript_safe_integer(value),
        ColumnType::Float => value.is_number(),
        ColumnType::Text => value.is_string(),
        ColumnType::Json => {
            estimated_value_bytes(value).map_err(|error| {
                EngineError::invalid_change(format!(
                    "Column `{}` in `{table}` contains invalid JSON: {}",
                    column.name, error.message
                ))
            })?;
            true
        }
    };
    if valid {
        Ok(())
    } else {
        Err(EngineError::type_mismatch(format!(
            "Column `{}` in `{table}` expects {}",
            column.name,
            column_type_name(column.data_type)
        )))
    }
}

fn is_javascript_safe_integer(value: &Value) -> bool {
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    value
        .as_u64()
        .is_some_and(|number| number <= MAX_SAFE_INTEGER)
        || value.as_i64().is_some_and(|number| {
            number >= -(MAX_SAFE_INTEGER as i64) && number <= MAX_SAFE_INTEGER as i64
        })
}

fn column_type_name(data_type: ColumnType) -> &'static str {
    match data_type {
        ColumnType::Boolean => "boolean",
        ColumnType::Integer => "integer",
        ColumnType::Float => "float",
        ColumnType::Text => "text",
        ColumnType::Json => "json",
    }
}

pub(crate) fn row_key(schema: &TableSchema, row: &Row) -> Result<String> {
    validate_primary_storage_key_bound(schema, row)?;
    let mut values = Vec::with_capacity(schema.primary_key.len());
    for column in &schema.primary_key {
        let value = row.get(column).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing primary-key column `{column}`",
                schema.name
            ))
        })?;
        if value == &Value::Null {
            return Err(EngineError::invalid_change(format!(
                "Primary-key column `{column}` in `{}` cannot be null",
                schema.name
            )));
        }
        values.push(value);
    }
    serde_json::to_string(&values).map_err(|error| {
        EngineError::invalid_change(format!("Could not encode primary key: {error}"))
    })
}

fn validate_primary_key_values(schema: &TableSchema, row: &Row) -> Result<()> {
    if schema.columns.is_empty() {
        return Ok(());
    }
    for name in &schema.primary_key {
        let value = row.get(name).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing primary-key column `{name}`",
                schema.name
            ))
        })?;
        let column = schema
            .columns
            .iter()
            .find(|column| column.name == *name)
            .expect("schema validation requires every primary-key column in the catalog");
        validate_value(column, value, &schema.name)?;
    }
    Ok(())
}

pub(crate) fn estimated_row_bytes(row: &Row) -> Result<usize> {
    validate_row_value_limits(row)?;
    let mut bytes = 32usize;
    for (key, value) in row {
        bytes = checked_row_write_add(bytes, 64)?;
        bytes = checked_row_write_add(bytes, checked_row_write_mul(key.len(), 2)?)?;
        bytes = checked_row_write_add(
            bytes,
            checked_row_write_mul(estimated_value_bytes(value)?, 2)?,
        )?;
    }
    Ok(bytes)
}

pub(crate) fn estimated_value_bytes(value: &Value) -> Result<usize> {
    validate_json_value(value)?;
    estimated_value_bytes_at_depth(value, 0)
}

fn estimated_value_bytes_at_depth(value: &Value, depth: usize) -> Result<usize> {
    debug_assert!(depth <= MAX_JSON_DEPTH);
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(16),
        Value::String(value) => checked_row_write_add(24, value.len()),
        Value::Array(values) => {
            let mut bytes = 32usize;
            for value in values {
                bytes = checked_row_write_add(bytes, 32)?;
                bytes = checked_row_write_add(
                    bytes,
                    estimated_value_bytes_at_depth(value, depth + 1)?,
                )?;
            }
            Ok(bytes)
        }
        Value::Object(values) => {
            let mut bytes = 32usize;
            for (key, value) in values {
                bytes = checked_row_write_add(bytes, 64)?;
                bytes = checked_row_write_add(bytes, checked_row_write_mul(key.len(), 2)?)?;
                bytes = checked_row_write_add(
                    bytes,
                    estimated_value_bytes_at_depth(value, depth + 1)?,
                )?;
            }
            Ok(bytes)
        }
    }
}

pub(crate) fn validate_json_value(value: &Value) -> Result<usize> {
    let bytes = encoded_json_bytes(value, 0)?;
    if bytes > MAX_LOGICAL_VALUE_BYTES {
        Err(row_write_limit_error(format!(
            "A JSON value cannot exceed {MAX_LOGICAL_VALUE_BYTES} encoded bytes"
        )))
    } else {
        Ok(bytes)
    }
}

fn validate_row_value_limits(row: &Row) -> Result<usize> {
    let mut bytes = 2usize;
    for (index, (key, value)) in row.iter().enumerate() {
        if index != 0 {
            bytes = checked_row_write_add(bytes, 1)?;
        }
        bytes = checked_row_write_add(bytes, encoded_json_string_bytes(key)?)?;
        bytes = checked_row_write_add(bytes, 1)?;
        bytes = checked_row_write_add(bytes, encoded_json_bytes(value, 1)?)?;
        if bytes > MAX_LOGICAL_ROW_BYTES {
            return Err(row_write_limit_error(format!(
                "A row cannot exceed {MAX_LOGICAL_ROW_BYTES} encoded bytes"
            )));
        }
    }
    Ok(bytes)
}

fn encoded_json_bytes(value: &Value, depth: usize) -> Result<usize> {
    if depth > MAX_JSON_DEPTH {
        return Err(row_write_limit_error(format!(
            "JSON cannot nest more than {MAX_JSON_DEPTH} levels"
        )));
    }
    match value {
        Value::Null => Ok(4),
        Value::Bool(false) => Ok(5),
        Value::Bool(true) => Ok(4),
        Value::Number(number) => Ok(number.to_string().len()),
        Value::String(value) => encoded_json_string_bytes(value),
        Value::Array(values) => {
            let mut bytes = 2usize;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    bytes = checked_row_write_add(bytes, 1)?;
                }
                bytes = checked_row_write_add(bytes, encoded_json_bytes(value, depth + 1)?)?;
                if bytes > MAX_LOGICAL_VALUE_BYTES {
                    return Err(row_write_limit_error(format!(
                        "A JSON value cannot exceed {MAX_LOGICAL_VALUE_BYTES} encoded bytes"
                    )));
                }
            }
            Ok(bytes)
        }
        Value::Object(values) => {
            let mut bytes = 2usize;
            for (index, (key, value)) in values.iter().enumerate() {
                if index != 0 {
                    bytes = checked_row_write_add(bytes, 1)?;
                }
                bytes = checked_row_write_add(bytes, encoded_json_string_bytes(key)?)?;
                bytes = checked_row_write_add(bytes, 1)?;
                bytes = checked_row_write_add(bytes, encoded_json_bytes(value, depth + 1)?)?;
                if bytes > MAX_LOGICAL_VALUE_BYTES {
                    return Err(row_write_limit_error(format!(
                        "A JSON value cannot exceed {MAX_LOGICAL_VALUE_BYTES} encoded bytes"
                    )));
                }
            }
            Ok(bytes)
        }
    }
}

fn encoded_json_string_bytes(value: &str) -> Result<usize> {
    value.chars().try_fold(2usize, |bytes, character| {
        let escaped = match character {
            '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            character => character.len_utf8(),
        };
        checked_row_write_add(bytes, escaped)
    })
}

pub(crate) fn validate_primary_storage_key_bound(schema: &TableSchema, row: &Row) -> Result<()> {
    let bytes = storage_tuple_bytes(schema, &schema.primary_key, row, false)?
        .ok_or_else(|| EngineError::invalid_change("A primary key cannot contain null"))?;
    ensure_storage_key_bytes(bytes)
}

pub(crate) fn validate_secondary_storage_key_bound(
    schema: &TableSchema,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<()> {
    let Some(index_bytes) = storage_tuple_bytes(schema, &definition.columns, row, true)? else {
        return Ok(());
    };
    let primary_bytes = storage_tuple_bytes(schema, &schema.primary_key, row, false)?
        .ok_or_else(|| EngineError::invalid_change("A primary key cannot contain null"))?;
    ensure_storage_key_bytes(checked_row_write_add(
        checked_row_write_add(index_bytes, 1)?,
        primary_bytes,
    )?)
}

fn storage_tuple_bytes(
    schema: &TableSchema,
    columns: &[String],
    row: &Row,
    omit_nulls: bool,
) -> Result<Option<usize>> {
    let mut bytes = 0usize;
    for name in columns {
        let value = row.get(name).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing key column `{name}`",
                schema.name
            ))
        })?;
        if value == &Value::Null {
            if omit_nulls {
                return Ok(None);
            }
            return Err(EngineError::invalid_change(format!(
                "Primary-key column `{name}` in `{}` cannot be null",
                schema.name
            )));
        }
        let data_type = schema
            .columns
            .iter()
            .find(|column| column.name == *name)
            .map(|column| column.data_type);
        let payload = match data_type {
            Some(ColumnType::Boolean) => 1,
            Some(ColumnType::Integer | ColumnType::Float) => 8,
            Some(ColumnType::Text) => value.as_str().map(str::len).ok_or_else(|| {
                EngineError::type_mismatch(format!(
                    "Key column `{name}` in `{}` expects text",
                    schema.name
                ))
            })?,
            Some(ColumnType::Json) => validate_json_value(value)?,
            None => match value {
                Value::Bool(_) => 1,
                Value::Number(number) => number.to_string().len(),
                Value::String(value) => value.len(),
                Value::Array(_) | Value::Object(_) => validate_json_value(value)?,
                Value::Null => unreachable!(),
            },
        };
        bytes = checked_row_write_add(bytes, checked_row_write_add(3, payload)?)?;
        ensure_storage_key_bytes(bytes)?;
    }
    Ok(Some(bytes))
}

fn ensure_storage_key_bytes(bytes: usize) -> Result<()> {
    if bytes > MAX_STORAGE_KEY_BYTES {
        Err(EngineError::invalid_change(format!(
            "An encoded primary or index key cannot exceed {MAX_STORAGE_KEY_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

fn validate_catalog_name_bound(name: &str) -> Result<()> {
    if name.len().saturating_add(1) > MAX_STORAGE_KEY_BYTES {
        Err(row_write_limit_error(format!(
            "A catalog name cannot exceed {} UTF-8 bytes",
            MAX_STORAGE_KEY_BYTES - 1
        )))
    } else {
        Ok(())
    }
}

fn checked_row_write_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right).ok_or_else(row_write_overflow_error)
}

fn checked_row_write_mul(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right).ok_or_else(row_write_overflow_error)
}

fn ensure_row_write_bytes(bytes: usize) -> Result<()> {
    if bytes > MAX_ROW_WRITE_BYTES {
        Err(row_write_limit_error(format!(
            "A row write-set cannot retain more than {MAX_ROW_WRITE_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

fn row_write_overflow_error() -> EngineError {
    row_write_limit_error("A row write-set size overflowed".to_owned())
}

fn row_write_limit_error(message: String) -> EngineError {
    EngineError::new("RESOURCE_LIMIT", message)
}

pub(crate) fn ensure_batch_change_count(count: usize) -> Result<()> {
    if count > MAX_BATCH_CHANGES {
        Err(EngineError::new(
            "TRANSACTION_TOO_LARGE",
            format!("A change batch cannot contain more than {MAX_BATCH_CHANGES} changes"),
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn preflight_change_batch(
    changes: &[Change],
    schemas: &BTreeMap<&str, &TableSchema>,
    indexes: &[&IndexDefinition],
) -> Result<()> {
    ensure_batch_change_count(changes.len())?;
    preflight_row_changes(changes, schemas, indexes)
}

pub(crate) fn preflight_row_write_set(
    changes: &[Change],
    schemas: &BTreeMap<&str, &TableSchema>,
    indexes: &[&IndexDefinition],
) -> Result<()> {
    if changes.len() > MAX_ROW_WRITE_CHANGES {
        return Err(row_write_limit_error(format!(
            "A row write-set cannot contain more than {MAX_ROW_WRITE_CHANGES} changes"
        )));
    }
    preflight_row_changes(changes, schemas, indexes)
}

fn preflight_row_changes(
    changes: &[Change],
    schemas: &BTreeMap<&str, &TableSchema>,
    indexes: &[&IndexDefinition],
) -> Result<()> {
    let mut batch_bytes = 0usize;
    for change in changes {
        let (table, input, is_delete) = match change {
            Change::Upsert { table, row } => (table, row, false),
            Change::Delete { table, key } => (table, key, true),
        };
        validate_catalog_name_bound(table)
            .map_err(|error| EngineError::invalid_change(error.message))?;
        let schema = schemas
            .get(table.as_str())
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let input_row_bytes = validate_row_value_limits(input)
            .map_err(|error| EngineError::invalid_change(error.message))?;
        let mut retained = estimated_row_bytes(input)?;
        if is_delete {
            validate_primary_key_values(schema, input)?;
            validate_primary_storage_key_bound(schema, input)?;
        }
        let row_bytes = if !is_delete && !schema.columns.is_empty() {
            let mut row_bytes = 2;
            for (index, column) in schema.columns.iter().enumerate() {
                let value = column.default.as_ref().unwrap_or(&Value::Null);
                let value = input.get(&column.name).unwrap_or(value);
                if index != 0 {
                    row_bytes = checked_row_write_add(row_bytes, 1)?;
                }
                let value_bytes = encoded_json_bytes(value, 1)
                    .map_err(|error| EngineError::invalid_change(error.message))?;
                row_bytes = checked_row_write_add(
                    row_bytes,
                    checked_row_write_add(
                        encoded_json_string_bytes(&column.name)?,
                        checked_row_write_add(value_bytes, 1)?,
                    )?,
                )?;
                if !input.contains_key(&column.name) {
                    retained = checked_row_write_add(retained, 64)?;
                    retained = checked_row_write_add(
                        retained,
                        checked_row_write_mul(column.name.len(), 2)?,
                    )?;
                    retained = checked_row_write_add(
                        retained,
                        checked_row_write_mul(estimated_value_bytes(value)?, 2)?,
                    )?;
                }
            }
            if row_bytes > MAX_LOGICAL_ROW_BYTES {
                return Err(EngineError::invalid_change(format!(
                    "A normalized row cannot exceed {MAX_LOGICAL_ROW_BYTES} encoded bytes"
                )));
            }
            row_bytes
        } else {
            input_row_bytes
        };
        if !is_delete {
            validate_prospective_storage_keys(schema, input, indexes)?;
        }
        debug_assert!(row_bytes <= MAX_LOGICAL_ROW_BYTES);
        batch_bytes = checked_row_write_add(
            batch_bytes,
            checked_row_write_add(table.len(), checked_row_write_add(retained, 64)?)?,
        )?;
        if batch_bytes > MAX_ROW_WRITE_BYTES {
            return Err(EngineError::new(
                "TRANSACTION_TOO_LARGE",
                format!("A change batch cannot retain more than {MAX_ROW_WRITE_BYTES} bytes"),
            ));
        }
    }
    Ok(())
}

fn validate_prospective_storage_keys(
    schema: &TableSchema,
    input: &Row,
    indexes: &[&IndexDefinition],
) -> Result<()> {
    let value_for = |name: &str| {
        input.get(name).or_else(|| {
            schema
                .columns
                .iter()
                .find(|column| column.name == name)
                .and_then(|column| column.default.as_ref())
        })
    };
    let tuple_bytes = |columns: &[String], omit_nulls: bool| -> Result<Option<usize>> {
        let mut bytes = 0usize;
        for name in columns {
            let value = value_for(name).unwrap_or(&Value::Null);
            if value == &Value::Null {
                if omit_nulls {
                    return Ok(None);
                }
                return Err(EngineError::invalid_change(format!(
                    "Primary-key column `{name}` in `{}` cannot be null",
                    schema.name
                )));
            }
            let data_type = schema
                .columns
                .iter()
                .find(|column| column.name == *name)
                .map(|column| column.data_type);
            let payload = match data_type {
                Some(ColumnType::Boolean) => 1,
                Some(ColumnType::Integer | ColumnType::Float) => 8,
                Some(ColumnType::Text) => value.as_str().map(str::len).ok_or_else(|| {
                    EngineError::type_mismatch(format!(
                        "Key column `{name}` in `{}` expects text",
                        schema.name
                    ))
                })?,
                Some(ColumnType::Json) => validate_json_value(value)?,
                None => match value {
                    Value::Bool(_) => 1,
                    Value::Number(number) => number.to_string().len(),
                    Value::String(value) => value.len(),
                    Value::Array(_) | Value::Object(_) => validate_json_value(value)?,
                    Value::Null => unreachable!(),
                },
            };
            bytes = checked_row_write_add(bytes, checked_row_write_add(3, payload)?)?;
            ensure_storage_key_bytes(bytes)?;
        }
        Ok(Some(bytes))
    };

    let primary_bytes =
        tuple_bytes(&schema.primary_key, false)?.expect("primary key tuple does not omit nulls");
    for definition in indexes
        .iter()
        .copied()
        .filter(|definition| definition.table == schema.name)
    {
        let Some(index_bytes) = tuple_bytes(&definition.columns, true)? else {
            continue;
        };
        ensure_storage_key_bytes(checked_row_write_add(
            checked_row_write_add(index_bytes, 1)?,
            primary_bytes,
        )?)?;
    }
    Ok(())
}

fn unique_index_violation(index: &str) -> EngineError {
    EngineError::constraint_violation(format!("Index `{index}` would contain duplicate values"))
}

fn next_revision(revision: u64) -> Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| EngineError::new("REVISION_OVERFLOW", "Database revision overflowed"))
}

fn snapshot_validation_error(error: EngineError) -> EngineError {
    EngineError::invalid_snapshot(error.message)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn nested_json(depth: usize) -> Value {
        (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
    }

    fn storage() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "posts".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        storage
    }

    fn typed_storage() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "users".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![
                    ColumnDefinition {
                        name: "id".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "email".to_owned(),
                        data_type: ColumnType::Text,
                        nullable: true,
                        default: None,
                    },
                ],
            })
            .unwrap();
        storage
    }

    #[test]
    fn replace_table_is_atomic() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();

        let error = storage
            .replace_table(
                "posts",
                vec![
                    row(json!({"id": 2, "title": "duplicate"})),
                    row(json!({"id": 2, "title": "duplicate"})),
                ],
            )
            .unwrap_err();

        assert_eq!(error.code, "INVALID_CHANGE");
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
        assert_eq!(storage.revision(), 1);
    }

    #[test]
    fn failed_change_batch_rolls_back_every_change() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "before"}))])
            .unwrap();

        let error = storage
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({"id": 1, "title": "after"})),
                    },
                    Change::Upsert {
                        table: "missing".to_owned(),
                        row: row(json!({"id": 2})),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap_err();

        assert_eq!(error.code, "TABLE_NOT_FOUND");
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "before"}))]
        );
        assert_eq!(storage.revision(), 1);
    }

    #[test]
    fn typed_delete_keys_are_validated_before_an_atomic_batch_is_published() {
        let mut storage = typed_storage();
        storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 1, "email": "kept@example.com"})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();
        let revision = storage.revision();

        let error = storage
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Delete {
                        table: "users".to_owned(),
                        key: row(json!({"id": "wrong"})),
                    },
                    Change::Delete {
                        table: "users".to_owned(),
                        key: row(json!({"id": 1})),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap_err();

        assert_eq!(error.code, "TYPE_MISMATCH");
        assert_eq!(storage.revision(), revision);
        assert_eq!(
            storage
                .lookup_primary_key("users", &row(json!({"id": 1})))
                .unwrap(),
            Some(row(json!({"id": 1, "email": "kept@example.com"})))
        );

        let oversized = storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Delete {
                    table: "users".to_owned(),
                    key: row(json!({"id": 1, "extra": "x".repeat(MAX_LOGICAL_VALUE_BYTES + 1)})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(oversized.code, "INVALID_CHANGE");
        assert_eq!(storage.revision(), revision);
    }

    #[test]
    fn public_batches_reject_oversized_secondary_keys_before_mutation() {
        let mut storage = typed_storage();
        storage
            .define_index(IndexDefinition {
                name: "users_email".to_owned(),
                table: "users".to_owned(),
                columns: vec!["email".to_owned()],
                unique: false,
            })
            .unwrap();
        let revision = storage.revision();

        let error = storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 1, "email": "x".repeat(MAX_STORAGE_KEY_BYTES)})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap_err();

        assert_eq!(error.code, "INVALID_CHANGE");
        assert_eq!(storage.revision(), revision);
        assert_eq!(storage.table_row_count("users").unwrap(), 0);
    }

    #[test]
    fn deeply_nested_json_change_batches_fail_atomically() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();
        let revision = storage.revision();

        let error = storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "posts".to_owned(),
                    row: row(json!({
                        "id": 2,
                        "payload": nested_json(MAX_JSON_DEPTH + 1),
                    })),
                }],
                ..ChangeBatch::default()
            })
            .unwrap_err();

        assert_eq!(error.code, "INVALID_CHANGE");
        assert!(error.message.contains("cannot nest more than 64 levels"));
        assert_eq!(storage.revision(), revision);
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
    }

    #[test]
    fn public_change_batches_share_the_paged_change_count_limit() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();
        let revision = storage.revision();

        let error = storage
            .apply_batch(&ChangeBatch {
                changes: (0..=MAX_BATCH_CHANGES)
                    .map(|id| Change::Delete {
                        table: "posts".to_owned(),
                        key: row(json!({"id": id})),
                    })
                    .collect(),
                ..ChangeBatch::default()
            })
            .unwrap_err();

        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
        assert!(error.message.contains("100000 changes"));
        assert_eq!(storage.revision(), revision);
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
    }

    #[test]
    fn row_write_sets_apply_primary_and_unique_swaps_against_the_final_state() {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "accounts".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![
                    ColumnDefinition {
                        name: "id".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "tenant".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "email".to_owned(),
                        data_type: ColumnType::Text,
                        nullable: true,
                        default: None,
                    },
                ],
            })
            .unwrap();
        storage
            .replace_table(
                "accounts",
                vec![
                    row(json!({"id": 1, "tenant": 7, "email": "a@example.com"})),
                    row(json!({"id": 2, "tenant": 7, "email": "b@example.com"})),
                    row(json!({"id": 3, "tenant": 7, "email": null})),
                    row(json!({"id": 4, "tenant": 7, "email": null})),
                ],
            )
            .unwrap();
        storage
            .define_index(IndexDefinition {
                name: "accounts_tenant_email".to_owned(),
                table: "accounts".to_owned(),
                columns: vec!["tenant".to_owned(), "email".to_owned()],
                unique: true,
            })
            .unwrap();

        storage
            .apply_row_changes_unrevisioned(vec![
                Change::Delete {
                    table: "accounts".to_owned(),
                    key: row(json!({"id": 1})),
                },
                Change::Delete {
                    table: "accounts".to_owned(),
                    key: row(json!({"id": 2})),
                },
                Change::Upsert {
                    table: "accounts".to_owned(),
                    row: row(json!({"id": 1, "tenant": 7, "email": "a@example.com"})),
                },
                Change::Upsert {
                    table: "accounts".to_owned(),
                    row: row(json!({"id": 2, "tenant": 7, "email": "b@example.com"})),
                },
                Change::Upsert {
                    table: "accounts".to_owned(),
                    row: row(json!({"id": 1, "tenant": 7, "email": "b@example.com"})),
                },
                Change::Upsert {
                    table: "accounts".to_owned(),
                    row: row(json!({"id": 2, "tenant": 7, "email": "a@example.com"})),
                },
            ])
            .unwrap();
        assert_eq!(
            storage
                .lookup_primary_key("accounts", &row(json!({"id": 1})))
                .unwrap()
                .unwrap()["email"],
            "b@example.com"
        );
        assert_eq!(
            storage
                .lookup_index(
                    "accounts",
                    &["tenant".to_owned(), "email".to_owned()],
                    &row(json!({"tenant": 7, "email": "a@example.com"})),
                )
                .unwrap()
                .unwrap()[0]["id"],
            2
        );
        assert_eq!(
            storage
                .lookup_index(
                    "accounts",
                    &["tenant".to_owned(), "email".to_owned()],
                    &row(json!({"tenant": 7, "email": null})),
                )
                .unwrap(),
            Some(vec![])
        );

        let before = storage.scan_table("accounts").unwrap();
        let error = storage
            .apply_row_changes_unrevisioned(vec![
                Change::Upsert {
                    table: "accounts".to_owned(),
                    row: row(json!({"id": 3, "tenant": 7, "email": "same@example.com"})),
                },
                Change::Upsert {
                    table: "accounts".to_owned(),
                    row: row(json!({"id": 4, "tenant": 7, "email": "same@example.com"})),
                },
            ])
            .unwrap_err();
        assert_eq!(error.code, "CONSTRAINT_VIOLATION");
        assert_eq!(storage.scan_table("accounts").unwrap(), before);
    }

    #[test]
    fn delete_uses_composite_primary_key() {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "memberships".to_owned(),
                primary_key: vec!["team_id".to_owned(), "user_id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        storage
            .replace_table(
                "memberships",
                vec![
                    row(json!({"team_id": 1, "user_id": 1, "role": "owner"})),
                    row(json!({"team_id": 1, "user_id": 2, "role": "member"})),
                ],
            )
            .unwrap();

        storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Delete {
                    table: "memberships".to_owned(),
                    key: row(json!({"team_id": 1, "user_id": 2})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();

        assert_eq!(
            storage.scan_table("memberships").unwrap(),
            vec![row(json!({"team_id": 1, "user_id": 1, "role": "owner"}))]
        );
    }

    #[test]
    fn primary_key_must_be_present_and_non_null() {
        let mut storage = storage();
        for invalid in [Map::new(), row(json!({"id": null}))] {
            let error = storage.replace_table("posts", vec![invalid]).unwrap_err();
            assert_eq!(error.code, "INVALID_CHANGE");
        }
    }

    #[test]
    fn replace_table_snapshot_atomically_defines_and_populates_a_table() {
        let mut storage = InMemoryStorage::default();
        let outcome = storage
            .replace_table_snapshot(
                TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                },
                vec![row(json!({"id": 1, "title": "first"}))],
            )
            .unwrap();

        assert_eq!(
            outcome,
            ApplyOutcome {
                revision: 1,
                tables: vec!["posts".to_owned()],
            }
        );
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "first"}))]
        );
    }

    #[test]
    fn failed_table_snapshot_does_not_leave_an_empty_schema() {
        let mut storage = InMemoryStorage::default();
        let error = storage
            .replace_table_snapshot(
                TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                },
                vec![
                    row(json!({"id": 1, "title": "duplicate"})),
                    row(json!({"id": 1, "title": "duplicate"})),
                ],
            )
            .unwrap_err();

        assert_eq!(error.code, "INVALID_CHANGE");
        assert_eq!(storage.revision(), 0);
        assert_eq!(
            storage.scan_table("posts").unwrap_err().code,
            "TABLE_NOT_FOUND"
        );
    }

    #[test]
    fn conflicting_table_snapshot_preserves_existing_schema_rows_and_revision() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();

        let error = storage
            .replace_table_snapshot(
                TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["slug".to_owned()],
                    columns: vec![],
                },
                vec![row(json!({"slug": "replacement"}))],
            )
            .unwrap_err();

        assert_eq!(error.code, "INVALID_SCHEMA");
        assert_eq!(storage.revision(), 1);
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
        storage
            .define_table(TableSchema {
                name: "posts".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
    }

    #[test]
    fn snapshot_round_trips_complete_state_deterministically() {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "empty".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        storage
            .define_table(TableSchema {
                name: "memberships".to_owned(),
                primary_key: vec!["team_id".to_owned(), "user_id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        storage
            .replace_table(
                "memberships",
                vec![
                    row(json!({"team_id": 2, "user_id": 1, "role": "member"})),
                    row(json!({"team_id": 1, "user_id": 2, "role": "owner"})),
                ],
            )
            .unwrap();
        storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "memberships".to_owned(),
                    row: row(json!({"team_id": 1, "user_id": 2, "role": "admin"})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();

        let first = storage.export_snapshot().unwrap();
        assert_eq!(storage.export_snapshot().unwrap(), first);

        let mut restored = InMemoryStorage::default();
        restored.import_snapshot(&first).unwrap();

        assert_eq!(restored.revision(), 2);
        assert!(restored.scan_table("empty").unwrap().is_empty());
        assert_eq!(
            restored.scan_table("memberships").unwrap(),
            vec![
                row(json!({"team_id": 1, "user_id": 2, "role": "admin"})),
                row(json!({"team_id": 2, "user_id": 1, "role": "member"})),
            ]
        );
        assert_eq!(restored.export_snapshot().unwrap(), first);

        // The restored schema is present and retains its composite primary key.
        restored
            .define_table(TableSchema {
                name: "memberships".to_owned(),
                primary_key: vec!["team_id".to_owned(), "user_id".to_owned()],
                columns: vec![],
            })
            .unwrap();
    }

    #[test]
    fn failed_snapshot_import_preserves_live_state() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();

        let invalid = StorageSnapshot {
            revision: 99,
            indexes: vec![],
            tables: vec![TableSnapshot {
                schema: TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                },
                rows: vec![
                    row(json!({"id": 2, "title": "duplicate"})),
                    row(json!({"id": 2, "title": "duplicate"})),
                ],
            }],
        };
        let error = storage
            .import_snapshot(&crate::snapshot::encode(&invalid).unwrap())
            .unwrap_err();

        assert_eq!(error.code, "INVALID_SNAPSHOT");
        assert_eq!(storage.revision(), 1);
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
    }

    #[test]
    fn snapshot_rejects_duplicate_tables_and_invalid_primary_keys() {
        let schema = TableSchema {
            name: "posts".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![],
        };
        let invalid_snapshots = [
            StorageSnapshot {
                revision: 0,
                indexes: vec![],
                tables: vec![
                    TableSnapshot {
                        schema: schema.clone(),
                        rows: vec![],
                    },
                    TableSnapshot {
                        schema: schema.clone(),
                        rows: vec![],
                    },
                ],
            },
            StorageSnapshot {
                revision: 0,
                indexes: vec![],
                tables: vec![TableSnapshot {
                    schema,
                    rows: vec![row(json!({"title": "missing id"}))],
                }],
            },
        ];

        for snapshot in invalid_snapshots {
            let error =
                InMemoryStorage::from_snapshot(&crate::snapshot::encode(&snapshot).unwrap())
                    .unwrap_err();
            assert_eq!(error.code, "INVALID_SNAPSHOT");
        }
    }

    #[test]
    fn snapshot_persists_definitions_and_rebuilds_index_postings() {
        let mut storage = typed_storage();
        storage
            .replace_table(
                "users",
                vec![
                    row(json!({"id": 1, "email": "one@example.com"})),
                    row(json!({"id": 2, "email": "two@example.com"})),
                ],
            )
            .unwrap();
        storage
            .define_index(IndexDefinition {
                name: "users_email".to_owned(),
                table: "users".to_owned(),
                columns: vec!["email".to_owned()],
                unique: true,
            })
            .unwrap();

        let bytes = storage.export_snapshot().unwrap();
        let restored = InMemoryStorage::from_snapshot(&bytes).unwrap();
        assert_eq!(restored.export_snapshot().unwrap(), bytes);
        assert_eq!(
            restored
                .lookup_index(
                    "users",
                    &["email".to_owned()],
                    &row(json!({"email": "two@example.com"})),
                )
                .unwrap()
                .unwrap(),
            vec![row(json!({"id": 2, "email": "two@example.com"}))]
        );
    }

    #[test]
    fn previous_snapshot_versions_restore_without_indexes() {
        let storage = typed_storage();
        for version in [1_u16, 2_u16] {
            let mut bytes = storage.export_snapshot().unwrap();
            bytes[8..10].copy_from_slice(&version.to_le_bytes());
            let restored = InMemoryStorage::from_snapshot(&bytes).unwrap();
            assert!(restored.indexes_for_table("users").unwrap().is_empty());
        }
    }

    #[test]
    fn snapshot_rejects_invalid_or_non_unique_index_definitions() {
        let schema = typed_storage().table_schema("users").unwrap();
        let invalid = StorageSnapshot {
            revision: 8,
            tables: vec![TableSnapshot {
                schema,
                rows: vec![
                    row(json!({"id": 1, "email": "same@example.com"})),
                    row(json!({"id": 2, "email": "same@example.com"})),
                ],
            }],
            indexes: vec![IndexDefinition {
                name: "users_email".to_owned(),
                table: "users".to_owned(),
                columns: vec!["email".to_owned()],
                unique: true,
            }],
        };
        assert_eq!(
            InMemoryStorage::from_snapshot(&crate::snapshot::encode(&invalid).unwrap())
                .unwrap_err()
                .code,
            "INVALID_SNAPSHOT"
        );

        let mut duplicate_definitions = invalid;
        duplicate_definitions.tables[0].rows.pop();
        duplicate_definitions
            .indexes
            .push(duplicate_definitions.indexes[0].clone());
        assert_eq!(
            InMemoryStorage::from_snapshot(
                &crate::snapshot::encode(&duplicate_definitions).unwrap()
            )
            .unwrap_err()
            .code,
            "INVALID_SNAPSHOT"
        );
    }

    #[test]
    fn visitors_stop_without_collecting_the_remaining_rows() {
        let mut storage = storage();
        storage
            .replace_table(
                "posts",
                vec![
                    row(json!({"id": 1})),
                    row(json!({"id": 2})),
                    row(json!({"id": 3})),
                ],
            )
            .unwrap();

        let mut visited = Vec::new();
        let outcome = storage
            .visit_table("posts", &mut |row| {
                visited.push(row["id"].clone());
                Ok(VisitControl::Stop)
            })
            .unwrap();

        assert_eq!(outcome, VisitOutcome::Stopped);
        assert_eq!(visited, vec![json!(1)]);
        assert_eq!(storage.table_row_count("posts").unwrap(), 3);
        assert_eq!(storage.visitor_counts(), (1, 0));
    }

    #[test]
    fn index_visitors_distinguish_an_absent_index_from_an_empty_posting() {
        let mut storage = typed_storage();
        storage
            .replace_table(
                "users",
                vec![row(json!({"id": 1, "email": "one@example.com"}))],
            )
            .unwrap();
        let columns = vec!["email".to_owned()];
        let key = row(json!({"email": "missing@example.com"}));
        let mut calls = 0;

        assert_eq!(
            storage
                .visit_index("users", &columns, &key, &mut |_| {
                    calls += 1;
                    Ok(VisitControl::Continue)
                })
                .unwrap(),
            None
        );
        storage
            .define_index(IndexDefinition {
                name: "users_email".to_owned(),
                table: "users".to_owned(),
                columns: columns.clone(),
                unique: false,
            })
            .unwrap();
        assert_eq!(
            storage
                .visit_index("users", &columns, &key, &mut |_| {
                    calls += 1;
                    Ok(VisitControl::Continue)
                })
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
        assert_eq!(calls, 0);
    }
}
