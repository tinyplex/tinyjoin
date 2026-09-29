use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use serde_json::Value;

use crate::{
    ChangedKeys, EngineError, IndexDefinition, MAX_CHANGED_KEYS_PER_TABLE, PageDevice,
    PagedStorage, Result, Row, RowChange, StorageReader, TableDefinition, TableKeys, TreeId,
    VisitControl, VisitOutcome,
    paged_codec::{
        EMPTY_RECORD, IndexEntryLayout, PrimaryKey, RecordLayout, encode_primary_key, encode_row,
    },
    paged_script::{ChangedRow, KeyedRows, TableChanges, held_row_bytes},
    paged_storage::{ChangeCost, ChangeRow, PagedTable, PagedWriteUsage, batch_too_large},
    row::{HeldRow, RowRef},
    statement::PreviousRow,
    storage::{KeyOrder, KeyRange, estimated_record_bytes, estimated_row_bytes, unplanned_record},
};

const MAX_TRANSACTION_KEYS: usize = 100_000;
const MAX_TRANSACTION_BYTES: usize = 16 * 1024 * 1024;
const OVERLAY_ENTRY_BYTES: usize = 128;

/// The bounded, uncommitted row view for one page-native SQL transaction.
#[derive(Clone)]
pub(crate) struct PagedTransaction {
    base_revision: u64,
    entries: BTreeMap<String, BTreeMap<Vec<u8>, OverlayEntry>>,
    /// The tables a statement has changed, in name order.
    touched_tables: Vec<String>,
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
    /// How many changed rows each table a statement has changed has, in name order. A transaction
    /// changes few tables, which a vector holds without a map's code.
    changed_tables: Vec<(String, usize)>,
    /// Each unique-index value a changed row holds, with that row's primary key, or with none
    /// once the row gives it up: a map's code for removing entries is large, so a value given up
    /// is left in place, and the map rebuilt once they are many.
    claims: Claims,
    /// How many values were given up since the claims were last rebuilt.
    released: usize,
}

/// Unique-index values, each with the primary key of the row claiming it, if one does.
type Claims = BTreeMap<(TreeId, Box<[u8]>), Option<Vec<u8>>>;

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
    row: PatchRow,
    /// Whether the statement upserts the key, which it may do only once.
    upserted: bool,
}

/// A statement's change to one key, as planning made it.
enum PatchRow {
    /// A row planning normalized as a map.
    Map(Row),
    /// A row planning encoded as its record.
    Record(Vec<u8>),
    /// A delete, of the key as a map.
    Delete(Row),
}

/// A table's entries in a patch, in key order.
type PatchEntries = Vec<(Vec<u8>, OverlayEntry)>;

/// A statement's overlay entries: each table it changes, with its entries in key order. A
/// statement changes a table or two, and most change a row or two, so vectors hold them without
/// the nodes a map would allocate.
#[derive(Default)]
struct OverlayPatch {
    entries: Vec<(String, PatchEntries)>,
    /// Whether any entry replaces one the transaction staged before.
    replaces: bool,
}

impl OverlayPatch {
    fn entry(&self, table: &str, key: &[u8]) -> Option<&OverlayEntry> {
        let (_, entries) = self.entries.iter().find(|(name, _)| name == table)?;
        let index = entries
            .binary_search_by(|(entry, _)| entry.as_slice().cmp(key))
            .ok()?;
        Some(&entries[index].1)
    }
}

/// What a validated patch changes in the transaction's totals.
struct TotalsChange {
    overlay_keys: usize,
    overlay_bytes: usize,
    usage: PagedWriteUsage,
    /// How many changed rows each of the patch's tables gains, or loses, in patch order, as a
    /// two's-complement change to its count.
    changed_rows: Vec<usize>,
    /// The unique-index claims the patch gives up.
    released: BTreeSet<(TreeId, Box<[u8]>)>,
    /// The claims it makes, each with the primary key that makes it.
    claimed: Claims,
}

impl PagedTransaction {
    pub(crate) fn new(base_revision: u64) -> Self {
        Self {
            base_revision,
            entries: BTreeMap::new(),
            touched_tables: Vec::new(),
            totals: Totals::default(),
        }
    }

    pub(crate) fn base_revision(&self) -> u64 {
        self.base_revision
    }

    pub(crate) fn is_dirty(&self) -> bool {
        !self.touched_tables.is_empty()
    }

    pub(crate) fn touched_tables(&self) -> Vec<String> {
        self.touched_tables.clone()
    }

