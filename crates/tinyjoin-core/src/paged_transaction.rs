use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    EngineError, IndexDefinition, PageDevice, PagedStorage, Result, Row, RowChange, StorageReader,
    TableDefinition, VisitControl, VisitOutcome,
    paged_codec::encode_primary_key,
    paged_storage::{AppendWriteContext, PagedWriteUsage, UniquePrefixes},
    storage::estimated_row_bytes,
};

const MAX_TRANSACTION_KEYS: usize = 100_000;
const MAX_TRANSACTION_BYTES: usize = 16 * 1024 * 1024;
const OVERLAY_ENTRY_BYTES: usize = 128;

/// The bounded, uncommitted row view for one page-native SQL transaction.
#[derive(Clone)]
pub(crate) struct PagedTransaction {
    base_revision: u64,
    entries: BTreeMap<String, BTreeMap<Vec<u8>, OverlayEntry>>,
    touched_tables: BTreeSet<String>,
    append_validation: Option<AppendValidation>,
}

/// Disjoint inserts have additive validation costs and never release a unique prefix.
/// Mixed mutations retain the complete existing validator instead of extending this cache
/// into another implementation of update/delete or unique-key movement semantics.
#[derive(Clone, Default)]
struct AppendValidation {
    overlay_keys: usize,
    overlay_bytes: usize,
    usage: PagedWriteUsage,
    unique_prefixes: UniquePrefixes,
}

#[derive(Clone)]
struct OverlayEntry {
    key: Row,
    base: Option<Row>,
    next: Option<Row>,
}

#[derive(Default)]
struct OverlayPatch {
    entries: BTreeMap<String, BTreeMap<Vec<u8>, OverlayEntry>>,
}

impl PagedTransaction {
    pub(crate) fn new(base_revision: u64) -> Self {
        Self {
            base_revision,
            entries: BTreeMap::new(),
            touched_tables: BTreeSet::new(),
            append_validation: Some(AppendValidation::default()),
        }
    }

    pub(crate) fn base_revision(&self) -> u64 {
        self.base_revision
    }

    pub(crate) fn is_dirty(&self) -> bool {
        !self.touched_tables.is_empty()
    }

    pub(crate) fn touched_tables(&self) -> BTreeSet<String> {
        self.touched_tables.clone()
    }

    pub(crate) fn changes(&self) -> Vec<RowChange> {
        changes_from_entries(self.entries.iter().flat_map(|(table, entries)| {
            entries.values().map(move |entry| (table.as_str(), entry))
        }))
    }

