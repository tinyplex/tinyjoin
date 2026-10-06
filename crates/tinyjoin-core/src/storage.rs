#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use serde_json::Value;

use crate::hash::KeySet;
use crate::paged_codec::{EMPTY_RECORD, IndexEntryLayout, RecordLayout, StoredRecord};
use crate::query::Filter;
use crate::row::{RowRef, ValueRef};
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

/// Inclusive bounds on the leading component of a table's keys or an index's entries, either of
/// which may be absent. Bounds are drawn from a predicate, and may admit rows it rejects but never
/// exclude a row it accepts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct KeyRange {
    pub(crate) lower: Option<Vec<u8>>,
    pub(crate) upper: Option<Vec<u8>>,
}

impl KeyRange {
    /// Where a visit in ascending key order starts.
    pub(crate) fn start(&self) -> &[u8] {
        self.lower.as_deref().unwrap_or_default()
    }

    /// Where a visit in descending key order starts, just past every key whose leading component
    /// is at or below the upper bound: that bound with its last byte incremented. No component is
    /// a proper prefix of another, or of an upper bound, so this admits exactly those keys.
    pub(crate) fn end(&self) -> Option<Vec<u8>> {
        let mut end = self.upper.clone()?;
        while let Some(last) = end.pop() {
            if last < u8::MAX {
                end.push(last + 1);
                return Some(end);
            }
        }
        None
    }

    /// Whether a key whose leading component is `component` lies within the range. A visit in key
    /// order stops at the first key that does not.
    pub(crate) fn contains(&self, component: &[u8]) -> bool {
        self.lower.as_deref().is_none_or(|lower| component >= lower)
            && self.upper.as_deref().is_none_or(|upper| component <= upper)
    }
}