    /// Records that a statement changed `table`.
    fn touch(&mut self, table: &str) {
        let position = self
            .touched_tables
            .partition_point(|touched| touched.as_str() < table);
        if self
            .touched_tables
            .get(position)
            .is_none_or(|touched| touched != table)
        {
            self.touched_tables.insert(position, table.to_owned());
        }
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

    /// The primary keys of the rows [`Self::changed_rows`] reports, as a commit reports them: a
    /// table's entries are kept once each in the order of their encoded keys, the order a statement
    /// reports its keys in.
    pub(crate) fn changed_keys<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
    ) -> Result<ChangedKeys> {
        let mut keys = ChangedKeys::default();
        for (table, entries) in &self.entries {
            let changed = || entries.iter().filter(|(_, entry)| entry.changed);
            // A table with a key more than it can report reports none, so none is decoded.
            if changed().nth(MAX_CHANGED_KEYS_PER_TABLE).is_some() {
                continue;
            }
            let paged = storage.table(table)?;
            let mut values = Vec::new();
            for (key, _) in changed() {
                paged
                    .record(key, EMPTY_RECORD)?
                    .push_key_values(&mut values)?;
            }
            if !values.is_empty() {
                keys.insert(TableKeys {
                    table: table.clone(),
                    columns: paged.schema.primary_key.clone(),
                    values,
                });
            }
        }
        Ok(keys)
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
        let change = self.validate_patch(storage, &patch)?;

        // No fallible validation remains: a failed statement changes neither the staged rows nor
        // the totals.
        for ((table, entries), changed_rows) in patch.entries.into_iter().zip(change.changed_rows) {
            let count = changed_count(&self.totals.changed_tables, &table, changed_rows);
            match self
                .totals
                .changed_tables
                .iter_mut()
                .find(|(name, _)| *name == table)
            {
                Some((_, counted)) => *counted = count,
                None if count > 0 => {
                    let tables = &mut self.totals.changed_tables;
                    let position = tables.partition_point(|(name, _)| *name < table);
                    tables.insert(position, (table.clone(), count));
                }
                None => {}
            }
            self.touch(&table);
            match self.entries.get_mut(&table) {
                Some(staged) => staged.extend(entries),
                None => {
                    let mut staged = BTreeMap::new();
                    staged.extend(entries);
                    self.entries.insert(table, staged);
                }
            }
        }
        for claim in change.released {
            if let Some(holder) = self.totals.claims.get_mut(&claim) {
                *holder = None;
                self.totals.released += 1;
            }
        }
        self.totals.claims.extend(change.claimed);
        if self.totals.released > 64 && self.totals.released > self.totals.claims.len() / 2 {
            for (slot, holder) in std::mem::take(&mut self.totals.claims) {
                if holder.is_some() {
                    self.totals.claims.insert(slot, holder);
                }
            }
            self.totals.released = 0;
        }
        self.totals.overlay_keys = change.overlay_keys;
        self.totals.overlay_bytes = change.overlay_bytes;
        self.totals.usage = change.usage;
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
        // Each table's changes, in name order: a statement changes one.
        let mut patched: Vec<(String, KeyedRows<PatchChange>)> = Vec::new();
        let mut replaces = false;
        let count = changes.len();
        let mut previous = previous.into_iter();
        for change in changes {
            let held = previous.next().unwrap_or(PreviousRow::Unread);
            let (table, row, encoded) = match change {
                RowChange::Upsert { table, row } => (table, PatchRow::Map(row), None),
                RowChange::Delete { table, key } => (table, PatchRow::Delete(key), None),
                RowChange::Put { table, key, record } => {
                    (table, PatchRow::Record(record), Some(key))
                }
                // A transaction's reader plans deletes as maps, which the overlay measures them by.
                RowChange::Remove { .. } => return Err(unplanned_record()),
            };
            let is_delete = matches!(row, PatchRow::Delete(_));
            // A stored row planning held is the row the change's key holds, so its entry's key is
            // the change's encoded key.
            let key = match (encoded, &held, &row) {
                (Some(key), ..) => key,
                (None, PreviousRow::Read(Some(HeldRow::Stored(entry))), _) => entry.key().to_vec(),
                (None, _, PatchRow::Map(row) | PatchRow::Delete(row)) => {
                    encode_primary_key(&storage.table(&table)?.schema, row)?
                }
                (None, _, PatchRow::Record(_)) => unreachable!("a record comes with its key"),
            };
            // A statement's only change needs nothing merged with it.
            if count == 1 {
                let (change, replaces) =
                    self.first_change(storage, &table, &key, held, row, is_delete)?;
                let entry = overlay_entry(storage, storage.table(&table)?, &table, &key, change)?;
                return Ok(OverlayPatch {
                    entries: vec![(table, vec![(key, entry)])],
                    replaces,
                });
            }
            let position = patched.partition_point(|(name, _)| *name < table);
            if patched.get(position).is_none_or(|(name, _)| *name != table) {
                patched.insert(position, (table.clone(), KeyedRows::default()));
            }
            let entries = &mut patched[position].1;
            match entries.get_mut(&key) {
                Some(slot) => {
                    // Keys are canonical, so this also catches spellings SQL considers equal, such
                    // as FLOAT `0` and `-0.0`. A delete and an upsert of one key stay valid.
                    if !is_delete && slot.upserted {
                        return Err(EngineError::constraint_violation(format!(
                            "SQL statement would write canonical primary key in `{table}` more than once"
                        )));
                    }
                    slot.upserted |= !is_delete;
                    slot.row = row;
                }
                None => {
                    let (change, replaced) =
                        self.first_change(storage, &table, &key, held, row, is_delete)?;
                    replaces |= replaced;
                    entries.insert(key, change);
                }
            }
        }
        let mut patch = OverlayPatch {
            replaces,
            ..OverlayPatch::default()
        };
        for (table, changes) in patched {
            let paged = storage.table(&table)?;
            let changes = changes.into_sorted();
            let mut entries = Vec::with_capacity(changes.len());
            for (key, change) in changes {
                let entry = overlay_entry(storage, paged, &table, &key, change)?;
                entries.push((key, entry));
            }
            patch.entries.push((table, entries));
        }
        Ok(patch)
    }

