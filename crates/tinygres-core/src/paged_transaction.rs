use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    Change, EngineError, IndexDefinition, PageDevice, PagedStorage, Result, Row, StorageReader,
    TableSchema, VisitControl, VisitOutcome, paged_codec::encode_primary_key,
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

    pub(crate) fn changes(&self) -> Vec<Change> {
        changes_from_entries(self.entries.iter().flat_map(|(table, entries)| {
            entries.values().map(move |entry| (table.as_str(), entry))
        }))
    }

    /// Validates and installs one statement's row delta without exposing a partial statement.
    pub(crate) fn stage<D: PageDevice>(
        &mut self,
        storage: &PagedStorage<D>,
        changes: Vec<Change>,
    ) -> Result<()> {
        self.ensure_base_revision(storage)?;
        if changes.is_empty() {
            return Ok(());
        }
        // Detect collisions in the statement's original sequence before the overlay's canonical
        // key map can collapse them. Direct change batches retain their documented last-write
        // behavior; this stricter rule is specific to hidden SQL write-sets.
        storage.validate_sql_row_change_sequence(&changes)?;

        let mut patch = OverlayPatch::default();
        for change in changes {
            let (table, input, next) = match change {
                Change::Upsert { table, row } => {
                    let next = Some(row.clone());
                    (table, row, next)
                }
                Change::Delete { table, key } => (table, key, None),
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
                    Some(entry) => entry.clone(),
                    None => {
                        let base = storage.lookup_primary_key(&table, &key)?;
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

        self.validate_candidate_overlay(&patch)?;
        let candidate_changes = self.candidate_changes(&patch);
        // This performs the canonical physical row/key, unique-index, operation-count, and
        // retained-byte validation against the complete transaction final state.
        storage.validate_row_write_set(&candidate_changes)?;

        for (table, entries) in patch.entries {
            self.touched_tables.insert(table.clone());
            self.entries.entry(table).or_default().extend(entries);
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

    fn candidate_changes(&self, patch: &OverlayPatch) -> Vec<Change> {
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

fn changes_from_entries<'a>(
    entries: impl Iterator<Item = (&'a str, &'a OverlayEntry)>,
) -> Vec<Change> {
    entries
        .filter(|(_, entry)| entry.base != entry.next)
        .map(|(table, entry)| match &entry.next {
            Some(row) => Change::Upsert {
                table: table.to_owned(),
                row: row.clone(),
            },
            None => Change::Delete {
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

fn primary_key_row(schema: &TableSchema, row: &Row) -> Result<Row> {
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
        if self.transaction.is_some() {
            self.storage.table_schema(table)?;
            Ok(Vec::new())
        } else {
            self.storage.indexes_for_table(table)
        }
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

    fn table_schema(&self, table: &str) -> Result<TableSchema> {
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
    use crate::{ColumnDefinition, ColumnType, MemoryPageDevice};

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    fn storage() -> PagedStorage<MemoryPageDevice> {
        let schema = TableSchema {
            name: "items".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![
                ColumnDefinition {
                    name: "id".to_owned(),
                    data_type: ColumnType::Integer,
                    nullable: false,
                    default: None,
                },
                ColumnDefinition {
                    name: "name".to_owned(),
                    data_type: ColumnType::Text,
                    nullable: false,
                    default: None,
                },
            ],
        };
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        storage.define_table(schema.clone()).unwrap();
        storage
            .replace_table_snapshot(
                schema,
                vec![
                    row(json!({"id": 1, "name": "one"})),
                    row(json!({"id": 2, "name": "two"})),
                ],
            )
            .unwrap();
        storage
    }

    #[test]
    fn overlay_reader_merges_updates_inserts_deletes_and_neutral_keys() {
        let storage = storage();
        let mut transaction = PagedTransaction::new(storage.revision());
        transaction
            .stage(
                &storage,
                vec![
                    Change::Delete {
                        table: "items".to_owned(),
                        key: row(json!({"id": 1})),
                    },
                    Change::Upsert {
                        table: "items".to_owned(),
                        row: row(json!({"id": 2, "name": "changed"})),
                    },
                    Change::Upsert {
                        table: "items".to_owned(),
                        row: row(json!({"id": 3, "name": "three"})),
                    },
                ],
            )
            .unwrap();
        transaction
            .stage(
                &storage,
                vec![Change::Delete {
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
            .map(|id| Change::Upsert {
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