    /// Validates and installs one statement's row delta without exposing a partial statement.
    pub(crate) fn stage<D: PageDevice>(
        &mut self,
        storage: &PagedStorage<D>,
        changes: Vec<RowChange>,
    ) -> Result<()> {
        self.ensure_base_revision(storage)?;
        if changes.is_empty() {
            return Ok(());
        }
        // Detect collisions in the statement's original sequence before the overlay's canonical
        // key map can collapse them.
        storage.validate_sql_row_change_sequence(&changes)?;

        let mut patch = OverlayPatch::default();
        let mut append_only = self.append_validation.is_some();
        for change in changes {
            let (table, input, next) = match change {
                RowChange::Upsert { table, row } => {
                    let next = Some(row.clone());
                    (table, row, next)
                }
                RowChange::Delete { table, key } => {
                    append_only = false;
                    (table, key, None)
                }
            };
            let schema = storage.table_schema(&table)?;
            let key = primary_key_row(&schema, &input)?;
            let encoded_key = encode_primary_key(&schema, &key)?;

            if !patch
                .entries
                .get(&table)
                .is_some_and(|entries| entries.contains_key(&encoded_key))
            {
                let entry = match self
                    .entries
                    .get(&table)
                    .and_then(|entries| entries.get(&encoded_key))
                {
                    Some(entry) => {
                        append_only = false;
                        entry.clone()
                    }
                    None => {
                        let base = storage.lookup_primary_key(&table, &key)?;
                        append_only &= base.is_none();
                        OverlayEntry {
                            key,
                            next: base.clone(),
                            base,
                        }
                    }
                };
                patch
                    .entries
                    .entry(table.clone())
                    .or_default()
                    .insert(encoded_key.clone(), entry);
            }
            patch
                .entries
                .get_mut(&table)
                .and_then(|entries| entries.get_mut(&encoded_key))
                .expect("the statement patch entry was installed above")
                .next = next;
        }

        let append = if append_only {
            let previous = self
                .append_validation
                .as_ref()
                .expect("append mode is active");
            let mut keys = previous.overlay_keys;
            let mut bytes = previous.overlay_bytes;
            for (table, entries) in &patch.entries {
                for (key, entry) in entries {
                    retain_entry(table, key, entry, &mut keys, &mut bytes)?;
                }
            }
            let changes =
                changes_from_entries(patch.entries.iter().flat_map(|(table, entries)| {
                    entries.values().map(move |entry| (table.as_str(), entry))
                }));
            let validated = storage.validate_row_write_set(
                &changes,
                Some(AppendWriteContext {
                    usage: previous.usage,
                    unique_prefixes: &previous.unique_prefixes,
                    touched_tables: &self.touched_tables,
                }),
            )?;
            Some((keys, bytes, validated))
        } else {
            self.validate_candidate_overlay(&patch)?;
            let candidate_changes = self.candidate_changes(&patch);
            // Replacements, deletions and committed rows need the complete final-state view.
            storage.validate_row_write_set(&candidate_changes, None)?;
            None
        };

        // No fallible validation remains: failed statements must not change cache eligibility,
        // counters, prefix claims, touched tables, or the staged row view.
        for (table, entries) in patch.entries {
            self.touched_tables.insert(table.clone());
            self.entries.entry(table).or_default().extend(entries);
        }
        if let Some((keys, bytes, validated)) = append {
            let state = self
                .append_validation
                .as_mut()
                .expect("append mode is active");
            state.overlay_keys = keys;
            state.overlay_bytes = bytes;
            state.usage = validated.usage;
            state.unique_prefixes.extend(validated.unique_prefixes);
        } else {
            self.append_validation = None;
        }
        Ok(())
    }

    fn ensure_base_revision<D: PageDevice>(&self, storage: &PagedStorage<D>) -> Result<()> {
        if storage.revision() == self.base_revision {
            Ok(())
        } else {
            Err(write_conflict(self.base_revision, storage.revision()))
        }
    }

    fn validate_candidate_overlay(&self, patch: &OverlayPatch) -> Result<()> {
        let mut key_count = 0usize;
        let mut retained_bytes = 0usize;
        for (table, entries) in &self.entries {
            for (encoded_key, entry) in entries {
                let entry = patch
                    .entries
                    .get(table)
                    .and_then(|entries| entries.get(encoded_key))
                    .unwrap_or(entry);
                retain_entry(
                    table,
                    encoded_key,
                    entry,
                    &mut key_count,
                    &mut retained_bytes,
                )?;
            }
        }
        for (table, entries) in &patch.entries {
            for (encoded_key, entry) in entries {
                if self
                    .entries
                    .get(table)
                    .is_some_and(|entries| entries.contains_key(encoded_key))
                {
                    continue;
                }
                retain_entry(
                    table,
                    encoded_key,
                    entry,
                    &mut key_count,
                    &mut retained_bytes,
                )?;
            }
        }
        Ok(())
    }

    fn candidate_changes(&self, patch: &OverlayPatch) -> Vec<RowChange> {
        let existing = self.entries.iter().flat_map(|(table, entries)| {
            entries.iter().map(move |(encoded_key, entry)| {
                let entry = patch
                    .entries
                    .get(table)
                    .and_then(|entries| entries.get(encoded_key))
                    .unwrap_or(entry);
                (table.as_str(), entry)
            })
        });
        let added = patch.entries.iter().flat_map(|(table, entries)| {
            entries
                .iter()
                .filter(|(encoded_key, _)| {
                    !self
                        .entries
                        .get(table)
                        .is_some_and(|entries| entries.contains_key(*encoded_key))
                })
                .map(move |(_, entry)| (table.as_str(), entry))
        });
        changes_from_entries(existing.chain(added))
    }

    fn table_entries(&self, table: &str) -> Option<&BTreeMap<Vec<u8>, OverlayEntry>> {
        self.entries.get(table)
    }
}

#[cfg(test)]
#[path = "paged_transaction_append_tests.rs"]
mod append_tests;

