#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use crate::{
    ColumnDefinition, ColumnType, EngineError, IndexDefinition, Result, Row, RowChange,
    TableDefinition,
};

const MAX_COLUMNS: usize = 256;
pub(crate) const MAX_JSON_DEPTH: usize = 64;
pub(crate) const MAX_LOGICAL_VALUE_BYTES: usize = crate::MAX_BTREE_VALUE_BYTES;
pub(crate) const MAX_LOGICAL_ROW_BYTES: usize = crate::MAX_BTREE_VALUE_BYTES - 8;
pub(crate) const MAX_STORAGE_KEY_BYTES: usize = crate::MAX_BTREE_KEY_BYTES;
const MAX_ROW_WRITE_CHANGES: usize = 200_000;
const MAX_ROW_WRITE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VisitControl {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VisitOutcome {
    Complete,
    Stopped,
}

/// Read-only relational storage used by query planning and execution.
///
/// Keeping this contract independent of mutation lets a page-backed reader
/// stream rows before its copy-on-write write path is complete.
pub(crate) trait StorageReader {
    /// Fails when the reader cannot safely answer from its current view.
    ///
    /// Readers without a fallible recovery state can use this default. Durable readers override
    /// it after an ambiguous publication so metadata-only no-op statements cannot report success.
    #[doc(hidden)]
    fn ensure_readable(&self) -> Result<()> {
        Ok(())
    }

    /// Charges work to a caller-defined budget spanning more than one query operator.
    ///
    /// Ordinary readers have no cross-statement budget. Script candidates override this so
    /// repeated bounded statements cannot multiply into an unbounded batch.
    #[doc(hidden)]
    fn charge_work(&self, _operations: usize) -> Result<()> {
        Ok(())
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<VisitOutcome>;
    fn table_row_count(&self, table: &str) -> Result<usize>;
    /// Collecting helper for operators that require all table rows at once.
    #[cfg(test)]
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
    /// Collecting helper for callers that require all matching index rows.
    #[cfg(test)]
    fn lookup_index(&self, table: &str, columns: &[String], key: &Row) -> Result<Option<Vec<Row>>> {
        let mut rows = Vec::new();
        let outcome = self.visit_index(table, columns, key, &mut |row| {
            rows.push(row.clone());
            Ok(VisitControl::Continue)
        })?;
        Ok(outcome.map(|_| rows))
    }
    fn table_schema(&self, table: &str) -> Result<TableDefinition>;
    fn revision(&self) -> u64;
}

#[cfg(test)]
pub(crate) trait StorageDriver: StorageReader {
    fn define_table(&mut self, schema: TableDefinition) -> Result<()>;
    fn drop_table(&mut self, table: &str) -> Result<()>;
    fn add_column(&mut self, table: &str, column: ColumnDefinition) -> Result<()>;
    fn define_index(&mut self, definition: IndexDefinition) -> Result<()>;
    fn drop_index(&mut self, name: &str) -> Result<()>;
    /// Atomically applies an already planned row write-set without publishing a revision.
    ///
    /// Callers use this for one SQL statement, then publish exactly one revision only after the
    /// complete statement succeeds. Deletes and upserts are interpreted as one final-state write
    /// set, so primary-key and unique-index swaps do not depend on mutation order.
    #[doc(hidden)]
    fn apply_row_changes_unrevisioned(&mut self, changes: Vec<RowChange>) -> Result<()>;
    fn advance_revision(&mut self) -> Result<u64>;
}

#[derive(Clone, Debug, Default)]
#[cfg(test)]
pub(crate) struct InMemoryStorage {
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
#[cfg(test)]
struct TableData {
    schema: TableDefinition,
    rows: BTreeMap<String, Row>,
}

#[derive(Clone, Debug)]
#[cfg(test)]
struct IndexData {
    definition: IndexDefinition,
    postings: BTreeMap<String, BTreeSet<String>>,
}

#[cfg(test)]
struct PreparedIndexChange {
    index: String,
    primary_key: String,
    old_key: Option<String>,
    new_key: Option<String>,
}

#[cfg(test)]
impl InMemoryStorage {
    pub(crate) fn access_counts(&self) -> (usize, usize) {
        (self.scan_count.get(), self.lookup_count.get())
    }

    pub(crate) fn visitor_counts(&self) -> (usize, usize) {
        (self.visited_row_count.get(), self.collector_count.get())
    }

    pub(crate) fn reset_counts(&self) {
        self.scan_count.set(0);
        self.lookup_count.set(0);
        self.visited_row_count.set(0);
        self.collector_count.set(0);
    }
}

#[cfg(test)]
impl StorageDriver for InMemoryStorage {
    fn define_table(&mut self, schema: TableDefinition) -> Result<()> {
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

    fn apply_row_changes_unrevisioned(&mut self, changes: Vec<RowChange>) -> Result<()> {
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
                RowChange::Upsert { table, row } => {
                    let schema = &self
                        .tables
                        .get(&table)
                        .ok_or_else(|| EngineError::table_not_found(&table))?
                        .schema;
                    let row = normalize_row(schema, row)?;
                    let key = row_key(schema, &row)?;
                    (table, key, Some(row))
                }
                RowChange::Delete { table, key } => {
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
    current: &TableDefinition,
    column: &ColumnDefinition,
) -> Result<TableDefinition> {
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
    current: &TableDefinition,
    column: &ColumnDefinition,
) -> Result<()> {
    let table = &current.name;
    validate_schema(current)?;
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

#[cfg(test)]
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

    fn table_schema(&self, table: &str) -> Result<TableDefinition> {
        self.tables
            .get(table)
            .map(|table| table.schema.clone())
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

#[cfg(test)]
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
    schema: &TableDefinition,
) -> Result<()> {
    debug_assert_eq!(definition.table, schema.name);
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

#[cfg(test)]
fn build_postings(
    schema: &TableDefinition,
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

#[cfg(test)]
fn index_key(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<String>> {
    validated_index_key(schema, definition, row)
}

#[cfg(test)]
pub(crate) fn validated_index_key(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<String>> {
    validate_secondary_storage_key_bound(schema, definition, row)?;
    index_key_from_stored_row(schema, definition, row)
}

#[cfg(test)]
fn index_key_from_stored_row(
    _schema: &TableDefinition,
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

#[cfg(test)]
fn index_lookup_key(
    schema: &TableDefinition,
    definition: &IndexDefinition,
    row: &Row,
) -> Result<Option<String>> {
    let Some(bytes) = storage_tuple_bytes(schema, &definition.columns, row, true)? else {
        return Ok(None);
    };
    ensure_storage_key_bytes(checked_row_write_add(bytes, 1)?)?;
    index_key_from_stored_row(schema, definition, row)
}

pub(crate) fn validate_schema(schema: &TableDefinition) -> Result<()> {
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
        return Err(EngineError::invalid_schema(format!(
            "Table `{}` must declare at least one column",
            schema.name
        )));
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

pub(crate) fn normalize_row(schema: &TableDefinition, mut row: Row) -> Result<Row> {
    validate_row_value_limits(&row).map_err(|error| EngineError::invalid_change(error.message))?;
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

pub(crate) fn validate_value(column: &ColumnDefinition, value: &Value, table: &str) -> Result<()> {
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

pub(crate) fn row_key(schema: &TableDefinition, row: &Row) -> Result<String> {
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

fn validate_primary_key_values(schema: &TableDefinition, row: &Row) -> Result<()> {
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

pub(crate) fn validate_primary_storage_key_bound(
    schema: &TableDefinition,
    row: &Row,
) -> Result<()> {
    let bytes = storage_tuple_bytes(schema, &schema.primary_key, row, false)?
        .ok_or_else(|| EngineError::invalid_change("A primary key cannot contain null"))?;
    ensure_storage_key_bytes(bytes)
}

#[cfg(test)]
pub(crate) fn validate_secondary_storage_key_bound(
    schema: &TableDefinition,
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
    schema: &TableDefinition,
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
            .ok_or_else(|| EngineError::column_not_found(name, &schema.name))?
            .data_type;
        let payload = match data_type {
            ColumnType::Boolean => 1,
            ColumnType::Integer | ColumnType::Float => 8,
            ColumnType::Text => value.as_str().map(str::len).ok_or_else(|| {
                EngineError::type_mismatch(format!(
                    "Key column `{name}` in `{}` expects text",
                    schema.name
                ))
            })?,
            ColumnType::Json => validate_json_value(value)?,
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

#[cfg(test)]
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

pub(crate) fn preflight_row_write_set(
    changes: &[RowChange],
    schemas: &BTreeMap<&str, &TableDefinition>,
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
    changes: &[RowChange],
    schemas: &BTreeMap<&str, &TableDefinition>,
    indexes: &[&IndexDefinition],
) -> Result<()> {
    let mut batch_bytes = 0usize;
    for change in changes {
        let (table, input, is_delete) = match change {
            RowChange::Upsert { table, row } => (table, row, false),
            RowChange::Delete { table, key } => (table, key, true),
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
        let row_bytes = if !is_delete {
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
                format!("A row write-set cannot retain more than {MAX_ROW_WRITE_BYTES} bytes"),
            ));
        }
    }
    Ok(())
}

fn validate_prospective_storage_keys(
    schema: &TableDefinition,
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
                .ok_or_else(|| EngineError::column_not_found(name, &schema.name))?
                .data_type;
            let payload = match data_type {
                ColumnType::Boolean => 1,
                ColumnType::Integer | ColumnType::Float => 8,
                ColumnType::Text => value.as_str().map(str::len).ok_or_else(|| {
                    EngineError::type_mismatch(format!(
                        "Key column `{name}` in `{}` expects text",
                        schema.name
                    ))
                })?,
                ColumnType::Json => validate_json_value(value)?,
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

#[cfg(test)]
fn unique_index_violation(index: &str) -> EngineError {
    EngineError::constraint_violation(format!("Index `{index}` would contain duplicate values"))
}

#[cfg(test)]
fn next_revision(revision: u64) -> Result<u64> {
    crate::revision::next_database_revision(revision)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn users_storage() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableDefinition {
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
    fn table_definitions_require_a_typed_column_catalog() {
        let error = validate_schema(&TableDefinition {
            name: "users".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![],
        })
        .unwrap_err();

        assert_eq!(error.code, "INVALID_SCHEMA");
        assert!(error.message.contains("at least one column"));
    }

    #[test]
    fn row_write_sets_apply_primary_and_unique_swaps_against_the_final_state() {
        let mut storage = users_storage();
        storage
            .define_index(IndexDefinition {
                name: "users_email".to_owned(),
                table: "users".to_owned(),
                columns: vec!["email".to_owned()],
                unique: true,
            })
            .unwrap();
        storage
            .apply_row_changes_unrevisioned(vec![
                RowChange::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 1, "email": "one@example.com"})),
                },
                RowChange::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 2, "email": "two@example.com"})),
                },
            ])
            .unwrap();

        storage
            .apply_row_changes_unrevisioned(vec![
                RowChange::Delete {
                    table: "users".to_owned(),
                    key: row(json!({"id": 1})),
                },
                RowChange::Delete {
                    table: "users".to_owned(),
                    key: row(json!({"id": 2})),
                },
                RowChange::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 1, "email": "two@example.com"})),
                },
                RowChange::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 2, "email": "one@example.com"})),
                },
            ])
            .unwrap();

        assert_eq!(
            storage
                .lookup_primary_key("users", &row(json!({"id": 1})))
                .unwrap()
                .unwrap()["email"],
            "two@example.com"
        );
        assert_eq!(
            storage
                .lookup_index(
                    "users",
                    &["email".to_owned()],
                    &row(json!({"email": "one@example.com"})),
                )
                .unwrap()
                .unwrap()[0]["id"],
            2
        );
    }

    #[test]
    fn invalid_row_write_sets_leave_the_reference_state_unchanged() {
        let mut storage = users_storage();
        storage
            .apply_row_changes_unrevisioned(vec![RowChange::Upsert {
                table: "users".to_owned(),
                row: row(json!({"id": 1, "email": "kept@example.com"})),
            }])
            .unwrap();

        let before = storage.scan_table("users").unwrap();
        let error = storage
            .apply_row_changes_unrevisioned(vec![
                RowChange::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 1, "email": "changed@example.com"})),
                },
                RowChange::Upsert {
                    table: "missing".to_owned(),
                    row: row(json!({"id": 2})),
                },
            ])
            .unwrap_err();

        assert_eq!(error.code, "TABLE_NOT_FOUND");
        assert_eq!(storage.scan_table("users").unwrap(), before);
    }

    #[test]
    fn visitors_stop_without_collecting_remaining_rows() {
        let mut storage = users_storage();
        storage
            .apply_row_changes_unrevisioned(vec![
                RowChange::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 1, "email": null})),
                },
                RowChange::Upsert {
                    table: "users".to_owned(),
                    row: row(json!({"id": 2, "email": null})),
                },
            ])
            .unwrap();

        let mut visited = Vec::new();
        let outcome = storage
            .visit_table("users", &mut |row| {
                visited.push(row["id"].clone());
                Ok(VisitControl::Stop)
            })
            .unwrap();

        assert_eq!(outcome, VisitOutcome::Stopped);
        assert_eq!(visited, vec![json!(1)]);
        assert_eq!(storage.table_row_count("users").unwrap(), 2);
        assert_eq!(storage.visitor_counts(), (1, 0));
    }
}