/// The order in which a visit returns rows, by primary key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyOrder {
    Ascending,
    Descending,
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

    /// Visits every row of a table, in primary-key order for stored rows.
    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome>;
    /// Whether [`Self::visit_index`] serves `table`'s indexes, as it cannot while a transaction has
    /// staged changes to the table that its index entries do not yet reflect.
    fn visits_indexes(&self, table: &str) -> bool {
        let _ = table;
        true
    }
    /// Whether this reader visits `table`'s rows in primary-key order, from every visit, so that a
    /// query ordered by its primary key can stream them.
    fn visits_in_key_order(&self, table: &str) -> bool {
        let _ = table;
        false
    }
    /// Visits the rows whose leading primary-key column lies within `range`, in `order` if the
    /// reader [visits in key order](Self::visits_in_key_order). Readers without ordered keys visit
    /// every row, as [`Self::visit_table`] does.
    fn visit_table_range(
        &self,
        table: &str,
        range: &KeyRange,
        order: KeyOrder,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let _ = (range, order);
        self.visit_table(table, visitor)
    }
    /// Visits the rows of a table that `filter` accepts, as [`Self::visit_table`] visits them all,
    /// charging each row it reads, accepted or not, to `scanned`. A reader of stored records tests
    /// each record before presenting its row, so that a rejected row is never decoded.
    fn visit_table_where(
        &self,
        table: &str,
        filter: &Filter<'_>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_table(table, &mut |row| {
            scanned(1)?;
            if filter.matches(row)? {
                visitor(row)
            } else {
                Ok(VisitControl::Continue)
            }
        })
    }
    /// Visits the rows within `range` that `filter` accepts, as [`Self::visit_table_range`] visits
    /// them all, charging each row read to `scanned` as [`Self::visit_table_where`] does.
    fn visit_table_range_where(
        &self,
        table: &str,
        range: &KeyRange,
        order: KeyOrder,
        filter: &Filter<'_>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_table_range(table, range, order, &mut |row| {
            scanned(1)?;
            if filter.matches(row)? {
                visitor(row)
            } else {
                Ok(VisitControl::Continue)
            }
        })
    }
    fn table_row_count(&self, table: &str) -> Result<usize>;
    /// Collecting helper for operators that require all table rows at once.
    #[cfg(test)]
    fn scan_table(&self, table: &str) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        self.visit_table(table, &mut |row| {
            rows.push(row.to_row()?);
            Ok(VisitControl::Continue)
        })?;
        Ok(rows)
    }
    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>>;
    /// Visits the row whose primary key is `key`, if there is one, as [`Self::lookup_primary_key`]
    /// finds it. Readers that store records visit it in place, without decoding it into a map.
    fn visit_primary_key(
        &self,
        table: &str,
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let schema = self.table_schema(table)?;
        match self.lookup_primary_key(table, key)? {
            Some(row) if visitor(&RowRef::map(&row, &schema))? == VisitControl::Stop => {
                Ok(VisitOutcome::Stopped)
            }
            _ => Ok(VisitOutcome::Complete),
        }
    }
    /// Visits the row whose primary key has `values`, one for each of `schema`'s key columns in
    /// key order, as [`Self::visit_primary_key`] visits the row of the map of them. Readers that
    /// store records encode the values straight into the key they look up.
    fn visit_primary_key_values(
        &self,
        table: &str,
        schema: &TableDefinition,
        values: &[&Value],
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let mut key = Row::new();
        for (column, value) in schema.primary_key.iter().zip(values) {
            key.insert(column.clone(), (*value).clone());
        }
        self.visit_primary_key(table, &key, visitor)
    }
    /// Where `table`'s columns live in its stored records, from a reader whose rows are stored
    /// records, which lets a writer plan rows straight into records. Other readers plan maps.
    /// Every table that has a foreign key, which a reader with no catalog of its own leaves out.
    fn tables_with_foreign_keys(&self) -> Vec<Rc<TableDefinition>> {
        Vec::new()
    }

    fn record_layout(&self, _table: &str) -> Option<Rc<RecordLayout>> {
        None
    }
    /// Whether a delete of a stored row may be planned by its encoded key alone, as a
    /// [`RowChange::Remove`], for a writer that applies it without a map of the key's columns. A
    /// transaction's overlay measures each delete by that map, so only a script's writer does.
    fn plans_removals(&self) -> bool {
        false
    }
    /// Whether a row of `table` holds the encoded primary key `key`, from a reader with record
    /// layouts, charged as a primary-key visit.
    fn holds_encoded_key(&self, _table: &str, _key: &[u8]) -> Result<bool> {
        Err(unplanned_record())
    }
    fn index_definition(&self, name: &str) -> Option<IndexDefinition>;
    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>>;
    fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>>;
    /// Visits, in primary-key order, the rows whose leading indexed column lies within `range`,
    /// using the index on `columns`. Returns `None`, having visited nothing, when the reader cannot
    /// use that index or more than `limit` entries lie within the range.
    fn visit_index_range(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        limit: usize,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        let _ = (table, columns, range, limit, visitor);
        Ok(None)
    }
    /// Visits, in index order, the entries of the index on `columns` whose leading indexed column
    /// lies within `range`, each as a row holding only the columns `layout` places, without
    /// reading the rows themselves. Returns `None`, having visited nothing, when the reader cannot
    /// use that index.
    fn visit_index_entries(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        layout: &IndexEntryLayout,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        let _ = (table, columns, range, layout, visitor);
        Ok(None)
    }
    /// Collecting helper for callers that require all matching index rows.
    #[cfg(test)]
    fn lookup_index(&self, table: &str, columns: &[String], key: &Row) -> Result<Option<Vec<Row>>> {
        let mut rows = Vec::new();
        let outcome = self.visit_index(table, columns, key, &mut |row| {
            rows.push(row.to_row()?);
            Ok(VisitControl::Continue)
        })?;
        Ok(outcome.map(|_| rows))
    }
    /// The table's schema, shared rather than copied.
    fn table_schema(&self, table: &str) -> Result<Rc<TableDefinition>>;
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
                        .get(&*table)
                        .ok_or_else(|| EngineError::table_not_found(&table))?
                        .schema;
                    let row = normalize_row(schema, row)?;
                    let key = row_key(schema, &row)?;
                    (table, key, Some(row))
                }
                RowChange::Delete { table, key } => {
                    let schema = &self
                        .tables
                        .get(&*table)
                        .ok_or_else(|| EngineError::table_not_found(&table))?
                        .schema;
                    validate_primary_key_values(schema, &key)?;
                    let key = row_key(schema, &key)?;
                    (table, key, None)
                }
                RowChange::Put { .. } | RowChange::Remove { .. } => {
                    return Err(unplanned_record());
                }
            };

            let next_bytes = next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
            let charge = key
                .len()
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(next_bytes))
                .and_then(|bytes| bytes.checked_add(96))
                .ok_or_else(row_write_overflow_error)?;
            let table_changes = tables.entry(table.to_string()).or_default();
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
    fn tables_with_foreign_keys(&self) -> Vec<Rc<TableDefinition>> {
        self.tables
            .values()
            .filter(|table| !table.schema.foreign_keys.is_empty())
            .map(|table| Rc::new(table.schema.clone()))
            .collect()
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
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
            if visitor(&RowRef::map(row, &table.schema))? == VisitControl::Stop {
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
            rows.push(row.to_row()?);
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
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
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
            if visitor(&RowRef::map(row, &table_data.schema))? == VisitControl::Stop {
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
            rows.push(row.to_row()?);
            Ok(VisitControl::Continue)
        })?;
        Ok(outcome.map(|_| rows))
    }

    fn table_schema(&self, table: &str) -> Result<Rc<TableDefinition>> {
        self.tables
            .get(table)
            .map(|table| Rc::new(table.schema.clone()))
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
    let mut seen = KeySet::default();
    for name in &definition.columns {
        validate_catalog_name_bound(name)
            .map_err(|error| EngineError::invalid_schema(error.message))?;
        if !seen.insert(name.as_str()) {
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
    ensure_storage_key_bytes(bytes)?;
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

    let mut columns = KeySet::default();
    for column in &schema.primary_key {
        validate_catalog_name_bound(column)
            .map_err(|error| EngineError::invalid_schema(error.message))?;
        if column.trim().is_empty() {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` contains an empty primary-key column",
                schema.name
            )));
        }
        if !columns.insert(column.as_str()) {
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

    let mut catalog_columns = KeySet::default();
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
    for (position, key) in schema.foreign_keys.iter().enumerate() {
        validate_catalog_name_bound(&key.name)
            .map_err(|error| EngineError::invalid_schema(error.message))?;
        if key.name.is_empty()
            || schema.foreign_keys[..position]
                .iter()
                .any(|previous| previous.name == key.name)
        {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` needs a distinct name for each foreign key",
                schema.name
            )));
        }
        if key.columns.is_empty() || key.columns.len() != key.referenced_columns.len() {
            return Err(EngineError::invalid_schema(format!(
                "Foreign key `{}` must reference as many columns as it has",
                key.name
            )));
        }
        for column in &key.columns {
            if !catalog_columns.contains(column.as_str()) {
                return Err(EngineError::column_not_found(column, &schema.name));
            }
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

    let mut changed = false;
    for column in &schema.columns {
        // One search finds a column the row names, as nearly every row names them all.
        let named = match row.get_mut(&column.name) {
            Some(value) => {
                changed |= normalize_value(column, value, &schema.name)?;
                true
            }
            None => false,
        };
        if !named {
            let mut value = column.default.clone().unwrap_or(Value::Null);
            normalize_value(column, &mut value, &schema.name)?;
            row.insert(column.name.clone(), value);
            changed = true;
        }
    }
    // Only a default or a respelled FLOAT changes the row's size from the one checked above.
    if changed {
        validate_row_value_limits(&row)
            .map_err(|error| EngineError::invalid_change(error.message))?;
    }
    Ok(row)
}

/// Checks a value `column` can hold and respells a FLOAT, returning whether it did.
fn normalize_value(column: &ColumnDefinition, value: &mut Value, table: &str) -> Result<bool> {
    validate_value(column, value, table)?;
    // A FLOAT is one binary64 number, however its JSON was spelled, so `1` and `1.0` store and
    // compare as the same value.
    if column.data_type == ColumnType::Float && !value.is_null() {
        let float = float_value(value);
        if float != *value {
            *value = float;
            return Ok(true);
        }
    }
    Ok(false)
}

/// A number as the binary64 value a FLOAT column holds. Numbers JavaScript can represent convert
/// exactly; any other value is returned unchanged.
pub(crate) fn float_value(value: &Value) -> Value {
    match value.as_f64().and_then(serde_json::Number::from_f64) {
        Some(number) if !value.is_f64() => Value::Number(number),
        _ => value.clone(),
    }
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
        ColumnType::Text => match (value.as_str(), column.max_length) {
            // A string has no more characters than bytes, so only a long one is counted.
            (Some(text), Some(max))
                if text.len() > max as usize && text.chars().count() > max as usize =>
            {
                return Err(EngineError::constraint_violation(format!(
                    "Column `{}` in `{table}` holds at most {max} characters",
                    column.name
                )));
            }
            (text, _) => text.is_some(),
        },
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

pub(crate) fn column_type_name(data_type: ColumnType) -> &'static str {
    match data_type {
        ColumnType::Boolean => "boolean",
        ColumnType::Integer => "integer",
        ColumnType::Float => "float",
        ColumnType::Text => "text",
        ColumnType::Json => "json",
    }
}

#[cfg(test)]
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
    let mut bytes = 32usize;
    for (key, value) in row {
        bytes = estimated_entry_bytes(bytes, key, value)?;
    }
    Ok(bytes)
}