fn changes_from_entries<'a>(
    entries: impl Iterator<Item = (&'a str, &'a OverlayEntry)>,
) -> Vec<RowChange> {
    entries
        .filter(|(_, entry)| entry.base != entry.next)
        .map(|(table, entry)| match &entry.next {
            Some(row) => RowChange::Upsert {
                table: table.to_owned(),
                row: row.clone(),
            },
            None => RowChange::Delete {
                table: table.to_owned(),
                key: entry.key.clone(),
            },
        })
        .collect()
}

fn retain_entry(
    table: &str,
    encoded_key: &[u8],
    entry: &OverlayEntry,
    key_count: &mut usize,
    retained_bytes: &mut usize,
) -> Result<()> {
    *key_count = key_count.checked_add(1).ok_or_else(transaction_too_large)?;
    if *key_count > MAX_TRANSACTION_KEYS {
        return Err(EngineError::new(
            "TRANSACTION_TOO_LARGE",
            format!("A transaction cannot retain more than {MAX_TRANSACTION_KEYS} row keys"),
        ));
    }
    let key_bytes = estimated_row_bytes(&entry.key)?;
    let base_bytes = entry.base.as_ref().map_or(Ok(0), estimated_row_bytes)?;
    let next_bytes = entry.next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
    *retained_bytes = retained_bytes
        .checked_add(table.len())
        .and_then(|bytes| bytes.checked_add(encoded_key.len()))
        .and_then(|bytes| bytes.checked_add(key_bytes))
        .and_then(|bytes| bytes.checked_add(base_bytes))
        .and_then(|bytes| bytes.checked_add(next_bytes))
        .and_then(|bytes| bytes.checked_add(OVERLAY_ENTRY_BYTES))
        .ok_or_else(transaction_too_large)?;
    if *retained_bytes > MAX_TRANSACTION_BYTES {
        return Err(transaction_too_large());
    }
    Ok(())
}

fn primary_key_row(schema: &TableDefinition, row: &Row) -> Result<Row> {
    schema
        .primary_key
        .iter()
        .map(|column| {
            row.get(column)
                .cloned()
                .map(|value| (column.clone(), value))
                .ok_or_else(|| {
                    EngineError::invalid_change(format!(
                        "Row for `{}` is missing primary-key column `{column}`",
                        schema.name
                    ))
                })
        })
        .collect()
}

fn transaction_too_large() -> EngineError {
    EngineError::new(
        "TRANSACTION_TOO_LARGE",
        format!("A transaction cannot retain more than {MAX_TRANSACTION_BYTES} bytes"),
    )
}

fn write_conflict(expected: u64, actual: u64) -> EngineError {
    EngineError::new(
        "WRITE_CONFLICT",
        format!("Transaction revision {expected} does not match current revision {actual}"),
    )
}

/// A single executor type for both committed and staged page-native reads.
///
/// Using one concrete reader avoids separately monomorphizing every query executor for the base
/// and overlay cases. Secondary indexes are deliberately disabled while staged rows exist: a
/// committed index cannot safely represent uncommitted inserts, deletes, or key moves.
pub(crate) struct PagedReadView<'a, D: PageDevice> {
    storage: &'a PagedStorage<D>,
    transaction: Option<&'a PagedTransaction>,
    work: Option<&'a Cell<usize>>,
}

impl<'a, D: PageDevice> PagedReadView<'a, D> {
    pub(crate) fn new(
        storage: &'a PagedStorage<D>,
        transaction: Option<&'a PagedTransaction>,
    ) -> Self {
        Self {
            storage,
            transaction,
            work: None,
        }
    }

    pub(crate) fn with_work_budget(
        storage: &'a PagedStorage<D>,
        transaction: Option<&'a PagedTransaction>,
        work: &'a Cell<usize>,
    ) -> Self {
        Self {
            storage,
            transaction,
            work: Some(work),
        }
    }

    fn ensure_base_revision(&self) -> Result<()> {
        match self.transaction {
            Some(transaction) => transaction.ensure_base_revision(self.storage),
            None => Ok(()),
        }
    }
}

