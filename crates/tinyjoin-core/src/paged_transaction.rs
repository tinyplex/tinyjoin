use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    EngineError, IndexDefinition, PageDevice, PagedStorage, Result, Row, RowChange, StorageReader,
    TableDefinition, TreeId, VisitControl, VisitOutcome,
    paged_codec::encode_primary_key,
    paged_storage::{ChangeCost, PagedWriteUsage},
    row::RowRef,
    storage::{KeyOrder, KeyRange, estimated_row_bytes},
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
    totals: Totals,
}

/// What the overlay retains and what its write set costs, kept up to date statement by statement,
/// so that staging a statement costs only what its own rows cost. Each entry records its own share,
/// which a later statement that replaces the entry takes back out. The totals are exactly what
/// validating the whole write set would find.
#[derive(Clone, Default)]
struct Totals {
    overlay_keys: usize,
    overlay_bytes: usize,
    /// The write set's usage, without the catalog operations it is charged once per table.
    usage: PagedWriteUsage,
    /// How many changed rows each table has.
    changed_tables: BTreeMap<String, usize>,
    /// Each unique-index value a changed row holds, with that row's primary key.
    claims: BTreeMap<(TreeId, Box<[u8]>), Vec<u8>>,
}

#[derive(Clone)]
struct OverlayEntry {
    key: Row,
    base: Option<Row>,
    next: Option<Row>,
    /// What the entry retains in the overlay.
    retained: usize,
    /// What the entry costs the write set, while its row differs from the committed one.
    cost: Option<ChangeCost>,
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
            totals: Totals::default(),
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
        let mut patch = self.patch(storage, changes)?;
        let (totals, released, claimed) = self.validate_patch(storage, &mut patch)?;