    /// A statement's first change to `key`, starting from the committed row of the entry it
    /// replaces, or else the row planning read, or else the row the key holds; and whether it
    /// replaces an entry the transaction staged before.
    fn first_change<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        table: &str,
        key: &[u8],
        held: PreviousRow,
        row: PatchRow,
        is_delete: bool,
    ) -> Result<(PatchChange, bool)> {
        let staged = self.entries.get(table).and_then(|entries| entries.get(key));
        let base = match (staged, held) {
            (Some(entry), _) => entry.row.old.clone(),
            (None, PreviousRow::Read(row)) => row,
            (None, PreviousRow::Unread) => {
                storage.committed_entry(table, key)?.map(HeldRow::Stored)
            }
        };
        let change = PatchChange {
            base,
            row,
            upserted: !is_delete,
        };
        Ok((change, staged.is_some()))
    }

    /// Measures a patch's entries and returns the totals with them in place of the entries they
    /// replace, the unique-index claims they give up, and the claims they make. Fails, changing
    /// nothing, if any limit is passed or two rows would hold one unique value.
    ///
    /// A claimed value must not be held by another staged row, nor by a committed row unless the
    /// transaction changes that row, since a changed row holds only the values of its new row. A
    /// row the patch changes back to its committed state holds its committed values again.
    fn validate_patch<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        patch: &OverlayPatch,
    ) -> Result<TotalsChange> {
        let mut overlay_keys = self.totals.overlay_keys;
        let mut overlay_bytes = self.totals.overlay_bytes;
        let mut usage = self.totals.usage;
        let mut changed_rows = vec![0_usize; patch.entries.len()];
        // Take each replaced entry's share out first, so that no total passes its limit only on
        // the way to a smaller final value. A patch of new keys, such as an insert's, has none.
        let mut released = BTreeSet::new();
        if patch.replaces {
            for ((table, entries), changed) in patch.entries.iter().zip(&mut changed_rows) {
                for (key, _) in entries {
                    let Some(previous) =
                        self.entries.get(table).and_then(|entries| entries.get(key))
                    else {
                        continue;
                    };
                    overlay_keys -= 1;
                    overlay_bytes -= previous.retained;
                    if let Some(cost) = &previous.cost {
                        usage = usage.minus(cost.usage);
                        *changed = changed.wrapping_sub(1);
                        released.extend(
                            cost.claims
                                .iter()
                                .map(|claim| (claim.tree_id, claim.prefix.clone())),
                        );
                    }
                }
            }
        }
        for ((_, entries), changed) in patch.entries.iter().zip(&mut changed_rows) {
            for (_, entry) in entries {
                retain_entry(&mut overlay_keys, &mut overlay_bytes, entry.retained)?;
                if let Some(cost) = &entry.cost {
                    usage = usage.plus(cost.usage)?;
                    *changed = changed.wrapping_add(1);
                }
            }
        }

        let mut claimed = Claims::new();
        let holder = |claimed: &Claims, slot: &(TreeId, Box<[u8]>)| -> Option<Vec<u8>> {
            claimed.get(slot).cloned().flatten().or_else(|| {
                (!released.contains(slot))
                    .then(|| self.totals.claims.get(slot).cloned().flatten())
                    .flatten()
            })
        };
        // Whether the transaction changes a row, once the patch is in place.
        let changes_row = |table: &str, key: &[u8]| {
            patch
                .entry(table, key)
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
                    claimed.insert(slot, Some(key.clone()));
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
        // Each table with changed rows once the patch is in place is charged its catalog
        // operations: those the transaction changed, and those the patch changes.
        let mut operations = 0_usize;
        for (table, count) in &self.totals.changed_tables {
            let changed = patch
                .entries
                .iter()
                .zip(&changed_rows)
                .find(|((name, _), _)| name == table)
                .map_or(0, |(_, changed)| *changed);
            if count.wrapping_add(changed) > 0 {
                operations = operations
                    .checked_add(storage.catalog_operations(table))
                    .ok_or_else(batch_too_large)?;
            }
        }
        for ((table, _), changed) in patch.entries.iter().zip(&changed_rows) {
            if table_count(&self.totals.changed_tables, table).is_none() && *changed as isize > 0 {
                operations = operations
                    .checked_add(storage.catalog_operations(table))
                    .ok_or_else(batch_too_large)?;
            }
        }
        usage.with_operations(operations)?;
        Ok(TotalsChange {
            overlay_keys,
            overlay_bytes,
            usage,
            changed_rows,
            released,
            claimed,
        })
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
    let PatchChange { base, row, .. } = change;
    let schema = &table.schema;
    let is_delete = matches!(row, PatchRow::Delete(_));
    // A planned map is encoded here; a planned record already was, and is measured in place.
    let (next, map) = match row {
        PatchRow::Map(row) => (Some(encode_row(schema, &row)?), Some(row)),
        PatchRow::Delete(key) => (None, Some(key)),
        PatchRow::Record(record) => (Some(record), None),
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
    let row_bytes = match (&map, &next) {
        (Some(row), _) => estimated_row_bytes(row)?,
        (None, Some(record)) => estimated_record_bytes(&table.record(key, record)?)?,
        (None, None) => unreachable!("a change without a map is a record"),
    };
    let next_bytes = if is_delete { 0 } else { row_bytes };
    let retained = retained_bytes(table_name, key, base_bytes, next_bytes)?;
    let cost = changed
        .then(|| {
            let row = match (&map, &next) {
                (Some(row), _) => ChangeRow::Map(row, row_bytes),
                (None, Some(record)) => ChangeRow::Record(record, row_bytes),
                (None, None) => unreachable!("a change without a map is a record"),
            };
            storage.change_cost(table_name, row, is_delete, key, base.as_ref())
        })
        .transpose()?;
    Ok(OverlayEntry {
        row: ChangedRow { old: base, next },
        changed,
        retained,
        cost,
    })
}

/// The primary-key columns of the encoded key `key` of `table`, as a map.
#[cfg(test)]
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

/// A table's count of changed rows once `changed`, a two's-complement change, is applied to it.
/// The number of changed rows `counts` records for `table`.
fn table_count(counts: &[(String, usize)], table: &str) -> Option<usize> {
    counts
        .iter()
        .find(|(name, _)| name == table)
        .map(|(_, count)| *count)
}

fn changed_count(counts: &[(String, usize)], table: &str, changed: usize) -> usize {
    let count = table_count(counts, table).unwrap_or(0);
    count
        .checked_add_signed(changed as isize)
        .expect("a changed row's table is counted")
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

    /// Visits the row of primary key `key` in `table`: the row the transaction stages there, if it
    /// stages one, or else the committed row.
    fn visit_key(
        &self,
        table: &str,
        key: PrimaryKey<'_>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_base_revision()?;
        self.charge_work(1)?;
        let paged = self.storage.table(table)?;
        let encoded_key = key.encode(&paged.schema)?;
        if let Some(entry) = self
            .transaction
            .and_then(|transaction| transaction.table_entries(table))
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
        self.storage.visit_encoded_key(table, &encoded_key, visitor)
    }

    /// Whether the committed table is exactly what this view sees, so that its key order and
    /// indexes apply: no transaction has staged a change to it.
    fn reads_committed(&self, table: &str) -> bool {
        self.transaction.is_none_or(|transaction| {
            table_count(&transaction.totals.changed_tables, table).is_none_or(|count| count == 0)
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
        let Some(entries) = self
            .transaction
            .and_then(|transaction| transaction.table_entries(table))
        else {
            // Without a budget to charge, rows go straight to the visitor.
            if self.work.is_none() {
                return self.storage.visit_table(table, visitor);
            }
            return self.storage.visit_table(table, &mut |row| {
                self.charge_work(1)?;
                visitor(row)
            });
        };
        // The committed rows that changed entries replace are passed over, and the entries' rows
        // visited after the rest. Both are in key order, so the scan finds each replaced row once.
        let mut replaced = Vec::new();
        for (key, entry) in entries {
            if entry.changed {
                replaced.push(key.as_slice());
            }
        }
        if self
            .storage
            .visit_table_except(table, &replaced, self.work, visitor)?
            == VisitOutcome::Stopped
        {
            return Ok(VisitOutcome::Stopped);
        }
        let paged = self.storage.table(table)?;
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
        if self.work.is_none() {
            return self.storage.visit_table_range(table, range, order, visitor);
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
        self.visit_key(table, PrimaryKey::Row(key), visitor)
    }

    fn visit_primary_key_values(
        &self,
        table: &str,
        _schema: &TableDefinition,
        values: &[&Value],
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_key(table, PrimaryKey::Values(values), visitor)
    }

    fn record_layout(&self, table: &str) -> Option<Rc<RecordLayout>> {
        self.storage
            .table(table)
            .ok()
            .map(|table| Rc::clone(table.layout()))
    }

    fn holds_encoded_key(&self, table: &str, key: &[u8]) -> Result<bool> {
        self.ensure_base_revision()?;
        self.charge_work(1)?;
        if let Some(entry) = self
            .transaction
            .and_then(|transaction| transaction.table_entries(table))
            .and_then(|entries| entries.get(key))
        {
            return Ok(entry.row.next.is_some());
        }
        Ok(self.storage.committed_entry(table, key)?.is_some())
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
                    .iter()
                    .filter(|(_, holder)| holder.is_some())
                    .map(|(slot, _)| slot.clone())
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
    fn overlay_scans_pass_over_replaced_rows_across_many_leaves() {
        // Even keys are committed, across many leaves; odd ones are only ever staged.
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut statements = vec![
            crate::statement::parse(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
                &[],
            )
            .unwrap(),
        ];
        for id in (2..=3000).step_by(2) {
            statements.push(
                crate::statement::parse(
                    &format!("INSERT INTO items (id, name) VALUES ({id}, 'item {id} of many')"),
                    &[],
                )
                .unwrap(),
            );
        }
        storage.execute_script(statements).unwrap();
        let mut expected = (2..=3000)
            .step_by(2)
            .map(|id| (id, format!("item {id} of many")))
            .collect::<BTreeMap<i64, String>>();
        let mut transaction = PagedTransaction::new(storage.revision());
        let mut changes = Vec::new();
        // Deletes, updates, and upserts of the value a row holds, which change nothing, across
        // the committed keys, including the first and the last; inserts between and around them.
        for id in (0..=3002).step_by(2) {
            let name = if id % 14 == 0 || id == 3000 {
                None
            } else if id % 22 == 0 || id == 2 {
                Some(format!("updated {id}"))
            } else if id % 6 == 0 {
                Some(format!("item {id} of many"))
            } else if id == 0 || id == 3002 {
                Some(format!("new {id}"))
            } else {
                continue;
            };
            changes.push(match &name {
                Some(name) => RowChange::Upsert {
                    table: "items".to_owned(),
                    row: row(json!({"id": id, "name": name})),
                },
                None => RowChange::Delete {
                    table: "items".to_owned(),
                    key: row(json!({"id": id})),
                },
            });
            match name {
                Some(name) => expected.insert(id, name),
                None => expected.remove(&id),
            };
        }
        for id in (1..3000).step_by(98) {
            changes.push(RowChange::Upsert {
                table: "items".to_owned(),
                row: row(json!({"id": id, "name": format!("odd {id}")})),
            });
            expected.insert(id, format!("odd {id}"));
        }
        let staged = changes.len();
        transaction.stage(&storage, changes, Vec::new()).unwrap();

        let work = Cell::new(0);
        for view in [
            PagedReadView::new(&storage, Some(&transaction)),
            PagedReadView::with_work_budget(&storage, Some(&transaction), &work),
        ] {
            let mut visited = Vec::new();
            view.visit_table("items", &mut |row| {
                let row = row.to_row()?;
                visited.push((
                    row["id"].as_i64().unwrap(),
                    row["name"].as_str().unwrap().to_owned(),
                ));
                Ok(VisitControl::Continue)
            })
            .unwrap();
            let mut sorted = visited.clone();
            sorted.sort();
            assert_eq!(sorted, expected.clone().into_iter().collect::<Vec<_>>());
        }
        // Every committed row is charged, whether or not it was passed over, and every entry.
        assert_eq!(work.get(), 1500 + staged);
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
