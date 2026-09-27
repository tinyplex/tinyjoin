use std::{
    cell::Cell,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
};

use crate::{
    EngineError, IndexDefinition, MAX_CHANGED_KEYS_PER_TABLE, PageDevice, PagedStorage, Result,
    Row, RowChange, StorageReader, TableDefinition, TreeId, VisitControl, VisitOutcome,
    paged_codec::{EMPTY_RECORD, IndexEntryLayout, encode_primary_key, encode_row},
    paged_script::{ChangedRow, TableChanges, held_row_bytes},
    paged_storage::{ChangeCost, PagedTable, PagedWriteUsage},
    row::{HeldRow, RowRef},
    statement::PreviousRow,
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

/// One staged key: the committed row it held and the row the transaction stages for it, kept as
/// the stored entry and the record a commit writes, so that neither is copied or encoded again.
#[derive(Clone)]
struct OverlayEntry {
    row: ChangedRow,
    /// Whether the staged row differs from the committed one, as staging found once, so that
    /// reading the overlay compares no rows.
    changed: bool,
    /// What the entry retains in the overlay.
    retained: usize,
    /// What the entry costs the write set, while its row differs from the committed one.
    cost: Option<ChangeCost>,
}

/// The last of a statement's changes to one key, with the committed row the key holds.
struct PatchChange {
    base: Option<HeldRow>,
    /// The row planning normalized, or for a delete, the key.
    row: Row,
    is_delete: bool,
    /// Whether the statement upserts the key, which it may do only once.
    upserted: bool,
}

#[derive(Default)]
struct OverlayPatch {
    entries: BTreeMap<String, BTreeMap<Vec<u8>, OverlayEntry>>,
    /// Whether any entry replaces one the transaction staged before.
    replaces: bool,
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

    #[cfg(test)]
    pub(crate) fn changes<D: PageDevice>(&self, storage: &PagedStorage<D>) -> Vec<RowChange> {
        changes_from_entries(
            storage,
            self.entries.iter().flat_map(|(table, entries)| {
                entries
                    .iter()
                    .map(move |(key, entry)| (table.as_str(), key.as_slice(), entry))
            }),
        )
    }

    /// The rows this transaction changes, by table and encoded primary key, each with the committed
    /// row it replaces. A row changed back to its committed state is left out.
    pub(crate) fn changed_rows(&self) -> Vec<TableChanges<'_>> {
        let mut changed = Vec::new();
        for (table, entries) in &self.entries {
            let rows = entries
                .iter()
                .filter(|(_, entry)| entry.changed)
                .map(|(key, entry)| (key.as_slice(), &entry.row))
                .collect::<Vec<_>>();
            if !rows.is_empty() {
                changed.push((table.as_str(), rows));
            }
        }
        changed
    }

    /// The primary keys of the rows [`Self::changed_rows`] reports, as a commit reports them.
    pub(crate) fn changed_keys<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
    ) -> Result<BTreeMap<String, Vec<Row>>> {
        let mut keys = Vec::new();
        for (table, entries) in &self.entries {
            let paged = storage.table(table)?;
            // One key more than a table can report shows that it reports none, so no more are read.
            for (key, _) in entries
                .iter()
                .filter(|(_, entry)| entry.changed)
                .take(MAX_CHANGED_KEYS_PER_TABLE + 1)
            {
                keys.push((table.as_str(), key_row(paged, key)?));
            }
        }
        Ok(crate::statement::collect_changed_keys(
            keys.iter().map(|(table, key)| (*table, key)),
        ))
    }

    /// Validates and installs one statement's row delta without exposing a partial statement.
    /// `previous` reports, change by change, the rows planning read; those it lacks are looked up.
    pub(crate) fn stage<D: PageDevice>(
        &mut self,
        storage: &PagedStorage<D>,
        changes: Vec<RowChange>,
        previous: Vec<PreviousRow>,
    ) -> Result<()> {
        self.ensure_base_revision(storage)?;
        if changes.is_empty() {
            return Ok(());
        }
        let patch = self.patch(storage, changes, previous)?;
        let (totals, released, claimed) = self.validate_patch(storage, &patch)?;

        // No fallible validation remains: a failed statement changes neither the staged rows nor
        // the totals.
        for (table, entries) in patch.entries {
            if !self.touched_tables.contains(&table) {
                self.touched_tables.insert(table.clone());
            }
            match self.entries.get_mut(&table) {
                Some(staged) => staged.extend(entries),
                None => {
                    self.entries.insert(table, entries);
                }
            }
        }
        let mut claims = std::mem::take(&mut self.totals.claims);
        for claim in released {
            claims.remove(&claim);
        }
        claims.extend(claimed);
        self.totals = Totals { claims, ..totals };
        Ok(())
    }

    /// The overlay entries a statement's changes produce, each starting from the committed row the
    /// entry it replaces started from, or from the committed row itself. Planning read through
    /// this overlay, so a row it read for a key the overlay does not hold is the committed row.
    fn patch<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        changes: Vec<RowChange>,
        previous: Vec<PreviousRow>,
    ) -> Result<OverlayPatch> {
        let mut patched = BTreeMap::<String, BTreeMap<Vec<u8>, PatchChange>>::new();
        let mut replaces = false;
        let mut previous = previous.into_iter();
        for change in changes {
            let held = previous.next().unwrap_or(PreviousRow::Unread);
            let (table, row, is_delete) = match change {
                RowChange::Upsert { table, row } => (table, row, false),
                RowChange::Delete { table, key } => (table, key, true),
            };
            // A stored row planning held is the row the change's key holds, so its entry's key is
            // the change's encoded key.
            let key = match &held {
                PreviousRow::Read(Some(HeldRow::Stored(entry))) => entry.key().to_vec(),
                _ => encode_primary_key(&storage.table(&table)?.schema, &row)?,
            };
            if !patched.contains_key(&table) {
                patched.insert(table.clone(), BTreeMap::new());
            }
            let entries = patched.get_mut(&table).expect("the table was added above");
            match entries.entry(key) {
                Entry::Occupied(mut slot) => {
                    let slot = slot.get_mut();
                    // Keys are canonical, so this also catches spellings SQL considers equal, such
                    // as FLOAT `0` and `-0.0`. A delete and an upsert of one key stay valid.
                    if !is_delete && slot.upserted {
                        return Err(EngineError::constraint_violation(format!(
                            "SQL statement would write canonical primary key in `{table}` more than once"
                        )));
                    }
                    slot.upserted |= !is_delete;
                    slot.row = row;
                    slot.is_delete = is_delete;
                }
                Entry::Vacant(slot) => {
                    let staged = self
                        .entries
                        .get(&table)
                        .and_then(|entries| entries.get(slot.key()));
                    replaces |= staged.is_some();
                    let base = match (staged, held) {
                        (Some(entry), _) => entry.row.old.clone(),
                        (None, PreviousRow::Read(row)) => row,
                        (None, PreviousRow::Unread) => storage
                            .committed_entry(&table, slot.key())?
                            .map(HeldRow::Stored),
                    };
                    slot.insert(PatchChange {
                        base,
                        row,
                        is_delete,
                        upserted: !is_delete,
                    });
                }
            }
        }
        let mut patch = OverlayPatch {
            replaces,
            ..OverlayPatch::default()
        };
        for (table, changes) in patched {
            let paged = storage.table(&table)?;
            let mut entries = BTreeMap::new();
            for (key, change) in changes {
                let entry = overlay_entry(storage, paged, &table, &key, change)?;
                entries.insert(key, entry);
            }
            patch.entries.insert(table, entries);
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
        patch: &OverlayPatch,
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
        // the way to a smaller final value. A patch of new keys, such as an insert's, has none.
        let mut released = BTreeSet::new();
        let replaced = if patch.replaces {
            patch.entries.iter()
        } else {
            Default::default()
        };
        for (table, entries) in replaced {
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
        for (table, entries) in &patch.entries {
            for entry in entries.values() {
                retain_entry(
                    &mut totals.overlay_keys,
                    &mut totals.overlay_bytes,
                    entry.retained,
                )?;
                if let Some(cost) = &entry.cost {
                    totals.usage = totals.usage.plus(cost.usage)?;
                    match totals.changed_tables.get_mut(table) {
                        Some(count) => *count += 1,
                        None => {
                            totals.changed_tables.insert(table.clone(), 1);
                        }
                    }
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
                let Some(base) = &entry.row.old else {
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

/// The overlay entry a statement's last change to a key leaves: its row encoded as the record a
/// commit writes, whether that differs from the committed row, and what the entry retains and
/// costs.
fn overlay_entry<D: PageDevice>(
    storage: &PagedStorage<D>,
    table: &PagedTable,
    table_name: &str,
    key: &[u8],
    change: PatchChange,
) -> Result<OverlayEntry> {
    let PatchChange {
        base,
        row,
        is_delete,
        ..
    } = change;
    let schema = &table.schema;
    let next = if is_delete {
        None
    } else {
        Some(encode_row(schema, &row)?)
    };
    // Records are canonical, so equal rows have equal records.
    let changed = match (&base, &next) {
        (None, None) => false,
        (Some(HeldRow::Stored(base)), Some(next)) => base.value() != next.as_slice(),
        (Some(HeldRow::Map(base)), Some(next)) => encode_row(schema, base)? != *next,
        _ => true,
    };
    let base_bytes = base
        .as_ref()
        .map_or(Ok(0), |base| held_row_bytes(table, base))?;
    let row_bytes = estimated_row_bytes(&row)?;
    let next_bytes = if is_delete { 0 } else { row_bytes };
    let retained = retained_bytes(table_name, key, base_bytes, next_bytes)?;
    let cost = changed
        .then(|| storage.change_cost(table_name, (&row, row_bytes), is_delete, key, base.as_ref()))
        .transpose()?;
    Ok(OverlayEntry {
        row: ChangedRow { old: base, next },
        changed,
        retained,
        cost,
    })
}

/// The primary-key columns of the encoded key `key` of `table`, as a map.
fn key_row(table: &PagedTable, key: &[u8]) -> Result<Row> {
    table.record(key, EMPTY_RECORD)?.key_row()
}

#[cfg(test)]
#[path = "paged_transaction_validation_tests.rs"]
mod validation_tests;

#[cfg(test)]
fn changes_from_entries<'a, D: PageDevice>(
    storage: &PagedStorage<D>,
    entries: impl Iterator<Item = (&'a str, &'a [u8], &'a OverlayEntry)>,
) -> Vec<RowChange> {
    entries
        .filter(|(_, _, entry)| entry.changed)
        .map(|(table, key, entry)| {
            let paged = storage.table(table).unwrap();
            match &entry.row.next {
                Some(record) => RowChange::Upsert {
                    table: table.to_owned(),
                    row: paged.record(key, record).unwrap().to_row().unwrap(),
                },
                None => RowChange::Delete {
                    table: table.to_owned(),
                    key: key_row(paged, key).unwrap(),
                },
            }
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

/// What an overlay entry retains: its table name and encoded key, the estimated bytes of its
/// committed and staged rows as maps, and a fixed overhead.
fn retained_bytes(
    table: &str,
    encoded_key: &[u8],
    base_bytes: usize,
    next_bytes: usize,
) -> Result<usize> {
    table
        .len()
        .checked_add(encoded_key.len())
        .and_then(|bytes| bytes.checked_add(base_bytes))
        .and_then(|bytes| bytes.checked_add(next_bytes))
        .and_then(|bytes| bytes.checked_add(OVERLAY_ENTRY_BYTES))
        .ok_or_else(transaction_too_large)
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
                .totals
                .changed_tables
                .get(table)
                .is_none_or(|count| *count == 0)
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
        let paged = self.storage.table(table)?;
        // Committed rows and staged entries are both in key order, so the staged entry for each
        // committed row, if any, is found by walking the two together.
        let mut staged = entries.map(|entries| entries.iter().peekable());
        let outcome = self.storage.visit_table(table, &mut |row| {
            self.charge_work(1)?;
            if let Some(staged) = &mut staged {
                let key = row.encoded_key()?;
                // One comparison per row: most committed rows lie before the next staged key.
                while let Some((staged_key, entry)) = staged.peek() {
                    match staged_key.as_slice().cmp(key.as_ref()) {
                        Ordering::Less => {
                            staged.next();
                        }
                        Ordering::Equal if entry.changed => return Ok(VisitControl::Continue),
                        _ => break,
                    }
                }
            }
            visitor(row)
        })?;
        if outcome == VisitOutcome::Stopped {
            return Ok(outcome);
        }
        if let Some(entries) = entries {
            for (key, entry) in entries {
                self.charge_work(1)?;
                if !entry.changed {
                    continue;
                }
                if let Some(record) = &entry.row.next
                    && visitor(&RowRef::record(paged.record(key, record)?))? == VisitControl::Stop
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
                match (entry.row.old.is_some(), entry.row.next.is_some()) {
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
            let paged = self.storage.table(table)?;
            let encoded_key = encode_primary_key(&paged.schema, key)?;
            if let Some(entry) = transaction
                .table_entries(table)
                .and_then(|entries| entries.get(&encoded_key))
            {
                return match &entry.row.next {
                    Some(record) => Ok(Some(paged.record(&encoded_key, record)?.to_row()?)),
                    None => Ok(None),
                };
            }
        }
        self.storage.lookup_primary_key(table, key)
    }

    fn visit_primary_key(
        &self,
        table: &str,
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_base_revision()?;
        self.charge_work(1)?;
        if let Some(transaction) = self.transaction {
            let paged = self.storage.table(table)?;
            let encoded_key = encode_primary_key(&paged.schema, key)?;
            if let Some(entry) = transaction
                .table_entries(table)
                .and_then(|entries| entries.get(&encoded_key))
            {
                return match &entry.row.next {
                    Some(record)
                        if visitor(&RowRef::record(paged.record(&encoded_key, record)?))?
                            == VisitControl::Stop =>
                    {
                        Ok(VisitOutcome::Stopped)
                    }
                    _ => Ok(VisitOutcome::Complete),
                };
            }
        }
        self.storage.visit_primary_key(table, key, visitor)
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

    fn visit_index_entries(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        layout: &IndexEntryLayout,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.ensure_base_revision()?;
        if !self.reads_committed(table) {
            self.storage.table_schema(table)?;
            return Ok(None);
        }
        self.storage
            .visit_index_entries(table, columns, range, layout, &mut |row| {
                self.charge_work(1)?;
                visitor(row)
            })
    }

    fn table_schema(&self, table: &str) -> Result<std::rc::Rc<TableDefinition>> {
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
                        Vec::new(),
                    )
                    .unwrap();
            }
            assert_eq!(storage.validated_row_count() - before, 128);
            let totals = &transaction.totals;
            assert_eq!(totals.overlay_keys, 128);
            let complete = storage
                .validate_row_write_set(&transaction.changes(&storage))
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
                Vec::new(),
            )
            .unwrap();
        transaction
            .stage(
                &storage,
                vec![RowChange::Delete {
                    table: "items".to_owned(),
                    key: row(json!({"id": 3})),
                }],
                Vec::new(),
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
        assert_eq!(transaction.changes(&storage).len(), 2);
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
            transaction
                .stage(&storage, changes, Vec::new())
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );
        assert!(!transaction.is_dirty());
        assert!(transaction.entries.is_empty());
    }

    /// What an entry inserting one small row retains.
    fn entry_bytes() -> usize {
        let next_bytes = estimated_row_bytes(&row(json!({"id": 1, "name": "one"}))).unwrap();
        retained_bytes("items", b"[1]", 0, next_bytes).unwrap()
    }

    #[test]
    fn transaction_key_bound_is_checked_before_retaining_an_extra_entry() {
        let bytes = entry_bytes();
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
        let entry_bytes = entry_bytes();
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