        // No fallible validation remains: a failed statement changes neither the staged rows nor
        // the totals.
        for (table, entries) in patch.entries {
            self.touched_tables.insert(table.clone());
            self.entries.entry(table).or_default().extend(entries);
        }
        let mut claims = std::mem::take(&mut self.totals.claims);
        for claim in released {
            claims.remove(&claim);
        }
        claims.extend(claimed);
        self.totals = Totals { claims, ..totals };
        Ok(())
    }

    /// The overlay entries a statement's changes produce, each starting from the entry it
    /// replaces or from the committed row.
    fn patch<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        changes: Vec<RowChange>,
    ) -> Result<OverlayPatch> {
        // Detect collisions in the statement's original sequence before the overlay's canonical
        // key map can collapse them.
        storage.validate_sql_row_change_sequence(&changes)?;
        let mut patch = OverlayPatch::default();
        for change in changes {
            let (table, input, next) = match change {
                RowChange::Upsert { table, row } => {
                    let next = Some(row.clone());
                    (table, row, next)
                }
                RowChange::Delete { table, key } => (table, key, None),
            };
            let schema = storage.table_schema(&table)?;
            let key = primary_key_row(&schema, &input)?;
            let encoded_key = encode_primary_key(&schema, &key)?;
            let entries = patch.entries.entry(table.clone()).or_default();
            if !entries.contains_key(&encoded_key) {
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
                            retained: 0,
                            cost: None,
                        }
                    }
                };
                entries.insert(encoded_key.clone(), entry);
            }
            entries
                .get_mut(&encoded_key)
                .expect("the statement patch entry was installed above")
                .next = next;
        }
        Ok(patch)
    }

    /// Measures a patch's entries and returns the totals with them in place of the entries they
    /// replace, the unique-index claims they give up, and the claims they make. Fails, changing
    /// nothing, if any limit is passed or two rows would hold one unique value.
    ///
    /// A claimed value must not be held by another staged row, nor by a committed row unless the
    /// transaction changes that row, since a changed row holds only the values of its new row. A
    /// row the patch changes back to its committed state holds its committed values again.
    #[allow(clippy::type_complexity)]
    fn validate_patch<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        patch: &mut OverlayPatch,
    ) -> Result<(
        Totals,
        BTreeSet<(TreeId, Box<[u8]>)>,
        BTreeMap<(TreeId, Box<[u8]>), Vec<u8>>,
    )> {
        let mut totals = Totals {
            overlay_keys: self.totals.overlay_keys,
            overlay_bytes: self.totals.overlay_bytes,
            usage: self.totals.usage,
            changed_tables: self.totals.changed_tables.clone(),
            claims: BTreeMap::new(),
        };
        // Take each replaced entry's share out first, so that no total passes its limit only on
        // the way to a smaller final value.
        let mut released = BTreeSet::new();
        for (table, entries) in &patch.entries {
            for key in entries.keys() {
                let Some(previous) = self.entries.get(table).and_then(|entries| entries.get(key))
                else {
                    continue;
                };
                totals.overlay_keys -= 1;
                totals.overlay_bytes -= previous.retained;
                if let Some(cost) = &previous.cost {
                    totals.usage = totals.usage.minus(cost.usage);
                    *totals
                        .changed_tables
                        .get_mut(table)
                        .expect("a changed row's table is counted") -= 1;
                    released.extend(
                        cost.claims
                            .iter()
                            .map(|claim| (claim.tree_id, claim.prefix.clone())),
                    );
                }
            }
        }
        for (table, entries) in &mut patch.entries {
            for (key, entry) in entries.iter_mut() {
                entry.retained = retained_bytes(table, key, entry)?;
                retain_entry(
                    &mut totals.overlay_keys,
                    &mut totals.overlay_bytes,
                    entry.retained,
                )?;
                entry.cost = if entry.base == entry.next {
                    None
                } else {
                    let change = match &entry.next {
                        Some(row) => RowChange::Upsert {
                            table: table.clone(),
                            row: row.clone(),
                        },
                        None => RowChange::Delete {
                            table: table.clone(),
                            key: entry.key.clone(),
                        },
                    };
                    Some(storage.change_cost(table, &change, entry.base.as_ref())?)
                };
                if let Some(cost) = &entry.cost {
                    totals.usage = totals.usage.plus(cost.usage)?;
                    *totals.changed_tables.entry(table.clone()).or_default() += 1;
                }
            }
        }

        let mut claimed = BTreeMap::<(TreeId, Box<[u8]>), Vec<u8>>::new();
        let holder = |claimed: &BTreeMap<(TreeId, Box<[u8]>), Vec<u8>>,
                      slot: &(TreeId, Box<[u8]>)|
         -> Option<Vec<u8>> {
            claimed.get(slot).cloned().or_else(|| {
                (!released.contains(slot))
                    .then(|| self.totals.claims.get(slot).cloned())
                    .flatten()
            })
        };
        // Whether the transaction changes a row, once the patch is in place.
        let changes_row = |table: &str, key: &[u8]| {
            patch
                .entries
                .get(table)
                .and_then(|entries| entries.get(key))
                .or_else(|| self.entries.get(table).and_then(|entries| entries.get(key)))
                .is_some_and(|entry| entry.cost.is_some())
        };
        for (table, entries) in &patch.entries {
            for (key, entry) in entries {
                let Some(cost) = &entry.cost else {
                    continue;
                };
                for claim in &cost.claims {
                    let slot = (claim.tree_id, claim.prefix.clone());
                    if holder(&claimed, &slot).is_some_and(|holder| holder != *key)
                        || claim
                            .owners
                            .iter()
                            .any(|owner| owner != key && !changes_row(table, owner))
                    {
                        return Err(storage.unique_violation_in(claim.tree_id));
                    }
                    claimed.insert(slot, key.clone());
                }
            }
        }
        for (table, entries) in &patch.entries {
            for (key, entry) in entries {
                if entry.cost.is_some() {
                    continue;
                }
                let Some(base) = &entry.base else {
                    continue;
                };
                for (tree_id, prefix) in storage.unique_values(table, base)? {
                    let slot = (tree_id, prefix.into_boxed_slice());
                    if holder(&claimed, &slot).is_some_and(|holder| holder != *key) {
                        return Err(storage.unique_violation_in(tree_id));
                    }
                }
            }
        }
        storage.write_set_usage(
            totals.usage,
            totals
                .changed_tables
                .iter()
                .filter(|(_, count)| **count > 0)
                .map(|(table, _)| table.as_str()),
        )?;
        Ok((totals, released, claimed))
    }

    fn ensure_base_revision<D: PageDevice>(&self, storage: &PagedStorage<D>) -> Result<()> {
        if storage.revision() == self.base_revision {
            Ok(())
        } else {
            Err(write_conflict(self.base_revision, storage.revision()))
        }
    }

    fn table_entries(&self, table: &str) -> Option<&BTreeMap<Vec<u8>, OverlayEntry>> {
        self.entries.get(table)
    }
}

#[cfg(test)]
#[path = "paged_transaction_validation_tests.rs"]
mod validation_tests;

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

/// Adds an entry retaining `bytes` to the overlay's totals, failing once either passes its limit.
fn retain_entry(keys: &mut usize, retained: &mut usize, bytes: usize) -> Result<()> {
    *keys = keys.checked_add(1).ok_or_else(transaction_too_large)?;
    if *keys > MAX_TRANSACTION_KEYS {
        return Err(EngineError::new(
            "TRANSACTION_TOO_LARGE",
            format!("A transaction cannot retain more than {MAX_TRANSACTION_KEYS} row keys"),
        ));
    }
    *retained = retained
        .checked_add(bytes)
        .filter(|retained| *retained <= MAX_TRANSACTION_BYTES)
        .ok_or_else(transaction_too_large)?;
    Ok(())
}