impl<D: PageDevice> StorageReader for PagedReadView<'_, D> {
    fn charge_work(&self, operations: usize) -> Result<()> {
        match self.work {
            Some(work) => crate::sql_script::charge_operations(work, operations),
            None => Ok(()),
        }
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_base_revision()?;
        let Some(transaction) = self.transaction else {
            return self.storage.visit_table(table, &mut |row| {
                self.charge_work(1)?;
                visitor(row)
            });
        };
        let entries = transaction.table_entries(table);
        let schema = self.storage.table_schema(table)?;
        let outcome = self.storage.visit_table(table, &mut |row| {
            self.charge_work(1)?;
            let encoded_key = encode_primary_key(&schema, row)?;
            if entries
                .and_then(|entries| entries.get(&encoded_key))
                .is_some_and(|entry| entry.base != entry.next)
            {
                return Ok(VisitControl::Continue);
            }
            visitor(row)
        })?;
        if outcome == VisitOutcome::Stopped {
            return Ok(outcome);
        }
        if let Some(entries) = entries {
            for entry in entries.values() {
                self.charge_work(1)?;
                if entry.base == entry.next {
                    continue;
                }
                if let Some(row) = &entry.next
                    && visitor(row)? == VisitControl::Stop
                {
                    return Ok(VisitOutcome::Stopped);
                }
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn table_row_count(&self, table: &str) -> Result<usize> {
        self.ensure_base_revision()?;
        let mut count = self.storage.table_row_count(table)?;
        if let Some(entries) = self
            .transaction
            .and_then(|transaction| transaction.table_entries(table))
        {
            for entry in entries.values() {
                match (entry.base.is_some(), entry.next.is_some()) {
                    (false, true) => {
                        count = count.checked_add(1).ok_or_else(transaction_too_large)?;
                    }
                    (true, false) => {
                        count = count.checked_sub(1).ok_or_else(|| {
                            EngineError::new(
                                "STORAGE_CORRUPT",
                                "A transaction delete exceeds the committed table row count",
                            )
                        })?;
                    }
                    _ => {}
                }
            }
        }
        Ok(count)
    }

    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        self.ensure_base_revision()?;
        self.charge_work(1)?;
        if let Some(transaction) = self.transaction {
            let schema = self.storage.table_schema(table)?;
            let encoded_key = encode_primary_key(&schema, key)?;
            if let Some(entry) = transaction
                .table_entries(table)
                .and_then(|entries| entries.get(&encoded_key))
            {
                return Ok(entry.next.clone());
            }
        }
        self.storage.lookup_primary_key(table, key)
    }

    fn index_definition(&self, name: &str) -> Option<IndexDefinition> {
        self.storage.index_definition(name)
    }

    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>> {
        self.ensure_base_revision()?;
        // A transaction stages rows but never DDL, so the committed definitions stay accurate.
        // Only their postings are stale, which is why `visit_index` declines inside a transaction.
        self.storage.indexes_for_table(table)
    }

    fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.ensure_base_revision()?;
        if self.transaction.is_some() {
            self.storage.table_schema(table)?;
            Ok(None)
        } else {
            self.storage.visit_index(table, columns, key, &mut |row| {
                self.charge_work(2)?;
                visitor(row)
            })
        }
    }

    fn table_schema(&self, table: &str) -> Result<TableDefinition> {
        self.ensure_base_revision()?;
        self.storage.table_schema(table)
    }

    fn revision(&self) -> u64 {
        self.transaction
            .map_or_else(|| self.storage.revision(), PagedTransaction::base_revision)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::MemoryPageDevice;

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    fn storage() -> PagedStorage<MemoryPageDevice> {
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        let statements = [
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            "INSERT INTO items (id, name) VALUES (1, 'one'), (2, 'two')",
        ]
        .into_iter()
        .map(|sql| crate::statement::parse(sql, &[]))
        .collect::<Result<Vec<_>>>()
        .unwrap();
        storage.execute_script(statements).unwrap();
        storage
    }

    #[test]
    fn sequential_appends_validate_each_new_row_once_with_or_without_unique_indexes() {
        for unique in [false, true] {
            let mut storage = storage();
            if unique {
                storage
                    .execute_script(vec![
                        crate::statement::parse(
                            "CREATE UNIQUE INDEX items_name ON items (name)",
                            &[],
                        )
                        .unwrap(),
                    ])
                    .unwrap();
            }
            let mut transaction = PagedTransaction::new(storage.revision());
            let before = storage.validated_row_count();
            for id in 3..131 {
                transaction
                    .stage(
                        &storage,
                        vec![RowChange::Upsert {
                            table: "items".to_owned(),
                            row: row(json!({"id": id, "name": format!("item-{id}")})),
                        }],
                    )
                    .unwrap();
            }
            assert_eq!(storage.validated_row_count() - before, 128);
            let append = transaction.append_validation.as_ref().unwrap();
            assert_eq!(append.overlay_keys, 128);
            let complete = storage
                .validate_row_write_set(&transaction.changes(), None)
                .unwrap();
            assert_eq!(append.usage, complete.usage);
            assert_eq!(append.unique_prefixes, complete.unique_prefixes);
        }
    }

    #[test]
    fn overlay_reader_merges_updates_inserts_deletes_and_neutral_keys() {
        let storage = storage();
        let mut transaction = PagedTransaction::new(storage.revision());
        transaction
            .stage(
                &storage,
                vec![
                    RowChange::Delete {
                        table: "items".to_owned(),
                        key: row(json!({"id": 1})),
                    },
                    RowChange::Upsert {
                        table: "items".to_owned(),
                        row: row(json!({"id": 2, "name": "changed"})),
                    },
                    RowChange::Upsert {
                        table: "items".to_owned(),
                        row: row(json!({"id": 3, "name": "three"})),
                    },
                ],
            )
            .unwrap();
        transaction
            .stage(
                &storage,
                vec![RowChange::Delete {
                    table: "items".to_owned(),
                    key: row(json!({"id": 3})),
                }],
            )
            .unwrap();

        let view = PagedReadView::new(&storage, Some(&transaction));
        assert_eq!(view.table_row_count("items").unwrap(), 1);
        assert!(
            view.lookup_primary_key("items", &row(json!({"id": 1})))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            view.lookup_primary_key("items", &row(json!({"id": 2})))
                .unwrap()
                .unwrap()["name"],
            "changed"
        );
        assert!(transaction.is_dirty());
        assert_eq!(transaction.changes().len(), 2);
    }

    #[test]
    fn oversized_statement_patch_is_rejected_without_installing_any_entry() {
        let storage = storage();
        let mut transaction = PagedTransaction::new(storage.revision());
        let payload = "x".repeat(900_000);
        let changes = (10..20)
            .map(|id| RowChange::Upsert {
                table: "items".to_owned(),
                row: row(json!({"id": id, "name": payload})),
            })
            .collect();
        assert_eq!(
            transaction.stage(&storage, changes).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );
        assert!(!transaction.is_dirty());
        assert!(transaction.entries.is_empty());
    }

    #[test]
    fn transaction_key_bound_is_checked_before_retaining_an_extra_entry() {
        let entry = OverlayEntry {
            key: row(json!({"id": 1})),
            base: None,
            next: Some(row(json!({"id": 1, "name": "one"}))),
        };
        let mut key_count = MAX_TRANSACTION_KEYS;
        let mut retained_bytes = 0;
        assert_eq!(
            retain_entry("items", b"[1]", &entry, &mut key_count, &mut retained_bytes,)
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );
    }

    #[test]
    fn overlay_byte_limit_accepts_the_boundary_and_rejects_one_more_byte() {
        let entry = OverlayEntry {
            key: row(json!({"id": 1})),
            base: None,
            next: Some(row(json!({"id": 1, "name": "one"}))),
        };
        let mut keys = 0;
        let mut bytes = 0;
        retain_entry("items", b"[1]", &entry, &mut keys, &mut bytes).unwrap();
        let entry_bytes = bytes;
        for extra in [0, 1] {
            let mut bytes = MAX_TRANSACTION_BYTES - entry_bytes + extra;
            let result = retain_entry("items", b"[1]", &entry, &mut keys, &mut bytes);
            if extra == 0 {
                result.unwrap();
                assert_eq!(bytes, MAX_TRANSACTION_BYTES);
            } else {
                assert_eq!(result.unwrap_err().code, "TRANSACTION_TOO_LARGE");
            }
        }
    }

    #[test]
    fn script_work_budget_counts_join_candidate_pairs_beyond_row_scans() {
        let storage = storage();
        let work = Cell::new(crate::sql_script::MAX_SQL_SCRIPT_OPERATIONS - 7);
        let view = PagedReadView::with_work_budget(&storage, None, &work);
        let plan =
            crate::join::parse_sql("SELECT a.id FROM items a JOIN items b ON a.id = b.id", &[])
                .unwrap();

        assert_eq!(
            crate::join::execute(&view, &plan).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );
        assert_eq!(work.get(), crate::sql_script::MAX_SQL_SCRIPT_OPERATIONS);
    }
}