/// Adds a row's value under `name` to the row's estimated `bytes`.
pub(crate) fn estimated_entry_bytes(bytes: usize, name: &str, value: &Value) -> Result<usize> {
    let bytes = checked_row_write_add(bytes, 64)?;
    let bytes = checked_row_write_add(bytes, checked_row_write_mul(name.len(), 2)?)?;
    checked_row_write_add(
        bytes,
        checked_row_write_mul(estimated_value_bytes_at_depth(value, 1)?, 2)?,
    )
}

/// The most JSON text a row of `schema` can take, apart from its values: braces, commas, and each
/// column's name and colon.
pub(crate) fn row_json_overhead(schema: &TableDefinition) -> Result<usize> {
    let mut bytes = 2usize.saturating_add(schema.columns.len().saturating_sub(1));
    for column in &schema.columns {
        bytes = checked_row_write_add(bytes, encoded_json_string_bytes(&column.name)?)?;
        bytes = checked_row_write_add(bytes, 1)?;
    }
    Ok(bytes)
}

/// An upper bound on a scalar's JSON text, or `None` for an array or object, which only its
/// encoding measures. A number takes at most 25 bytes however it is spelled, and a string at most
/// six per byte, when every byte is escaped, and its quotes.
#[inline]
pub(crate) fn json_scalar_bound(value: &Value) -> Option<usize> {
    match value {
        Value::Null | Value::Bool(_) => Some(5),
        Value::Number(_) => Some(25),
        Value::String(text) => Some(text.len().saturating_mul(6).saturating_add(2)),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// [`estimated_row_bytes`] for the row a stored record decodes to, reading columns in place. The
/// decoded row holds every column of its schema. A boolean, number, or NULL is estimated at 16
/// bytes whatever its value, which the record's layout counts once, so only text and JSON values
/// are read, each adding what it takes beyond that.
pub(crate) fn estimated_record_bytes(record: &StoredRecord<'_>) -> Result<usize> {
    let (estimate, sized) = record.scalar_estimate();
    let mut bytes = estimate.ok_or_else(row_write_overflow_error)?;
    for position in sized {
        let value = match record.schema().columns[*position].data_type {
            // A string takes 24 bytes more than its length, as a NULL takes 16.
            ColumnType::Text => record.text_len(*position)?.map_or(16, |length| 24 + length),
            _ => record
                .column(*position)?
                .owned_bytes(|value| estimated_value_bytes_at_depth(value, 1))?,
        };
        // Every value's estimate is at least a scalar's.
        bytes = checked_row_write_add(bytes, checked_row_write_mul(value - 16, 2)?)?;
    }
    Ok(bytes)
}

/// [`estimated_row_bytes`] for the map of a stored row's primary-key columns, which is what a delete
/// otherwise plans for the row. A key without text takes the same whatever its values, which its
/// layout measured once; a key with text is measured as the map it decodes to.
pub(crate) fn estimated_key_bytes(record: &StoredRecord<'_>) -> Result<usize> {
    match record.key_estimate() {
        Some(bytes) => Ok(bytes),
        None => estimated_row_bytes(&record.key_row()?),
    }
}

pub(crate) fn estimated_value_bytes(value: &Value) -> Result<usize> {
    validate_json_value(value)?;
    estimated_value_bytes_at_depth(value, 0)
}

/// [`estimated_value_bytes`] for a value whose encoded size was already checked, such as a bound
/// parameter.
pub(crate) fn estimated_checked_value_bytes(value: &Value) -> Result<usize> {
    estimated_value_bytes_at_depth(value, 0)
}

fn estimated_value_bytes_at_depth(value: &Value, depth: usize) -> Result<usize> {
    if depth > MAX_JSON_DEPTH {
        return Err(row_write_limit_error(format!(
            "JSON cannot nest more than {MAX_JSON_DEPTH} levels"
        )));
    }
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
        Value::Number(number) => Ok(encoded_json_number_bytes(number)),
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

/// The length of a number as serde_json writes it, without allocating the text.
fn encoded_json_number_bytes(number: &serde_json::Number) -> usize {
    if let Some(value) = number.as_u64() {
        decimal_digits(value)
    } else if let Some(value) = number.as_i64() {
        1 + decimal_digits(value.unsigned_abs())
    } else {
        struct ByteCounter(usize);
        impl std::fmt::Write for ByteCounter {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.0 += text.len();
                Ok(())
            }
        }
        let mut counter = ByteCounter(0);
        std::fmt::Write::write_fmt(&mut counter, format_args!("{number}"))
            .expect("counting formatted bytes cannot fail");
        counter.0
    }
}

fn decimal_digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// The length of a string as JSON text. Only ASCII quotes, backslashes and control characters are
/// escaped, so every other byte of UTF-8, including each byte of a multi-byte character, is
/// written as itself.
fn encoded_json_string_bytes(value: &str) -> Result<usize> {
    let mut escapes = 0usize;
    for byte in value.bytes() {
        match byte {
            b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => escapes += 1,
            0x00..=0x1f => escapes += 5,
            _ => {}
        }
    }
    checked_row_write_add(value.len(), escapes).and_then(|bytes| checked_row_write_add(bytes, 2))
}

pub(crate) fn validate_primary_storage_key_bound(
    schema: &TableDefinition,
    row: &Row,
) -> Result<()> {
    let bytes = storage_tuple_bytes(schema, &schema.primary_key, row, false)?
        .ok_or_else(|| EngineError::invalid_change("A primary key cannot contain null"))?;
    ensure_storage_key_bytes(bytes)
}

/// Whether the primary key of `values`, one for each key column in key order and each a value its
/// column holds, fits a stored key, as [`validate_primary_storage_key_bound`] finds for the map of
/// them: a boolean is encoded in one byte, a number in eight, and text escapes each zero byte and
/// ends with two.
pub(crate) fn primary_key_values_fit(values: &[&Value]) -> bool {
    let mut bytes = 0usize;
    for value in values {
        bytes += match value {
            Value::Bool(_) => 1,
            Value::String(text) => text.len() + text.bytes().filter(|byte| *byte == 0).count() + 2,
            _ => 8,
        };
    }
    bytes <= MAX_STORAGE_KEY_BYTES
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
    ensure_storage_key_bytes(checked_row_write_add(index_bytes, primary_bytes)?)
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
        bytes = checked_row_write_add(bytes, key_component_bytes(schema, name, data_type, value)?)?;
        ensure_storage_key_bytes(bytes)?;
    }
    Ok(Some(bytes))
}

/// The encoded size of one page format 3 key component: fixed-width scalars, and text with each
/// zero byte escaped and a two-byte terminator. JSON cannot be a key column.
fn key_component_bytes(
    schema: &TableDefinition,
    name: &str,
    data_type: ColumnType,
    value: &Value,
) -> Result<usize> {
    match data_type {
        ColumnType::Boolean => Ok(1),
        ColumnType::Integer | ColumnType::Float => Ok(8),
        ColumnType::Text => value
            .as_str()
            .map(|text| text.len() + text.bytes().filter(|byte| *byte == 0).count() + 2)
            .ok_or_else(|| {
                EngineError::type_mismatch(format!(
                    "Key column `{name}` in `{}` expects text",
                    schema.name
                ))
            }),
        ColumnType::Json => Err(EngineError::type_mismatch(format!(
            "JSON column `{name}` in `{}` cannot be part of a key",
            schema.name
        ))),
    }
}

pub(crate) fn ensure_storage_key_bytes(bytes: usize) -> Result<()> {
    if bytes > MAX_STORAGE_KEY_BYTES {
        Err(EngineError::invalid_change(format!(
            "An encoded primary or index key cannot exceed {MAX_STORAGE_KEY_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_catalog_name_bound(name: &str) -> Result<()> {
    if name.len().saturating_add(1) > MAX_STORAGE_KEY_BYTES {
        Err(row_write_limit_error(format!(
            "A catalog name cannot exceed {} UTF-8 bytes",
            MAX_STORAGE_KEY_BYTES - 1
        )))
    } else {
        Ok(())
    }
}

// Each is an addition and a branch in its caller, which estimating a row's bytes runs dozens of
// times: as a call, with the error's closure a second, those were a tenth of the calls a statement
// by key made.
#[inline(always)]
fn checked_row_write_add(left: usize, right: usize) -> Result<usize> {
    match left.checked_add(right) {
        Some(bytes) => Ok(bytes),
        None => Err(row_write_overflow_error()),
    }
}

#[inline(always)]
fn checked_row_write_mul(left: usize, right: usize) -> Result<usize> {
    match left.checked_mul(right) {
        Some(bytes) => Ok(bytes),
        None => Err(row_write_overflow_error()),
    }
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

#[cold]
#[inline(never)]
fn row_write_overflow_error() -> EngineError {
    row_write_limit_error("A row write-set size overflowed".to_owned())
}

fn row_write_limit_error(message: String) -> EngineError {
    EngineError::new("RESOURCE_LIMIT", message)
}

/// Cumulative logical input retained by a disjoint sequence of row write sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowWriteUsage {
    changes: usize,
    bytes: usize,
}

impl RowWriteUsage {
    /// This usage with `other`'s added, failing as [`preflight_row_write_set`] would once either
    /// total passes its limit.
    pub(crate) fn plus(self, other: Self) -> Result<Self> {
        let changes = checked_row_write_add(self.changes, other.changes)?;
        if changes > MAX_ROW_WRITE_CHANGES {
            return Err(row_write_limit_error(format!(
                "A row write-set cannot contain more than {MAX_ROW_WRITE_CHANGES} changes"
            )));
        }
        let bytes = checked_row_write_add(self.bytes, other.bytes)?;
        if bytes > MAX_ROW_WRITE_BYTES {
            return Err(EngineError::new(
                "TRANSACTION_TOO_LARGE",
                format!("A row write-set cannot retain more than {MAX_ROW_WRITE_BYTES} bytes"),
            ));
        }
        Ok(Self { changes, bytes })
    }

    /// This usage without `other`'s, which it includes.
    pub(crate) fn minus(self, other: Self) -> Self {
        Self {
            changes: self.changes - other.changes,
            bytes: self.bytes - other.bytes,
        }
    }
}

/// A table's schema and, where its rows are stored as records, their layout.
pub(crate) type TableShape<'a> = (&'a TableDefinition, Option<&'a RecordLayout>);

pub(crate) fn preflight_row_write_set<'a>(
    changes: &[RowChange],
    tables: &dyn Fn(&str) -> Option<TableShape<'a>>,
    indexes: &[&IndexDefinition],
    previous: RowWriteUsage,
) -> Result<RowWriteUsage> {
    let change_count = previous
        .changes
        .checked_add(changes.len())
        .ok_or_else(row_write_overflow_error)?;
    if change_count > MAX_ROW_WRITE_CHANGES {
        return Err(row_write_limit_error(format!(
            "A row write-set cannot contain more than {MAX_ROW_WRITE_CHANGES} changes"
        )));
    }
    Ok(RowWriteUsage {
        changes: change_count,
        bytes: preflight_row_changes(changes, tables, indexes, previous.bytes)?,
    })
}

fn preflight_row_changes<'a>(
    changes: &[RowChange],
    tables: &dyn Fn(&str) -> Option<TableShape<'a>>,
    indexes: &[&IndexDefinition],
    mut batch_bytes: usize,
) -> Result<usize> {
    // The table the last change named, with its shape: a statement's changes name one table, which
    // is checked and resolved once rather than for every row.
    let mut last: Option<(&str, TableShape<'a>)> = None;
    for change in changes {
        let (RowChange::Upsert { table, .. }
        | RowChange::Delete { table, .. }
        | RowChange::Put { table, .. }
        | RowChange::Remove { table, .. }) = change;
        let (schema, layout) = match last {
            Some((name, shape)) if name == &**table => shape,
            _ => {
                validate_catalog_name_bound(table)
                    .map_err(|error| EngineError::invalid_change(error.message))?;
                let shape = tables(table).ok_or_else(|| EngineError::table_not_found(table))?;
                last = Some((table, shape));
                shape
            }
        };
        let (input, is_delete) = match change {
            RowChange::Upsert { row, .. } => (row, false),
            RowChange::Delete { key, .. } => (key, true),
            RowChange::Put { key, record, .. } => {
                let layout = layout.ok_or_else(unplanned_record)?;
                let record = StoredRecord::new(schema, layout, key, record)?;
                let record_bytes = estimated_record_bytes(&record)?;
                batch_bytes =
                    preflight_record_change(table, &record, record_bytes, indexes, batch_bytes)?;
                continue;
            }
            RowChange::Remove { key, .. } => {
                // Charged as the map of its key columns a delete otherwise plans.
                let layout = layout.ok_or_else(unplanned_record)?;
                let key = StoredRecord::new(schema, layout, key, EMPTY_RECORD)?;
                batch_bytes = charge_row_write(table, estimated_key_bytes(&key)?, batch_bytes)?;
                continue;
            }
        };
        let input_bytes = estimated_row_bytes(input)?;
        batch_bytes = preflight_row_change(
            table,
            schema,
            (input, input_bytes),
            is_delete,
            indexes,
            batch_bytes,
        )?;
    }
    Ok(batch_bytes)
}

/// [`preflight_row_write_set`] for one change to a table the caller resolved: an upsert of
/// `input`, which planning normalized, or a delete of the key `input`. `input_bytes` is the input's
/// [`estimated_row_bytes`].
pub(crate) fn preflight_row_write(
    table: &str,
    schema: &TableDefinition,
    (input, input_bytes): (&Row, usize),
    is_delete: bool,
    indexes: &[&IndexDefinition],
    previous: RowWriteUsage,
) -> Result<RowWriteUsage> {
    let changes = previous
        .changes
        .checked_add(1)
        .ok_or_else(row_write_overflow_error)?;
    if changes > MAX_ROW_WRITE_CHANGES {
        return Err(row_write_limit_error(format!(
            "A row write-set cannot contain more than {MAX_ROW_WRITE_CHANGES} changes"
        )));
    }
    validate_catalog_name_bound(table)
        .map_err(|error| EngineError::invalid_change(error.message))?;
    Ok(RowWriteUsage {
        changes,
        bytes: preflight_row_change(
            table,
            schema,
            (input, input_bytes),
            is_delete,
            indexes,
            previous.bytes,
        )?,
    })
}

/// [`preflight_row_write`] for an upsert planned as its stored record, whose estimated bytes as
/// a map are `record_bytes`. Planning checked its values and size as it would a map's.
pub(crate) fn preflight_record_write(
    table: &str,
    record: &StoredRecord<'_>,
    record_bytes: usize,
    indexes: &[&IndexDefinition],
    previous: RowWriteUsage,
) -> Result<RowWriteUsage> {
    let changes = previous
        .changes
        .checked_add(1)
        .ok_or_else(row_write_overflow_error)?;
    if changes > MAX_ROW_WRITE_CHANGES {
        return Err(row_write_limit_error(format!(
            "A row write-set cannot contain more than {MAX_ROW_WRITE_CHANGES} changes"
        )));
    }
    validate_catalog_name_bound(table)
        .map_err(|error| EngineError::invalid_change(error.message))?;
    Ok(RowWriteUsage {
        changes,
        bytes: preflight_record_change(table, record, record_bytes, indexes, previous.bytes)?,
    })
}

/// [`preflight_row_change`] for an upsert planned as its stored record.
fn preflight_record_change(
    table: &str,
    record: &StoredRecord<'_>,
    record_bytes: usize,
    indexes: &[&IndexDefinition],
    batch_bytes: usize,
) -> Result<usize> {
    validate_record_storage_keys(record, indexes)?;
    let batch_bytes = checked_row_write_add(
        batch_bytes,
        checked_row_write_add(table.len(), checked_row_write_add(record_bytes, 64)?)?,
    )?;
    if batch_bytes > MAX_ROW_WRITE_BYTES {
        return Err(EngineError::new(
            "TRANSACTION_TOO_LARGE",
            format!("A row write-set cannot retain more than {MAX_ROW_WRITE_BYTES} bytes"),
        ));
    }
    Ok(batch_bytes)
}

/// [`validate_prospective_storage_keys`] for a stored record, whose encoded key is its primary-key
/// tuple.
fn validate_record_storage_keys(
    record: &StoredRecord<'_>,
    indexes: &[&IndexDefinition],
) -> Result<()> {
    let primary_bytes = record.key().len();
    ensure_storage_key_bytes(primary_bytes)?;
    validate_index_storage_keys(record.schema(), primary_bytes, indexes, &|position| {
        record.column(position)
    })
}

/// Checks the key each of `indexes` would hold for a row of `schema` whose primary-key tuple
/// takes `primary_bytes`, reading the row's columns by schema position with `column`. A tuple
/// with a null in it is not indexed.
fn validate_index_storage_keys<'a>(
    schema: &TableDefinition,
    primary_bytes: usize,
    indexes: &[&IndexDefinition],
    column: &dyn Fn(usize) -> Result<ValueRef<'a>>,
) -> Result<()> {
    'indexes: for definition in indexes
        .iter()
        .copied()
        .filter(|definition| definition.table == schema.name)
    {
        let mut bytes = 0usize;
        for name in &definition.columns {
            let position = schema
                .columns
                .iter()
                .position(|column| column.name == *name)
                .ok_or_else(|| EngineError::column_not_found(name, &schema.name))?;
            let value = column(position)?;
            if value.is_null() {
                continue 'indexes;
            }
            let data_type = schema.columns[position].data_type;
            let component = match (data_type, &value) {
                (ColumnType::Boolean, _) => 1,
                (ColumnType::Integer | ColumnType::Float, _) => 8,
                (ColumnType::Text, ValueRef::Text(text)) => {
                    text.len() + text.bytes().filter(|byte| *byte == 0).count() + 2
                }
                _ => key_component_bytes(schema, name, data_type, &value.clone().into_value())?,
            };
            bytes = checked_row_write_add(bytes, component)?;
            ensure_storage_key_bytes(bytes)?;
        }
        ensure_storage_key_bytes(checked_row_write_add(bytes, primary_bytes)?)?;
    }
    Ok(())
}

/// The error for a row planned as a record reaching a writer that plans maps.
pub(crate) fn unplanned_record() -> EngineError {
    EngineError::new(
        "INTERNAL_ERROR",
        "A row planned as a stored record reached a writer without record layouts",
    )
}

fn preflight_row_change(
    table: &str,
    schema: &TableDefinition,
    (input, input_bytes): (&Row, usize),
    is_delete: bool,
    indexes: &[&IndexDefinition],
    batch_bytes: usize,
) -> Result<usize> {
    if is_delete {
        // Planning read the key from a stored row, so it holds valid values within their limits.
        debug_assert!(
            input.len() == schema.primary_key.len()
                && validate_primary_key_values(schema, input).is_ok()
        );
    } else {
        // Normalizing the row gave it every column and checked its values and its encoded size
        // against their limits, so only the keys it would add to indexes remain to check.
        debug_assert!(
            schema
                .columns
                .iter()
                .all(|column| input.contains_key(&column.name))
        );
        validate_prospective_storage_keys(schema, input, indexes)?;
    }
    charge_row_write(table, input_bytes, batch_bytes)
}

/// Adds a change to `table` of a row of `input_bytes` to a write set's bytes, failing past their
/// limit.
fn charge_row_write(table: &str, input_bytes: usize, batch_bytes: usize) -> Result<usize> {
    let batch_bytes = checked_row_write_add(
        batch_bytes,
        checked_row_write_add(table.len(), checked_row_write_add(input_bytes, 64)?)?,
    )?;
    if batch_bytes > MAX_ROW_WRITE_BYTES {
        return Err(EngineError::new(
            "TRANSACTION_TOO_LARGE",
            format!("A row write-set cannot retain more than {MAX_ROW_WRITE_BYTES} bytes"),
        ));
    }
    Ok(batch_bytes)
}

fn validate_prospective_storage_keys(
    schema: &TableDefinition,
    input: &Row,
    indexes: &[&IndexDefinition],
) -> Result<()> {
    let primary_bytes = storage_tuple_bytes(schema, &schema.primary_key, input, false)?
        .expect("a primary-key tuple omits no nulls");
    validate_index_storage_keys(schema, primary_bytes, indexes, &|position| {
        let column = &schema.columns[position];
        let value = input
            .get(&column.name)
            .or(column.default.as_ref())
            .unwrap_or(&Value::Null);
        Ok(ValueRef::from_value(value, column.data_type))
    })
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
                        max_length: None,
                    },
                    ColumnDefinition {
                        name: "email".to_owned(),
                        data_type: ColumnType::Text,
                        nullable: true,
                        default: None,
                        max_length: None,
                    },
                ],
                foreign_keys: vec![],
            })
            .unwrap();
        storage
    }

    #[test]
    fn row_write_preflight_preserves_cumulative_count_and_byte_boundaries() {
        let schema = users_storage().table_schema("users").unwrap();
        let tables = |name: &str| (name == "users").then_some((&*schema, None));
        let changes = vec![RowChange::Upsert {
            table: "users".into(),
            row: row(json!({"id": 1, "email": "one"})),
        }];
        let addition =
            preflight_row_write_set(&changes, &tables, &[], RowWriteUsage::default()).unwrap();
        for extra in [0, 1] {
            let count = preflight_row_write_set(
                &changes,
                &tables,
                &[],
                RowWriteUsage {
                    changes: MAX_ROW_WRITE_CHANGES - 1 + extra,
                    bytes: 0,
                },
            );
            let bytes = preflight_row_write_set(
                &changes,
                &tables,
                &[],
                RowWriteUsage {
                    changes: 0,
                    bytes: MAX_ROW_WRITE_BYTES - addition.bytes + extra,
                },
            );
            if extra == 0 {
                assert_eq!(count.unwrap().changes, MAX_ROW_WRITE_CHANGES);
                assert_eq!(bytes.unwrap().bytes, MAX_ROW_WRITE_BYTES);
            } else {
                assert_eq!(count.unwrap_err().code, "RESOURCE_LIMIT");
                assert_eq!(bytes.unwrap_err().code, "TRANSACTION_TOO_LARGE");
            }
        }
    }

    #[test]
    fn table_definitions_require_a_typed_column_catalog() {
        let error = validate_schema(&TableDefinition {
            name: "users".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![],
            foreign_keys: vec![],
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
                table: "users".into(),
                columns: vec!["email".to_owned()],
                unique: true,
            })
            .unwrap();
        storage
            .apply_row_changes_unrevisioned(vec![
                RowChange::Upsert {
                    table: "users".into(),
                    row: row(json!({"id": 1, "email": "one@example.com"})),
                },
                RowChange::Upsert {
                    table: "users".into(),
                    row: row(json!({"id": 2, "email": "two@example.com"})),
                },
            ])
            .unwrap();

        storage
            .apply_row_changes_unrevisioned(vec![
                RowChange::Delete {
                    table: "users".into(),
                    key: row(json!({"id": 1})),
                },
                RowChange::Delete {
                    table: "users".into(),
                    key: row(json!({"id": 2})),
                },
                RowChange::Upsert {
                    table: "users".into(),
                    row: row(json!({"id": 1, "email": "two@example.com"})),
                },
                RowChange::Upsert {
                    table: "users".into(),
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
                table: "users".into(),
                row: row(json!({"id": 1, "email": "kept@example.com"})),
            }])
            .unwrap();

        let before = storage.scan_table("users").unwrap();
        let error = storage
            .apply_row_changes_unrevisioned(vec![
                RowChange::Upsert {
                    table: "users".into(),
                    row: row(json!({"id": 1, "email": "changed@example.com"})),
                },
                RowChange::Upsert {
                    table: "missing".into(),
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
                    table: "users".into(),
                    row: row(json!({"id": 1, "email": null})),
                },
                RowChange::Upsert {
                    table: "users".into(),
                    row: row(json!({"id": 2, "email": null})),
                },
            ])
            .unwrap();

        let mut visited = Vec::new();
        let outcome = storage
            .visit_table("users", &mut |row| {
                visited.push(row.to_row()?["id"].clone());
                Ok(VisitControl::Stop)
            })
            .unwrap();

        assert_eq!(outcome, VisitOutcome::Stopped);
        assert_eq!(visited, vec![json!(1)]);
        assert_eq!(storage.table_row_count("users").unwrap(), 2);
        assert_eq!(storage.visitor_counts(), (1, 0));
    }

    #[test]
    fn encoded_json_lengths_match_serde_json_text() {
        let values = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(7),
            json!(-7),
            json!(u64::MAX),
            json!(i64::MIN),
            json!(9_007_199_254_740_991_i64),
            json!(0.5),
            json!(-0.0),
            json!(1e300),
            json!(-2.5e-8),
            json!(123_456.789),
            json!(""),
            json!("plain"),
            json!("quote \" backslash \\ newline \n tab \t return \r"),
            json!("\u{0000}\u{0001}\u{0008}\u{000c}\u{001f}\u{007f}"),
            json!("caf\u{e9} \u{1f600} \u{4e2d}\u{6587}"),
            json!([1, "two", [3.5, null], {"four": false}]),
            json!({"b": "\"", "a": [-1, 2.25]}),
        ];
        for value in values {
            assert_eq!(
                encoded_json_bytes(&value, 0).unwrap(),
                serde_json::to_string(&value).unwrap().len(),
                "{value}"
            );
        }
    }
}