/// What an overlay entry retains: its table name and keys, its rows, and a fixed overhead.
fn retained_bytes(table: &str, encoded_key: &[u8], entry: &OverlayEntry) -> Result<usize> {
    let key_bytes = estimated_row_bytes(&entry.key)?;
    let base_bytes = entry.base.as_ref().map_or(Ok(0), estimated_row_bytes)?;
    let next_bytes = entry.next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
    table
        .len()
        .checked_add(encoded_key.len())
        .and_then(|bytes| bytes.checked_add(key_bytes))
        .and_then(|bytes| bytes.checked_add(base_bytes))
        .and_then(|bytes| bytes.checked_add(next_bytes))
        .and_then(|bytes| bytes.checked_add(OVERLAY_ENTRY_BYTES))
        .ok_or_else(transaction_too_large)
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

    /// Whether the committed table is exactly what this view sees, so that its key order and
    /// indexes apply: no transaction has staged a change to it.
    fn reads_committed(&self, table: &str) -> bool {
        self.transaction.is_none_or(|transaction| {
            transaction
                .table_entries(table)
                .is_none_or(|entries| entries.values().all(|entry| entry.base == entry.next))
        })
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
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
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
            if let Some(entries) = entries
                && entries
                    .get(row.encoded_key()?.as_ref())
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
                    && visitor(&RowRef::map(row, &schema))? == VisitControl::Stop
                {
                    return Ok(VisitOutcome::Stopped);
                }
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn visits_indexes(&self, table: &str) -> bool {
        self.reads_committed(table)
    }

    fn visits_in_key_order(&self, table: &str) -> bool {
        self.reads_committed(table)
    }

    fn visit_table_range(
        &self,
        table: &str,
        range: &KeyRange,
        order: KeyOrder,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_base_revision()?;
        if !self.reads_committed(table) {
            // Staged rows follow the committed ones rather than taking their places in key order,
            // so a transaction which changed the table visits every row.
            return self.visit_table(table, visitor);
        }
        self.storage
            .visit_table_range(table, range, order, &mut |row| {
                self.charge_work(1)?;
                visitor(row)
            })
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
        // Only the postings for a table it changed are stale, which is why index visits then
        // decline.
        self.storage.indexes_for_table(table)
    }

    fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.ensure_base_revision()?;
        if !self.reads_committed(table) {
            self.storage.table_schema(table)?;
            return Ok(None);
        }
        self.storage.visit_index(table, columns, key, &mut |row| {
            self.charge_work(2)?;
            visitor(row)
        })
    }

    fn visit_index_range(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        limit: usize,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.ensure_base_revision()?;
        if !self.reads_committed(table) {
            self.storage.table_schema(table)?;
            return Ok(None);
        }
        self.storage
            .visit_index_range(table, columns, range, limit, &mut |row| {
                self.charge_work(2)?;
                visitor(row)
            })
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
            let totals = &transaction.totals;
            assert_eq!(totals.overlay_keys, 128);
            let complete = storage
                .validate_row_write_set(&transaction.changes())
                .unwrap();
            assert_eq!(
                storage
                    .write_set_usage(totals.usage, ["items"].into_iter())
                    .unwrap(),
                complete.usage
            );
            assert_eq!(
                totals
                    .claims
                    .keys()
                    .cloned()
                    .collect::<crate::paged_storage::UniquePrefixes>(),
                complete.unique_prefixes
            );
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

    fn entry() -> OverlayEntry {
        OverlayEntry {
            key: row(json!({"id": 1})),
            base: None,
            next: Some(row(json!({"id": 1, "name": "one"}))),
            retained: 0,
            cost: None,
        }
    }

    #[test]
    fn transaction_key_bound_is_checked_before_retaining_an_extra_entry() {
        let bytes = retained_bytes("items", b"[1]", &entry()).unwrap();
        let mut key_count = MAX_TRANSACTION_KEYS;
        let mut retained = 0;
        assert_eq!(
            retain_entry(&mut key_count, &mut retained, bytes)
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );
    }

    #[test]
    fn overlay_byte_limit_accepts_the_boundary_and_rejects_one_more_byte() {
        let entry_bytes = retained_bytes("items", b"[1]", &entry()).unwrap();
        let mut keys = 0;
        for extra in [0, 1] {
            let mut bytes = MAX_TRANSACTION_BYTES - entry_bytes + extra;
            let result = retain_entry(&mut keys, &mut bytes, entry_bytes);
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
        // Two probe rows and two key lookups fit, and the two candidate pairs do not.
        let work = Cell::new(crate::sql_script::MAX_SQL_SCRIPT_OPERATIONS - 5);
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
