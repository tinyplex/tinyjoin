use std::{
    cell::{Cell, Ref, RefCell},
    collections::{BTreeMap, BTreeSet},
    hash::Hasher,
    rc::Rc,
};

use serde_json::Value;

use crate::{
    ChangedKeys, EngineError, IndexDefinition, MAX_CHANGED_KEYS_PER_TABLE, PageDevice,
    PagedStorage, Result, Row, RowChange, StorageReader, TableDefinition, TableKeys, TreeId,
    VisitControl, VisitOutcome,
    hash::KeyHasher,
    name_map::NameMap,
    paged_codec::{
        EMPTY_RECORD, IndexEntryLayout, PrimaryKey, RecordLayout, StoredEntry, encode_primary_key,
        encode_row,
    },
    paged_script::{ChangedRow, KeyedRows, TableChanges, held_row_bytes, sort_keyed},
    paged_storage::{
        ChangeCost, ChangeRow, PagedIndex, PagedTable, PagedWriteUsage, batch_too_large,
    },
    query::Filter,
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
    /// The entries staged for each table a statement has changed, in name order.
    tables: NameMap<TableOverlay>,
    /// The tables a statement has changed, in name order.
    touched_tables: Vec<String>,
    totals: Totals,
    /// The vectors the last statement's patch was built in, for the next statement's.
    room: PatchRoom,
    /// Whether a statement of one change is staged as one of several is, which a test sets to
    /// compare the two ways.
    #[cfg(test)]
    general_only: bool,
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

/// The entries a transaction stages for one table.
///
/// The entries are one vector in arrival order, which an entry never leaves: a later change to
/// its key replaces it in place, since a staged key stays staged for the transaction. A hash
/// index finds a key's position, and a list of the positions in key order serves the walks that
/// need the order: the commit's batches and reported keys, the committed rows a scan passes over
/// and the staged rows it presents after them, and the row count, so that a corrupt table fails
/// it at the same entry however its keys arrived. The list is the arrival order itself while
/// every key arrives above the one before it, as the keys of ascending inserts and upserts do.
/// Once a key arrives below the last, it and the keys after it form a tail behind the positions
/// known to be in key order, and the next ordered walk sorts the tail and merges it in, at a
/// cost that grows with the tail rather than with the table: a transaction that scans between
/// its statements by scattered keys merges a few positions at each scan, rather than sorting
/// every staged key again.
///
/// The memory this takes is one contiguous vector of entries, doubling as it grows, where a
/// B-tree map held them in nodes of about a kilobyte that the allocator reuses: at the
/// transaction's key limit the vector's last doubling briefly holds its old buffer beside its
/// new one.
#[derive(Clone, Default)]
struct TableOverlay {
    /// The staged keys and their entries, in arrival order.
    rows: Vec<(Vec<u8>, OverlayEntry)>,
    /// Where each key is in `rows`.
    index: KeyIndex,
    /// How many entries delete their row. An insert of a key no row was read for can replace a
    /// staged entry only in a table with one, so other tables' inserts skip the search for one.
    deletes: usize,
    /// Each position in `rows` once: the first `sorted` of them in key order, and the rest, the
    /// tail, in arrival order.
    order: RefCell<Vec<u32>>,
    /// How many leading positions of `order` are in key order. Fewer than the rows means a tail
    /// has arrived since the order was last whole, which the next ordered walk merges in.
    sorted: Cell<usize>,
    /// How many tail positions the merges have searched the head for, so that a test can see
    /// that a merge searches for its tail alone.
    #[cfg(test)]
    searches: Cell<usize>,
}

impl TableOverlay {
    /// The entry staged for `key`, if one is.
    fn get(&self, key: &[u8]) -> Option<&OverlayEntry> {
        self.index
            .find(&self.rows, key)
            .ok()
            .map(|position| &self.rows[position].1)
    }

    /// Installs a statement's entries, in key order, each replacing the entry of its key or
    /// joining the rows, and counts the deletes the statement adds to the table, or takes away.
    /// The entries are taken out of `entries`, which keeps its capacity. `found` is where
    /// staging looked for the statement's only entry among the keys, when it did: nothing has
    /// joined the rows since, so the key is not hashed and probed for a second time.
    fn install(&mut self, entries: &mut PatchEntries, deletes: isize, mut found: Option<Probe>) {
        debug_assert!(found.is_none() || entries.len() == 1);
        self.deletes = self.deletes.saturating_add_signed(deletes);
        let order = self.order.get_mut();
        let sorted = self.sorted.get_mut();
        for (key, entry) in entries.drain(..) {
            debug_assert!(found.is_none_or(|found| found == self.index.find(&self.rows, &key)));
            let probe = found
                .take()
                .unwrap_or_else(|| self.index.find(&self.rows, &key));
            let slot = match probe {
                Ok(position) => {
                    // The key keeps its position, and with it its place in the order; the key
                    // the entry arrived with is dropped, as a map drops the key of a value it
                    // replaces.
                    self.rows[position].1 = entry;
                    continue;
                }
                Err(slot) => slot,
            };
            let position = self.rows.len();
            // A key above every key before it keeps a whole order whole; any other key begins
            // the tail, or lengthens it.
            if *sorted == position
                && order
                    .last()
                    .is_none_or(|&last| self.rows[last as usize].0 < key)
            {
                *sorted += 1;
            }
            self.rows.push((key, entry));
            self.index.insert(&self.rows, position, slot);
            // The transaction's key limit keeps every position far below what a slot can hold.
            order.push(position as u32);
        }
    }

    /// Each row's position, in key order.
    ///
    /// A walk may hold the positions while it reads the table again through the same view, as a
    /// visitor can, and that read shares them: a merge needs a tail, which only installing a key
    /// makes, and that needs the transaction mutably, which no live reader of it allows. The test
    /// for a tail reads a cell and a length, so no shared borrow is held into the merge.
    fn ordered(&self) -> Ref<'_, [u32]> {
        if self.sorted.get() < self.rows.len() {
            self.merge_tail();
        }
        Ref::map(self.order.borrow(), Vec::as_slice)
    }

    /// Makes the order whole: sorts the tail by key, each position beside its borrowed key, then
    /// merges it into the head, the positions before it, which are in key order already. Each
    /// tail position is placed by a binary search of the head's keys after the place before it,
    /// and the order is rebuilt in one pass of copies, so a merge compares a key at each step of
    /// each tail position's search and copies a position for each row, where sorting every
    /// position again would compare a key for each row many times over. A tail begins with a key
    /// below the head's last, so a tail is never wholly above the head, and nothing looks for one
    /// that could be appended as it is.
    fn merge_tail(&self) {
        let rows = &self.rows;
        let key_of = |position: u32| rows[position as usize].0.as_slice();
        let mut order = self.order.borrow_mut();
        // The merged order keeps the room this one had, so that the next key to arrive does not
        // copy it again.
        let mut merged = Vec::with_capacity(order.capacity());
        let (head, tail) = order.split_at(self.sorted.get());
        let mut keyed = Vec::with_capacity(tail.len());
        for &position in tail {
            keyed.push((key_of(position), position));
        }
        sort_keyed(&mut keyed);
        let mut copied = 0;
        for (key, position) in keyed {
            #[cfg(test)]
            self.searches.set(self.searches.get() + 1);
            let place = copied + head[copied..].partition_point(|&before| key_of(before) < key);
            merged.extend_from_slice(&head[copied..place]);
            merged.push(position);
            copied = place;
        }
        merged.extend_from_slice(&head[copied..]);
        *order = merged;
        self.sorted.set(rows.len());
    }
}

/// A slot of a [`KeyIndex`] holding no position.
const EMPTY_SLOT: u32 = u32::MAX;

/// Where a table's staged keys are among its rows, found by hashing the key: an open-addressing
/// table of positions, probed linearly and kept at most half full. A staged key is never removed,
/// so the table needs no tombstones, and it keeps no hash beside a position: growing it hashes
/// the keys again from the rows, which costs a transaction a pass over its keys at each doubling.
#[derive(Clone, Default)]
struct KeyIndex {
    /// A power of two of slots, or none, each a position in the rows or [`EMPTY_SLOT`].
    slots: Vec<u32>,
}

impl KeyIndex {
    /// The position of `key` in `rows`, or else the empty slot its probe ended at, where
    /// [`Self::insert`] puts the key if it joins the rows next, as a binary search reports where
    /// a key it misses would go. An index with no slots reports a slot it does not have, which
    /// nothing reads: its first insert grows it.
    fn find(
        &self,
        rows: &[(Vec<u8>, OverlayEntry)],
        key: &[u8],
    ) -> std::result::Result<usize, usize> {
        if self.slots.is_empty() {
            return Err(0);
        }
        let mask = self.slots.len() - 1;
        let mut slot = self.home(key);
        loop {
            let position = self.slots[slot];
            if position == EMPTY_SLOT {
                return Err(slot);
            }
            if rows[position as usize].0.as_slice() == key {
                return Ok(position as usize);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Adds the key at `position`, the last of `rows`, which [`Self::find`] missed, its probe
    /// ending at `slot`. The key is not hashed or probed again: it takes that slot, which nothing
    /// has filled since, unless the rows have reached half the slots, when growing places every
    /// key of the rows, the new one among them, and nothing else is placed.
    fn insert(&mut self, rows: &[(Vec<u8>, OverlayEntry)], position: usize, slot: usize) {
        debug_assert_eq!(position + 1, rows.len());
        if rows.len() * 2 > self.slots.len() {
            self.grow(rows);
        } else {
            debug_assert_eq!(self.slots[slot], EMPTY_SLOT);
            self.slots[slot] = position as u32;
        }
        debug_assert_eq!(self.find(rows, &rows[position].0), Ok(position));
    }

    /// Doubles the slots and places every key of `rows` in them. The slots are allocated afresh
    /// rather than grown in place, since the old ones hold nothing worth a reallocation's copy.
    fn grow(&mut self, rows: &[(Vec<u8>, OverlayEntry)]) {
        self.slots = vec![EMPTY_SLOT; (self.slots.len() * 2).max(16)];
        for (position, (key, _)) in rows.iter().enumerate() {
            self.place(key, position);
        }
    }

    /// Stores `position` in the first free slot from its key's home.
    fn place(&mut self, key: &[u8], position: usize) {
        let mask = self.slots.len() - 1;
        let mut slot = self.home(key);
        while self.slots[slot] != EMPTY_SLOT {
            slot = (slot + 1) & mask;
        }
        self.slots[slot] = position as u32;
    }

    /// The slot `key` hashes to: as many of its hash's top bits as the slots take.
    fn home(&self, key: &[u8]) -> usize {
        (key_hash(key) >> (64 - self.slots.len().trailing_zeros())) as usize
    }
}

/// The hash a [`KeyIndex`] takes a key's slot from: the key's length, then its bytes a word at a
/// time, folded in with the multiply the engine's hash maps use. A word is read little-endian,
/// which WebAssembly loads in one instruction where a big-endian read swaps its bytes one by one.
/// A multiply carries a bit's variation only upward, and keys vary at either end of a word: an
/// integer key varies in its last bytes, which that read puts highest; a float key in its first;
/// a text key in the first bytes of its last word. The folded hash is therefore mixed once more,
/// its high half into its low half and the whole upward again, before its top bits choose the
/// slot.
fn key_hash(key: &[u8]) -> u64 {
    let mut hasher = KeyHasher::default();
    hasher.write_usize(key.len());
    let (words, rest) = key.as_chunks::<8>();
    for word in words {
        hasher.write_u64(u64::from_le_bytes(*word));
    }
    if !rest.is_empty() {
        let mut word = 0;
        for (index, byte) in rest.iter().enumerate() {
            word |= u64::from(*byte) << (8 * index);
        }
        hasher.write_u64(word);
    }
    let folded = hasher.finish();
    hasher.write_u64(folded >> 32);
    hasher.finish()
}

/// The last of a statement's changes to one key, with the committed row the key holds.
struct PatchChange {
    base: Option<HeldRow>,
    row: PatchRow,
    /// Whether the statement upserts the key, which it may do only once.
    upserted: bool,
    /// Whether the change replaces a staged entry that deleted the key.
    replaced_delete: bool,
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

/// Where [`KeyIndex::find`] found a key among a table's staged rows, or else the empty slot its
/// probe ended at, kept from the search a statement's only change makes to the install of its
/// entry.
type Probe = std::result::Result<usize, usize>;

/// The table a patch's last change named: its name, its place in the patch, and the entries the
/// transaction already stages for it.
type LastTable<'a> = (Rc<str>, usize, Option<&'a TableOverlay>);

/// One table's part of a statement's patch.
struct PatchTable {
    table: Rc<str>,
    /// Where the transaction's overlay for the table is among its overlays, or where a new one
    /// goes, as [`NameMap::position`] found it before any of the patch's tables was installed.
    at: std::result::Result<usize, usize>,
    /// The table's entries, in key order.
    entries: PatchEntries,
    /// How many more of the table's staged entries delete their row once the patch is installed.
    deletes: isize,
    /// How many changed rows the table gains, or loses, as a two's-complement change to its
    /// count, which validation finds.
    changed: usize,
}

impl PatchTable {
    /// The entry the patch holds for `key`, if it holds one.
    fn entry(&self, key: &[u8]) -> Option<&OverlayEntry> {
        let index = self
            .entries
            .binary_search_by(|(entry, _)| entry.as_slice().cmp(key))
            .ok()?;
        Some(&self.entries[index].1)
    }
}

/// A statement's overlay entries: each table it changes, with its entries in key order. A
/// statement changes a table or two, and most change a row or two, so vectors hold them without
/// the nodes a map would allocate.
struct OverlayPatch {
    /// The tables the statement changes, in name order.
    tables: Vec<PatchTable>,
    /// The entries vector the transaction kept from its last statement, unless a table of the
    /// patch holds its entries in it.
    spare: PatchEntries,
    /// Whether any entry replaces one the transaction staged before.
    replaces: bool,
    /// Where the patch's only entry was looked for among its table's staged keys, when it was.
    found: Option<Probe>,
}

/// The emptied vectors of the last statement's patch, which the next statement's is built in, so
/// that a statement of one change allocates neither. They hold nothing between statements, and
/// a statement that fails frees them with its patch.
#[derive(Default)]
struct PatchRoom {
    tables: Vec<PatchTable>,
    entries: PatchEntries,
}

impl Clone for PatchRoom {
    /// A copy of a transaction makes its own room with its first statement: the vectors are
    /// empty, so there is nothing of them to copy but their capacity.
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// What a validated patch changes in the transaction's totals.
struct TotalsChange {
    overlay_keys: usize,
    overlay_bytes: usize,
    usage: PagedWriteUsage,
    /// The unique-index claims the patch gives up, if it gives any up: a patch of a table
    /// without a unique index, as most are, builds no set of them.
    released: Option<BTreeSet<(TreeId, Box<[u8]>)>>,
    /// The claims it makes, each with the primary key that makes it, if it makes any.
    claimed: Option<Claims>,
}

impl PagedTransaction {
    pub(crate) fn new(base_revision: u64) -> Self {
        Self {
            base_revision,
            tables: NameMap::new(),
            touched_tables: Vec::new(),
            totals: Totals::default(),
            room: PatchRoom::default(),
            #[cfg(test)]
            general_only: false,
        }
    }

    /// Has every later statement of one change staged as a statement of several is.
    #[cfg(test)]
    fn set_general_only(&mut self) {
        self.general_only = true;
    }

    /// Whether a statement of one change is staged as a statement of several is.
    #[cfg(test)]
    fn general_only(&self) -> bool {
        self.general_only
    }

    #[cfg(not(test))]
    fn general_only(&self) -> bool {
        false
    }

    pub(crate) fn base_revision(&self) -> u64 {
        self.base_revision
    }

    /// The entries staged for `table`, if a statement has changed it.
    fn table(&self, table: &str) -> Option<&TableOverlay> {
        self.tables.get(table)
    }

    /// Installs a validated statement's patch, each table's entries in the overlay the patch
    /// found for it or in a new one where it found none, and returns the patch's emptied vectors
    /// for the next statement's.
    fn install(&mut self, patch: OverlayPatch) -> PatchRoom {
        let OverlayPatch {
            mut tables,
            mut spare,
            mut found,
            ..
        } = patch;
        // The tables are installed last name first. The patch's tables and the overlays are
        // both in name order, so an overlay added for one table goes in at or after the place
        // found for each table still to be installed, and leaves each of those places right.
        while let Some(patched) = tables.pop() {
            let PatchTable {
                table,
                at,
                mut entries,
                deletes,
                ..
            } = patched;
            let overlay = match at {
                Ok(at) => {
                    debug_assert_eq!(
                        self.tables.keys().nth(at).map(String::as_str),
                        Some(&*table)
                    );
                    self.tables
                        .at_mut(at)
                        .expect("a patch's overlay stays where it was found")
                }
                Err(at) => {
                    // A table is touched as its overlay is made, so the two lists name the
                    // same tables.
                    self.touch(&table);
                    self.tables
                        .insert_at(at, String::from(&*table), TableOverlay::default())
                }
            };
            overlay.install(&mut entries, deletes, found.take());
            // The transaction keeps one emptied vector, of no more capacity than a first push
            // gives one, which is a few hundred bytes: a statement of thousands of rows does
            // not leave its vector behind. The vector it takes the place of has no capacity, so
            // forgetting it frees nothing less than dropping it would, without the calls a drop
            // makes to find that out.
            if spare.capacity() == 0 && entries.capacity() <= 4 {
                std::mem::forget(std::mem::replace(&mut spare, entries));
            }
        }
        PatchRoom {
            tables,
            entries: spare,
        }
    }

    pub(crate) fn is_dirty(&self) -> bool {
        !self.touched_tables.is_empty()
    }

    /// Everything the transaction holds, written out for tests that compare two ways of staging
    /// the same statements: each entry's rows, whether it changed the row, what it retains and
    /// costs; the totals; and the tables touched.
    #[cfg(test)]
    pub(crate) fn fingerprint(&self) -> String {
        use std::fmt::Write;
        let mut text = String::new();
        for (table, overlay) in self.tables.iter() {
            for &position in overlay.ordered().iter() {
                let (key, entry) = &overlay.rows[position as usize];
                writeln!(
                    text,
                    "{table} {key:?} old={:?} next={:?} changed={} retained={} cost={:?}",
                    entry.row.old, entry.row.next, entry.changed, entry.retained, entry.cost
                )
                .unwrap();
            }
            writeln!(text, "{table} deletes={}", overlay.deletes).unwrap();
        }
        writeln!(
            text,
            "keys={} bytes={} usage={:?} changed_tables={:?} claims={:?} released={} touched={:?}",
            self.totals.overlay_keys,
            self.totals.overlay_bytes,
            self.totals.usage,
            self.totals.changed_tables,
            self.totals.claims,
            self.totals.released,
            self.touched_tables
        )
        .unwrap();
        text
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
        let mut entries = Vec::new();
        for (table, overlay) in self.tables.iter() {
            for &position in overlay.ordered().iter() {
                let (key, entry) = &overlay.rows[position as usize];
                entries.push((table.as_str(), key.as_slice(), entry));
            }
        }
        changes_from_entries(storage, entries.into_iter())
    }

    /// The rows this transaction changes, by table and encoded primary key, each with the committed
    /// row it replaces. A row changed back to its committed state is left out.
    pub(crate) fn changed_rows(&self) -> Vec<TableChanges<'_>> {
        let mut changed = Vec::new();
        for (table, overlay) in self.tables.iter() {
            let mut rows = Vec::new();
            for &position in overlay.ordered().iter() {
                let (key, entry) = &overlay.rows[position as usize];
                if entry.changed {
                    rows.push((key.as_slice(), &entry.row));
                }
            }
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
        for (table, overlay) in self.tables.iter() {
            let order = overlay.ordered();
            let changed = || {
                order
                    .iter()
                    .map(|&position| &overlay.rows[position as usize])
                    .filter(|(_, entry)| entry.changed)
            };
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
                    schema: Rc::clone(&paged.schema),
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
        // The patch is built in the vectors the last statement's left. A statement that fails
        // returns with them taken, and frees them with its patch.
        let room = std::mem::take(&mut self.room);
        let mut patch = self.patch(storage, changes, previous, room)?;
        let change = self.validate_patch(storage, &mut patch)?;

        // No fallible validation remains: a failed statement changes neither the staged rows nor
        // the totals.
        for patched in &patch.tables {
            let table = &*patched.table;
            let count = changed_count(&self.totals.changed_tables, table, patched.changed);
            match self
                .totals
                .changed_tables
                .iter_mut()
                .find(|(name, _)| name == table)
            {
                Some((_, counted)) => *counted = count,
                None if count > 0 => {
                    let tables = &mut self.totals.changed_tables;
                    let position = tables.partition_point(|(name, _)| name.as_str() < table);
                    tables.insert(position, (table.to_owned(), count));
                }
                None => {}
            }
        }
        let room = self.install(patch);
        // The vectors left in the room's place when it was taken have no capacity, so they are
        // forgotten as the emptied vector above is.
        let taken = std::mem::replace(&mut self.room, room);
        debug_assert!(taken.tables.capacity() == 0 && taken.entries.capacity() == 0);
        std::mem::forget(taken);
        if let Some(released) = change.released {
            for claim in released {
                if let Some(holder) = self.totals.claims.get_mut(&claim) {
                    *holder = None;
                    self.totals.released += 1;
                }
            }
        }
        if let Some(claimed) = change.claimed {
            self.totals.claims.extend(claimed);
        }
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
    /// The patch is built in the vectors of `room`.
    fn patch<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        changes: Vec<RowChange>,
        previous: Vec<PreviousRow>,
        room: PatchRoom,
    ) -> Result<OverlayPatch> {
        let PatchRoom {
            mut tables,
            entries: mut spare,
        } = room;
        debug_assert!(tables.is_empty() && spare.is_empty());
        // Each table's changes, in name order: a statement changes one.
        let mut patched: Vec<(Rc<str>, KeyedRows<PatchChange>)> = Vec::new();
        let mut replaces = false;
        let count = changes.len();
        let mut previous = previous.into_iter();
        // The table the last change named, with its place in `patched` and the entries the
        // transaction already stages for it: a statement's changes name one table, which is
        // looked up once rather than for every row.
        let mut last: Option<LastTable<'_>> = None;
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
            // A statement's only change needs nothing merged with it: its entry goes into the
            // kept vectors, under the change's own name for its table, and the searches made
            // for its overlay and for its key are kept for installing it.
            if count == 1 && !self.general_only() {
                let at = self.tables.position(&table);
                let staged = at.ok().and_then(|at| self.tables.at(at));
                let (change, found) =
                    self.first_change(storage, staged, &table, &key, held, row, is_delete)?;
                let deletes = isize::from(is_delete) - isize::from(change.replaced_delete);
                let paged = storage.table(&table)?;
                let indexes = storage.table_indexes(&table);
                let entry = overlay_entry(storage, paged, &indexes, &key, change)?;
                spare.push((key, entry));
                tables.push(PatchTable {
                    table,
                    at,
                    entries: spare,
                    deletes,
                    changed: 0,
                });
                return Ok(OverlayPatch {
                    tables,
                    spare: Vec::new(),
                    replaces: matches!(found, Some(Ok(_))),
                    found,
                });
            }
            let (position, staged) = match &last {
                Some((name, position, staged)) if *name == table => (*position, *staged),
                _ => {
                    let position = patched.partition_point(|(name, _)| **name < *table);
                    if patched
                        .get(position)
                        .is_none_or(|(name, _)| **name != *table)
                    {
                        patched.insert(position, (Rc::clone(&table), KeyedRows::default()));
                    }
                    let staged = self.table(&table);
                    last = Some((table.clone(), position, staged));
                    (position, staged)
                }
            };
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
                    let (change, found) =
                        self.first_change(storage, staged, &table, &key, held, row, is_delete)?;
                    replaces |= matches!(found, Some(Ok(_)));
                    entries.insert(key, change);
                }
            }
        }
        for (table, changes) in patched {
            let paged = storage.table(&table)?;
            let indexes = storage.table_indexes(&table);
            let changes = changes.into_sorted();
            let mut entries = Vec::with_capacity(changes.len());
            let mut deletes = 0isize;
            for (key, change) in changes {
                deletes += isize::from(matches!(change.row, PatchRow::Delete(_)))
                    - isize::from(change.replaced_delete);
                let entry = overlay_entry(storage, paged, &indexes, &key, change)?;
                entries.push((key, entry));
            }
            let at = self.tables.position(&table);
            tables.push(PatchTable {
                table,
                at,
                entries,
                deletes,
                changed: 0,
            });
        }
        Ok(OverlayPatch {
            tables,
            spare,
            replaces,
            found: None,
        })
    }

    /// A statement's first change to `key`, starting from the committed row of the entry it
    /// replaces, among those the transaction stages for the table, `staged`, or else the row
    /// planning read, or else the row the key holds; and where the key was looked for among the
    /// staged keys, if it was: at the position of the entry the change replaces, or at the empty
    /// slot a key the table does not stage would take.
    #[allow(clippy::too_many_arguments)]
    fn first_change<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        staged: Option<&TableOverlay>,
        table: &str,
        key: &[u8],
        held: PreviousRow,
        row: PatchRow,
        is_delete: bool,
    ) -> Result<(PatchChange, Option<Probe>)> {
        // A key planning read no row for has no live staged entry, which planning would have
        // read, so only a table with staged deletes can hold an entry for it.
        let has_deletes = staged.is_some_and(|overlay| overlay.deletes > 0);
        let unstaged = !is_delete && !has_deletes && matches!(held, PreviousRow::Read(None));
        let (found, replaced) = match staged {
            Some(overlay) if !unstaged => {
                let found = overlay.index.find(&overlay.rows, key);
                let replaced = found.ok().map(|position| &overlay.rows[position].1);
                (Some(found), replaced)
            }
            _ => (None, None),
        };
        let replaced_delete = replaced.is_some_and(|entry| entry.row.next.is_none());
        let base = match (replaced, held) {
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
            replaced_delete,
        };
        Ok((change, found))
    }

    /// Measures a patch's entries and returns the totals with them in place of the entries they
    /// replace, the unique-index claims they give up, and the claims they make. Fails, changing
    /// nothing, if any limit is passed or two rows would hold one unique value.
    ///
    /// A claimed value must not be held by another staged row, nor by a committed row unless the
    /// transaction changes that row, since a changed row holds only the values of its new row. A
    /// row the patch changes back to its committed state holds its committed values again.
    ///
    /// Each of the patch's tables is left with how many changed rows it gains or loses.
    fn validate_patch<D: PageDevice>(
        &self,
        storage: &PagedStorage<D>,
        patch: &mut OverlayPatch,
    ) -> Result<TotalsChange> {
        let mut overlay_keys = self.totals.overlay_keys;
        let mut overlay_bytes = self.totals.overlay_bytes;
        let mut usage = self.totals.usage;
        // The entries the transaction stages for a table of the patch, which the patch found
        // where its overlay is, or found none.
        let staged = |table: &PatchTable| table.at.ok().and_then(|at| self.tables.at(at));
        // Take each replaced entry's share out first, so that no total passes its limit only on
        // the way to a smaller final value. A patch of new keys, such as an insert's, has none.
        let mut released: Option<BTreeSet<(TreeId, Box<[u8]>)>> = None;
        if patch.replaces {
            for table in &mut patch.tables {
                let Some(overlay) = staged(table) else {
                    continue;
                };
                for (key, _) in &table.entries {
                    let Some(previous) = overlay.get(key) else {
                        continue;
                    };
                    overlay_keys -= 1;
                    overlay_bytes -= previous.retained;
                    if let Some(cost) = &previous.cost {
                        usage = usage.minus(cost.usage);
                        table.changed = table.changed.wrapping_sub(1);
                        if !cost.claims.is_empty() {
                            released.get_or_insert_with(BTreeSet::new).extend(
                                cost.claims
                                    .iter()
                                    .map(|claim| (claim.tree_id, claim.prefix.clone())),
                            );
                        }
                    }
                }
            }
        }
        for table in &mut patch.tables {
            for (_, entry) in &table.entries {
                retain_entry(&mut overlay_keys, &mut overlay_bytes, entry.retained)?;
                if let Some(cost) = &entry.cost {
                    usage = usage.plus(cost.usage)?;
                    table.changed = table.changed.wrapping_add(1);
                }
            }
        }

        // The claims the patch makes, in a map made when it makes its first: most patches make
        // none, and give none up. A map or set that was not made is read as an empty one.
        let mut claimed: Option<Claims> = None;
        let holder = |claimed: &Option<Claims>, slot: &(TreeId, Box<[u8]>)| -> Option<Vec<u8>> {
            claimed
                .as_ref()
                .and_then(|claimed| claimed.get(slot))
                .cloned()
                .flatten()
                .or_else(|| {
                    released
                        .as_ref()
                        .is_none_or(|released| !released.contains(slot))
                        .then(|| self.totals.claims.get(slot).cloned().flatten())
                        .flatten()
                })
        };
        // Whether the transaction changes a row, once the patch is in place.
        let changes_row = |table: &PatchTable, key: &[u8]| {
            table
                .entry(key)
                .or_else(|| staged(table).and_then(|overlay| overlay.get(key)))
                .is_some_and(|entry| entry.cost.is_some())
        };
        for table in &patch.tables {
            for (key, entry) in &table.entries {
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
                    claimed
                        .get_or_insert_with(Claims::new)
                        .insert(slot, Some(key.clone()));
                }
            }
        }
        for table in &patch.tables {
            for (key, entry) in &table.entries {
                if entry.cost.is_some() {
                    continue;
                }
                let Some(base) = &entry.row.old else {
                    continue;
                };
                for (tree_id, prefix) in storage.unique_values(&table.table, base)? {
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
                .tables
                .iter()
                .find(|patched| *patched.table == **table)
                .map_or(0, |patched| patched.changed);
            if count.wrapping_add(changed) > 0 {
                operations = operations
                    .checked_add(storage.catalog_operations(table))
                    .ok_or_else(batch_too_large)?;
            }
        }
        for table in &patch.tables {
            if table_count(&self.totals.changed_tables, &table.table).is_none()
                && table.changed as isize > 0
            {
                operations = operations
                    .checked_add(storage.catalog_operations(&table.table))
                    .ok_or_else(batch_too_large)?;
            }
        }
        usage.with_operations(operations)?;
        Ok(TotalsChange {
            overlay_keys,
            overlay_bytes,
            usage,
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
}

/// The overlay entry a statement's last change to a key leaves: its row encoded as the record a
/// commit writes, whether that differs from the committed row, and what the entry retains and
/// costs.
fn overlay_entry<D: PageDevice>(
    storage: &PagedStorage<D>,
    table: &PagedTable,
    indexes: &[&PagedIndex],
    key: &[u8],
    change: PatchChange,
) -> Result<OverlayEntry> {
    let PatchChange { base, row, .. } = change;
    let schema = &table.schema;
    let table_name = schema.name.as_str();
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
            storage.change_cost_in(table, indexes, row, is_delete, key, base.as_ref())
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
                    table: std::rc::Rc::from(table),
                    row: paged.record(key, record).unwrap().to_row().unwrap(),
                },
                None => RowChange::Delete {
                    table: std::rc::Rc::from(table),
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
            .and_then(|transaction| transaction.table(table))
            .and_then(|overlay| overlay.get(&encoded_key))
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
        self.storage.visit_entry(paged, &encoded_key, visitor)
    }

    /// Whether the committed table is exactly what this view sees, so that its key order and
    /// indexes apply: no transaction has staged a change to it.
    /// Visits the table's rows as the transaction sees them: the committed rows, except those the
    /// transaction replaced, and then the rows it staged, all charged to the view's work and to
    /// `scanned`, and each presented only if `filter` accepts it. Within `range` and in `order`
    /// only while the transaction has not changed the table: staged rows follow the committed
    /// ones rather than taking their places in key order, so a transaction which changed the
    /// table visits every row, and the caller's filter still decides membership.
    fn visit_rows_where(
        &self,
        table: &str,
        range: Option<&KeyRange>,
        order: KeyOrder,
        filter: Option<&Filter<'_>>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_base_revision()?;
        let Some(overlay) = self
            .transaction
            .and_then(|transaction| transaction.table(table))
        else {
            return self.storage.visit_rows_where(
                table,
                range,
                order,
                &[],
                self.work,
                filter,
                scanned,
                visitor,
            );
        };
        // The committed rows that changed entries replace are passed over, and the entries' rows
        // visited after the rest. Both are in key order, so the scan finds each replaced row once.
        let mut replaced = Vec::new();
        for &position in overlay.ordered().iter() {
            let (key, entry) = &overlay.rows[position as usize];
            if entry.changed {
                replaced.push(key.as_slice());
            }
        }
        if self.storage.visit_rows_where(
            table,
            None,
            KeyOrder::Ascending,
            &replaced,
            self.work,
            filter,
            scanned,
            visitor,
        )? == VisitOutcome::Stopped
        {
            return Ok(VisitOutcome::Stopped);
        }
        let paged = self.storage.table(table)?;
        // The order is held while the visitor runs, which may read the table again through this
        // view and share it: nothing can stage a key, and so give the order a tail, while a view
        // of the transaction is alive.
        for &position in overlay.ordered().iter() {
            let (key, entry) = &overlay.rows[position as usize];
            self.charge_work(1)?;
            if !entry.changed {
                continue;
            }
            let Some(record) = &entry.row.next else {
                continue;
            };
            scanned(1)?;
            let row = RowRef::record(paged.record(key, record)?);
            if let Some(filter) = filter
                && !filter.matches(&row)?
            {
                continue;
            }
            if visitor(&row)? == VisitControl::Stop {
                return Ok(VisitOutcome::Stopped);
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn reads_committed(&self, table: &str) -> bool {
        self.transaction.is_none_or(|transaction| {
            table_count(&transaction.totals.changed_tables, table).is_none_or(|count| count == 0)
        })
    }
}

impl<D: PageDevice> StorageReader for PagedReadView<'_, D> {
    // A transaction cannot change the catalog, so the storage's is the transaction's.
    fn tables_with_foreign_keys(&self) -> Vec<Rc<TableDefinition>> {
        self.storage.tables_with_foreign_keys()
    }

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
        self.visit_rows_where(
            table,
            None,
            KeyOrder::Ascending,
            None,
            &mut |_| Ok(()),
            visitor,
        )
    }

    fn visit_table_where(
        &self,
        table: &str,
        filter: &Filter<'_>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_rows_where(
            table,
            None,
            KeyOrder::Ascending,
            Some(filter),
            scanned,
            visitor,
        )
    }

    fn visit_table_range_where(
        &self,
        table: &str,
        range: &KeyRange,
        order: KeyOrder,
        filter: &Filter<'_>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_rows_where(table, Some(range), order, Some(filter), scanned, visitor)
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
        self.visit_rows_where(table, Some(range), order, None, &mut |_| Ok(()), visitor)
    }

    fn table_row_count(&self, table: &str) -> Result<usize> {
        self.ensure_base_revision()?;
        let mut count = self.storage.table_row_count(table)?;
        if let Some(overlay) = self
            .transaction
            .and_then(|transaction| transaction.table(table))
        {
            // In key order, so that a count the entries take below zero, which only a corrupt
            // table can, fails at the same entry however the keys arrived.
            for &position in overlay.ordered().iter() {
                let (_, entry) = &overlay.rows[position as usize];
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
                .table(table)
                .and_then(|overlay| overlay.get(&encoded_key))
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
            .and_then(|transaction| transaction.table(table))
            .and_then(|overlay| overlay.get(key))
        {
            return Ok(entry.row.next.is_some());
        }
        Ok(self.storage.committed_entry(table, key)?.is_some())
    }

    // What `visit_key` finds for a visitor that holds the row, by the same tests in the same
    // order: a staged row is copied from the overlay, and a committed one out of its leaf, once.
    fn held_encoded_key(&self, table: &str, key: &[u8]) -> Result<Option<StoredEntry>> {
        self.ensure_base_revision()?;
        self.charge_work(1)?;
        let paged = self.storage.table(table)?;
        if let Some(entry) = self
            .transaction
            .and_then(|transaction| transaction.table(table))
            .and_then(|overlay| overlay.get(key))
        {
            return match &entry.row.next {
                Some(record) => {
                    paged.record(key, record)?;
                    Ok(Some(StoredEntry::new(key, record)))
                }
                None => Ok(None),
            };
        }
        self.storage.committed_entry_of(paged, key)
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

    fn table_has_indexes(&self, table: &str) -> Result<bool> {
        self.ensure_base_revision()?;
        self.storage.table_has_indexes(table)
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
                            table: "items".into(),
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
                        table: "items".into(),
                        key: row(json!({"id": 1})),
                    },
                    RowChange::Upsert {
                        table: "items".into(),
                        row: row(json!({"id": 2, "name": "changed"})),
                    },
                    RowChange::Upsert {
                        table: "items".into(),
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
                    table: "items".into(),
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

    /// The entry a visit of the row whose key has `values` holds, which is how a point statement
    /// read the row it changes before the view answered [`StorageReader::held_encoded_key`]: the
    /// oracle for it.
    fn visited_entry(
        view: &PagedReadView<'_, MemoryPageDevice>,
        table: &str,
        schema: &TableDefinition,
        values: &[&Value],
    ) -> Result<Option<StoredEntry>> {
        let mut held = None;
        view.visit_primary_key_values(table, schema, values, &mut |row| {
            held = match row.hold()? {
                HeldRow::Stored(entry) => Some(entry),
                HeldRow::Map(_) | HeldRow::Measured(_) => {
                    unreachable!("a view of stored records holds their entries")
                }
            };
            Ok(VisitControl::Stop)
        })?;
        Ok(held)
    }

    /// An entry as its key and its record, or the refusal of one as its code and message, so that
    /// two routes to it can be compared.
    type HeldOutcome = std::result::Result<Option<(Vec<u8>, Vec<u8>)>, (String, String)>;

    fn held_outcome(result: Result<Option<StoredEntry>>) -> HeldOutcome {
        match result {
            Ok(entry) => Ok(entry.map(|entry| (entry.key().to_vec(), entry.value().to_vec()))),
            Err(error) => Err((error.code, error.message)),
        }
    }

    #[test]
    fn held_rows_are_the_rows_a_visit_holds() {
        const IDS: i64 = 96;
        let mut seed = 0x243f_6a88_85a3_08d3_u64;
        let mut random = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        // `items` holds committed rows. `spare`, of the same shape, never holds one, so it has no
        // root to look a key up from.
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        storage
            .execute_script(
                ["items", "spare"]
                    .into_iter()
                    .map(|table| {
                        crate::statement::parse(
                            &format!(
                                "CREATE TABLE {table} \
                                 (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT)"
                            ),
                            &[],
                        )
                        .unwrap()
                    })
                    .collect(),
            )
            .unwrap();
        // A row whose note is absent, short, about as long as a leaf holds inline, or longer, so
        // that some records are read from their leaves and others from overflow pages.
        let generated = |id: i64, random: &mut dyn FnMut(u64) -> u64| {
            let note = match random(5) {
                0 => Value::Null,
                1 | 2 => json!("n".repeat(random(40) as usize)),
                3 => json!("b".repeat(980 + random(60) as usize)),
                _ => json!("o".repeat(1_100 + random(1_500) as usize)),
            };
            row(json!({"id": id, "name": format!("item {id} {}", random(1_000)), "note": note}))
        };
        // About half of the keys are committed, over three commits.
        let mut committed = BTreeMap::new();
        for commit in 0..3 {
            let mut statements = Vec::new();
            for id in (0..IDS).filter(|id| id % 3 == commit) {
                if random(2) == 0 {
                    continue;
                }
                let stored = generated(id, &mut random);
                statements.push(
                    crate::statement::parse(
                        "INSERT INTO items (id, name, note) VALUES ($1, $2, $3)",
                        &[
                            stored["id"].clone(),
                            stored["name"].clone(),
                            stored["note"].clone(),
                        ],
                    )
                    .unwrap(),
                );
                committed.insert(id, stored);
            }
            storage.execute_script(statements).unwrap();
        }
        // A transaction inserts, updates and deletes rows of both tables, deletes rows it
        // inserted, and inserts again rows it deleted, by keys in no order.
        let mut transaction = PagedTransaction::new(storage.revision());
        let mut staged: BTreeMap<(&str, i64), Option<Row>> = BTreeMap::new();
        for step in 0..IDS {
            let id = (step * 37 + 11) % IDS;
            for table in ["items", "spare"] {
                let (first, second) = match random(6) {
                    0 | 1 => continue,
                    2 => (Some(generated(id, &mut random)), None),
                    3 => (None, None),
                    4 => (Some(generated(id, &mut random)), Some(None)),
                    _ => (None, Some(Some(generated(id, &mut random)))),
                };
                for next in [Some(first), second].into_iter().flatten() {
                    let change = match &next {
                        Some(next) => RowChange::Upsert {
                            table: table.into(),
                            row: next.clone(),
                        },
                        None => RowChange::Delete {
                            table: table.into(),
                            key: row(json!({"id": id})),
                        },
                    };
                    transaction
                        .stage(&storage, vec![change], Vec::new())
                        .unwrap();
                    staged.insert((table, id), next);
                }
            }
        }

        let work = Cell::new(0);
        // How many keys each view found in each state: committed in a leaf and in overflow pages,
        // staged in place of a committed row, staged where there was none, deleted, and absent.
        let (mut inline, mut overflow, mut updated, mut inserted, mut deleted, mut absent) =
            (0, 0, 0, 0, 0, 0);
        for (overlay, budget) in [(false, false), (true, false), (true, true)] {
            let transaction = overlay.then_some(&transaction);
            let view = if budget {
                PagedReadView::with_work_budget(&storage, transaction, &work)
            } else {
                PagedReadView::new(&storage, transaction)
            };
            for table in ["items", "spare"] {
                let paged = storage.table(table).unwrap();
                assert_eq!(paged.root_page_id.is_some(), table == "items");
                // Every key that could hold a row, and keys beyond both ends of them.
                for id in -2..IDS + 2 {
                    let value = json!(id);
                    let key = PrimaryKey::Values(&[&value]).encode(&paged.schema).unwrap();
                    let charged = work.get();
                    let held = held_outcome(view.held_encoded_key(table, &key));
                    // Each route charges the read as one visit of a key.
                    assert_eq!(work.get() - charged, usize::from(budget));
                    let visited =
                        held_outcome(visited_entry(&view, table, &paged.schema, &[&value]));
                    assert_eq!(work.get() - charged, 2 * usize::from(budget));
                    assert_eq!(held, visited, "{table} {id}");
                    // The entry is the row the view sees there, whichever route found it.
                    let base = committed.get(&id).filter(|_| table == "items");
                    let change = staged.get(&(table, id)).filter(|_| overlay);
                    let expected = match change {
                        Some(next) => next.as_ref(),
                        None => base,
                    };
                    let found = held.unwrap();
                    assert_eq!(
                        found.as_ref().map(|(key, record)| paged
                            .record(key, record)
                            .unwrap()
                            .to_row()
                            .unwrap()),
                        expected.cloned(),
                        "{table} {id}"
                    );
                    match (change, base, &found) {
                        (None, Some(_), Some((_, record)))
                            if record.len() > crate::btree::MAX_BTREE_INLINE_VALUE_BYTES =>
                        {
                            overflow += 1;
                        }
                        (None, Some(_), _) => inline += 1,
                        (Some(Some(_)), Some(_), _) => updated += 1,
                        (Some(Some(_)), None, _) => inserted += 1,
                        (Some(None), ..) => deleted += 1,
                        (None, None, _) => absent += 1,
                    }
                }
            }
        }
        for count in [inline, overflow, updated, inserted, deleted, absent] {
            assert!(count > 10, "{count} keys in one of the states");
        }

        // Each route refuses a read at the same test: the transaction's revision, then the work
        // the statement may do, then the table.
        let stale = PagedTransaction::new(storage.revision() + 1);
        let spent = Cell::new(crate::sql_script::MAX_SQL_SCRIPT_OPERATIONS);
        let schema = Rc::clone(&storage.table("items").unwrap().schema);
        let value = json!(1);
        let key = PrimaryKey::Values(&[&value]).encode(&schema).unwrap();
        for (transaction, work, table, code) in [
            (Some(&stale), Some(&spent), "missing", "WRITE_CONFLICT"),
            (Some(&stale), None, "items", "WRITE_CONFLICT"),
            (
                Some(&transaction),
                Some(&spent),
                "missing",
                "TRANSACTION_TOO_LARGE",
            ),
            (None, Some(&spent), "items", "TRANSACTION_TOO_LARGE"),
            (Some(&transaction), None, "missing", "TABLE_NOT_FOUND"),
            (None, None, "missing", "TABLE_NOT_FOUND"),
        ] {
            let view = match work {
                Some(work) => PagedReadView::with_work_budget(&storage, transaction, work),
                None => PagedReadView::new(&storage, transaction),
            };
            let held = held_outcome(view.held_encoded_key(table, &key));
            let visited = held_outcome(visited_entry(&view, table, &schema, &[&value]));
            assert_eq!(held, visited, "{table} {code}");
            assert_eq!(held.unwrap_err().0, code, "{table}");
            assert_eq!(spent.get(), crate::sql_script::MAX_SQL_SCRIPT_OPERATIONS);
        }

        // A staged record that no reader can open is refused by both routes with one error: its
        // first header byte is given a flag no record has.
        let ((_, id), _) = staged
            .iter()
            .find(|((table, _), next)| *table == "items" && next.is_some())
            .unwrap();
        let value = json!(id);
        let key = PrimaryKey::Values(&[&value]).encode(&schema).unwrap();
        let overlay = transaction.tables.get_mut("items").unwrap();
        let (_, entry) = overlay
            .rows
            .iter_mut()
            .find(|(staged, _)| *staged == key)
            .unwrap();
        entry.row.next.as_mut().unwrap()[0] |= 0x80;
        let view = PagedReadView::new(&storage, Some(&transaction));
        let held = held_outcome(view.held_encoded_key("items", &key));
        assert_eq!(
            held,
            held_outcome(visited_entry(&view, "items", &schema, &[&value]))
        );
        assert_eq!(held.unwrap_err().0, "PAGED_STORAGE_VERSION_UNSUPPORTED");

        // The same for a committed record, corrupted in its leaf, which is the table's root while
        // the table is this small. The leaf is sealed again, so that only the record is wrong, and
        // the root that preceded the active one is erased, so that opening does not take the
        // change for an interrupted commit and go back to it.
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        storage
            .execute_script(
                [
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT)",
                    "INSERT INTO items (id, name) VALUES (1, 'one'), (2, 'two'), (3, 'three')",
                ]
                .into_iter()
                .map(|sql| crate::statement::parse(sql, &[]).unwrap())
                .collect(),
            )
            .unwrap();
        let schema = Rc::clone(&storage.table("items").unwrap().schema);
        let leaf = storage.table("items").unwrap().root_page_id.unwrap();
        let value = json!(2);
        let key = PrimaryKey::Values(&[&value]).encode(&schema).unwrap();
        let pager = crate::Pager::open_or_create(storage.into_device()).unwrap();
        let inactive = pager.active_metadata().superblock.slot.inactive();
        let mut device = pager.into_device();
        device
            .write_page(inactive.page_id(), &[0; crate::PAGE_SIZE])
            .unwrap();
        let mut page = [0; crate::PAGE_SIZE];
        device.read_page(leaf, &mut page).unwrap();
        // A leaf cell is its header of eight bytes, which begins with the length of its key, then
        // the key, then the value, which here is the record.
        let cell_key = page
            .windows(key.len())
            .position(|window| window == key)
            .unwrap();
        assert_eq!(page[cell_key - 8..cell_key - 6], [key.len() as u8, 0]);
        page[cell_key + key.len()] |= 0x80;
        crate::page::seal(&mut page);
        device.write_page(leaf, &page).unwrap();
        let storage = PagedStorage::open(device).unwrap();
        let transaction = PagedTransaction::new(storage.revision());
        for transaction in [None, Some(&transaction)] {
            let view = PagedReadView::new(&storage, transaction);
            let held = held_outcome(view.held_encoded_key("items", &key));
            assert_eq!(
                held,
                held_outcome(visited_entry(&view, "items", &schema, &[&value]))
            );
            assert_eq!(held.unwrap_err().0, "PAGED_STORAGE_VERSION_UNSUPPORTED");
            // The rows beside it are read as they were.
            for id in [1, 3] {
                let value = json!(id);
                let key = PrimaryKey::Values(&[&value]).encode(&schema).unwrap();
                let held = held_outcome(view.held_encoded_key("items", &key));
                assert!(held.as_ref().is_ok_and(Option::is_some));
                assert_eq!(
                    held,
                    held_outcome(visited_entry(&view, "items", &schema, &[&value]))
                );
            }
        }
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
                    table: "items".into(),
                    row: row(json!({"id": id, "name": name})),
                },
                None => RowChange::Delete {
                    table: "items".into(),
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
                table: "items".into(),
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
                table: "items".into(),
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
        assert_eq!(transaction.tables.len(), 0);
    }

    #[test]
    fn a_failed_statement_frees_the_kept_vectors_and_changes_nothing_else() {
        // Rows that retain far more than they hold, so that few of them bring a transaction to
        // its byte limit and writing its fingerprint out stays quick: a JSON array is estimated
        // by its elements, however short they are.
        let doc = Value::Array(vec![json!(0); 10_000]);
        let mut statements = vec![
            crate::statement::parse("CREATE TABLE docs (id INTEGER PRIMARY KEY, doc JSON)", &[])
                .unwrap(),
        ];
        for id in 1..=9 {
            statements.push(
                crate::statement::parse(
                    "INSERT INTO docs (id, doc) VALUES ($1, $2)",
                    &[json!(id), doc.clone()],
                )
                .unwrap(),
            );
        }
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        storage.execute_script(statements).unwrap();
        let put = |table: &str, id: i64, doc: &Value| {
            vec![RowChange::Upsert {
                table: table.into(),
                row: row(json!({"id": id, "doc": doc})),
            }]
        };
        let kept = |transaction: &PagedTransaction| {
            let room = &transaction.room;
            assert!(room.tables.is_empty() && room.entries.is_empty());
            (room.tables.capacity(), room.entries.capacity())
        };

        // The twin stages every statement that succeeds, and never sees one that fails.
        let mut staged = PagedTransaction::new(storage.revision());
        let mut twin = PagedTransaction::new(storage.revision());
        // A row put back as it is retains its committed row and its staged one, and costs the
        // write set nothing, so it is the overlay's own byte limit that these reach: eight fit
        // under it, within one more of it.
        for id in 1..=8 {
            for transaction in [&mut staged, &mut twin] {
                transaction
                    .stage(&storage, put("docs", id, &doc), Vec::new())
                    .unwrap();
            }
        }
        let retained = staged.totals.overlay_bytes;
        assert!(
            retained < MAX_TRANSACTION_BYTES && retained + retained / 8 > MAX_TRANSACTION_BYTES
        );
        let (tables, entries) = kept(&staged);
        assert!(tables > 0 && entries > 0);

        // The ninth passes the limit once its patch is whole, in validation.
        let before = staged.fingerprint();
        let error = staged
            .stage(&storage, put("docs", 9, &doc), Vec::new())
            .unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.message),
            (
                "TRANSACTION_TOO_LARGE",
                format!("A transaction cannot retain more than {MAX_TRANSACTION_BYTES} bytes")
            )
        );
        assert_eq!(staged.fingerprint(), before);
        assert_eq!(kept(&staged), (0, 0));

        // A small statement then stages as it does in the twin, in vectors of its own, which
        // the transaction keeps again.
        for transaction in [&mut staged, &mut twin] {
            transaction
                .stage(&storage, put("docs", 10, &Value::Null), Vec::new())
                .unwrap();
        }
        assert_eq!(staged.fingerprint(), twin.fingerprint());
        let (tables, entries) = kept(&staged);
        assert!(tables > 0 && entries > 0);

        // A statement that fails before its patch is whole frees the vectors too.
        let before = staged.fingerprint();
        assert_eq!(
            staged
                .stage(&storage, put("missing", 1, &Value::Null), Vec::new())
                .unwrap_err()
                .code,
            "TABLE_NOT_FOUND"
        );
        assert_eq!(staged.fingerprint(), before);
        assert_eq!(kept(&staged), (0, 0));
        for transaction in [&mut staged, &mut twin] {
            transaction
                .stage(&storage, put("docs", 11, &json!([1])), Vec::new())
                .unwrap();
        }
        assert_eq!(staged.fingerprint(), twin.fingerprint());

        // A copy of a transaction, as a script's savepoint takes, starts without kept vectors
        // and leaves the transaction its own.
        let (tables, entries) = kept(&staged);
        assert!(tables > 0 && entries > 0);
        let copy = staged.clone();
        assert_eq!(kept(&copy), (0, 0));
        assert_eq!(kept(&staged), (tables, entries));
        assert_eq!(copy.fingerprint(), staged.fingerprint());
    }

    #[test]
    fn a_statement_of_many_rows_leaves_no_large_vector_with_the_transaction() {
        let storage = storage();
        let rows = |ids: std::ops::Range<i64>| {
            ids.map(|id| RowChange::Upsert {
                table: "items".into(),
                row: row(json!({"id": id, "name": format!("item-{id}")})),
            })
            .collect::<Vec<_>>()
        };
        // How many entries the vector the transaction keeps has room for, which is outside
        // every budget, so it must stay small.
        let kept = |transaction: &PagedTransaction| {
            let room = &transaction.room;
            assert!(room.tables.is_empty() && room.entries.is_empty());
            room.entries.capacity()
        };

        // A statement of many rows builds its entries in a vector of their number, which is
        // freed once they are installed: a transaction that had kept none keeps none.
        let mut transaction = PagedTransaction::new(storage.revision());
        transaction
            .stage(&storage, rows(10..1_010), Vec::new())
            .unwrap();
        assert_eq!(kept(&transaction), 0);
        // A statement of one row leaves the vector its entry was pushed into, which a later
        // statement of many rows neither builds its entries in nor replaces with its own.
        transaction
            .stage(&storage, rows(2_000..2_001), Vec::new())
            .unwrap();
        let small = kept(&transaction);
        assert!((1..=4).contains(&small));
        transaction
            .stage(&storage, rows(3_000..4_000), Vec::new())
            .unwrap();
        assert_eq!(kept(&transaction), small);
        assert_eq!(transaction.totals.overlay_keys, 2_001);

        // A statement of a few rows leaves its vector where the transaction had kept none,
        // since it is no larger than the one a single row leaves.
        let mut transaction = PagedTransaction::new(storage.revision());
        transaction
            .stage(&storage, rows(10..13), Vec::new())
            .unwrap();
        assert!((3..=4).contains(&kept(&transaction)));
    }

    #[test]
    fn a_statement_of_several_tables_installs_each_where_its_overlay_is() {
        // Four tables, each with a unique index and a committed row under a key no other table
        // holds, so that entries read from another table's overlay would not be found there.
        const TABLES: [&str; 4] = ["a", "b", "c", "d"];
        let held = |index: usize| 10 * (index as i64 + 1);
        let mut statements = Vec::new();
        for (index, table) in TABLES.iter().enumerate() {
            for sql in [
                format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"),
                format!("CREATE UNIQUE INDEX {table}_name ON {table} (name)"),
                format!(
                    "INSERT INTO {table} VALUES ({}, '{table} held')",
                    held(index)
                ),
            ] {
                statements.push(crate::statement::parse(&sql, &[]).unwrap());
            }
        }
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        storage.execute_script(statements).unwrap();
        let put = |index: usize, id: i64, name: &str| RowChange::Upsert {
            table: TABLES[index].into(),
            row: row(json!({"id": id, "name": format!("{} {name}", TABLES[index])})),
        };
        let chosen = |set: u32| (0..TABLES.len()).filter(move |index| set & (1 << index) != 0);

        // Every set of tables the transaction stages before the statement, every set the
        // statement changes, and a statement that only adds rows or also replaces staged ones.
        for before in 0..16 {
            for patched in 1..16 {
                for replaces in [false, true] {
                    // Each table staged before has its committed row renamed, which gives up
                    // the name it held. The statement gives that name to a new row, which the
                    // table's overlay allows only if the entry renaming the committed row is
                    // read from it; and it renames the committed row again, or for a table not
                    // staged before, for the first time.
                    let changes = |index: usize| {
                        let name = if before & (1 << index) != 0 {
                            "held"
                        } else {
                            "new"
                        };
                        let mut changes = vec![put(index, 1, name)];
                        if replaces {
                            changes.push(put(index, held(index), "moved again"));
                        }
                        changes
                    };
                    let mut transaction = PagedTransaction::new(storage.revision());
                    for index in chosen(before) {
                        transaction
                            .stage(&storage, vec![put(index, held(index), "moved")], Vec::new())
                            .unwrap();
                    }
                    transaction
                        .stage(
                            &storage,
                            chosen(patched).flat_map(changes).collect(),
                            Vec::new(),
                        )
                        .unwrap();

                    // The twin stages the same rows a change at a time, last table first.
                    let mut twin = PagedTransaction::new(storage.revision());
                    for index in chosen(before).rev() {
                        twin.stage(&storage, vec![put(index, held(index), "moved")], Vec::new())
                            .unwrap();
                    }
                    for index in chosen(patched).rev() {
                        for change in changes(index) {
                            twin.stage(&storage, vec![change], Vec::new()).unwrap();
                        }
                    }
                    assert_eq!(
                        transaction.fingerprint(),
                        twin.fingerprint(),
                        "{before:04b} {patched:04b} {replaces}"
                    );
                    // Each overlay holds the rows of its own table, which name it.
                    assert_eq!(
                        transaction.tables.keys().collect::<Vec<_>>(),
                        chosen(before | patched)
                            .map(|index| TABLES[index])
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(
                        transaction.touched_tables(),
                        transaction.tables.keys().cloned().collect::<Vec<_>>()
                    );
                    for (table, overlay) in transaction.tables.iter() {
                        let paged = storage.table(table).unwrap();
                        for (key, entry) in &overlay.rows {
                            let record = entry.row.next.as_ref().unwrap();
                            let staged = paged.record(key, record).unwrap().to_row().unwrap();
                            let name = staged["name"].as_str().unwrap();
                            assert!(name.starts_with(table.as_str()), "{table}: {name}");
                        }
                    }
                }
            }
        }
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

    /// An entry of no consequence, for an index over keys alone.
    fn blank_entry() -> OverlayEntry {
        OverlayEntry {
            row: ChangedRow {
                old: None,
                next: None,
            },
            changed: false,
            retained: 0,
            cost: None,
        }
    }

    /// Indexes `present`, shuffled, checking at each doubling that each key took one slot; then
    /// that every key is found at its position and none of `absent` is found. Returns how many
    /// doublings the index grew through, and the longest run of probes a key took to be found.
    fn check_key_index(mut present: Vec<Vec<u8>>, absent: &[Vec<u8>]) -> (usize, usize) {
        assert_eq!(present.iter().collect::<BTreeSet<_>>().len(), present.len());
        // Shuffled, so that the keys do not arrive in the order they vary in.
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        for index in (1..present.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            present.swap(index, (seed % (index as u64 + 1)) as usize);
        }
        let occupied = |index: &KeyIndex| {
            index
                .slots
                .iter()
                .filter(|slot| **slot != EMPTY_SLOT)
                .count()
        };
        let mut rows = Vec::new();
        let mut index = KeyIndex::default();
        let mut doublings = 0;
        for key in present {
            let slots = index.slots.len();
            let position = rows.len();
            let slot = index.find(&rows, &key).unwrap_err();
            rows.push((key, blank_entry()));
            index.insert(&rows, position, slot);
            if index.slots.len() != slots {
                doublings += 1;
                assert_eq!(occupied(&index), rows.len());
            }
        }
        assert_eq!(occupied(&index), rows.len());
        let mut longest = 0;
        for (position, (key, _)) in rows.iter().enumerate() {
            assert_eq!(index.find(&rows, key), Ok(position));
            let mut slot = index.home(key);
            let mut probes = 1;
            while index.slots[slot] as usize != position {
                slot = (slot + 1) & (index.slots.len() - 1);
                probes += 1;
            }
            longest = longest.max(probes);
        }
        for key in absent {
            assert!(index.find(&rows, key).is_err(), "{key:?} was found");
        }
        (doublings, longest)
    }

    #[test]
    fn key_index_finds_every_key_and_no_other() {
        // Keys of each shape a primary key takes, varying where its encoding puts the variation:
        // an integer in its last bytes, a float in its first, text after a shared prefix or in
        // fewer bytes than a word, a boolean in its one byte, and a composite key in the integer
        // after its text.
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        let statements = [
            "CREATE TABLE ints (id INTEGER PRIMARY KEY)",
            "CREATE TABLE floats (id FLOAT PRIMARY KEY)",
            "CREATE TABLE texts (id TEXT PRIMARY KEY)",
            "CREATE TABLE flags (id BOOLEAN PRIMARY KEY)",
            "CREATE TABLE pairs (a TEXT, b INTEGER, PRIMARY KEY (a, b))",
        ]
        .into_iter()
        .map(|sql| crate::statement::parse(sql, &[]))
        .collect::<Result<Vec<_>>>()
        .unwrap();
        storage.execute_script(statements).unwrap();
        let encode = |table: &str, key: Value| {
            encode_primary_key(&storage.table(table).unwrap().schema, &row(key)).unwrap()
        };
        let mut ints = (Vec::new(), Vec::new());
        let mut floats = (Vec::new(), Vec::new());
        let mut texts = (Vec::new(), Vec::new());
        let mut pairs = (Vec::new(), Vec::new());
        for id in 1..=10_000_i64 {
            ints.0.push(encode("ints", json!({"id": id})));
            floats.0.push(encode("floats", json!({"id": id as f64})));
            texts
                .0
                .push(encode("texts", json!({"id": format!("task-{id:06}")})));
            pairs.0.push(encode(
                "pairs",
                json!({"a": format!("group-{}", id % 7), "b": id}),
            ));
        }
        for id in 10_001..=10_100_i64 {
            ints.1.push(encode("ints", json!({"id": id})));
            floats
                .1
                .push(encode("floats", json!({"id": id as f64 - 0.5})));
            texts
                .1
                .push(encode("texts", json!({"id": format!("task-{id:06}")})));
            pairs.1.push(encode(
                "pairs",
                json!({"a": format!("group-{}", id % 7), "b": id}),
            ));
        }
        ints.1.push(encode("ints", json!({"id": 0})));
        ints.1.push(encode("ints", json!({"id": -1})));
        texts.1.push(encode("texts", json!({"id": "task-00001"})));
        texts.1.push(encode("texts", json!({"id": "task-0000010"})));
        pairs
            .1
            .push(encode("pairs", json!({"a": "group-1", "b": 2})));
        let mut short = (Vec::new(), Vec::new());
        for flag in [true, false] {
            short.0.push(encode("flags", json!({"id": flag})));
        }
        for first in 'a'..='z' {
            short
                .0
                .push(encode("texts", json!({"id": first.to_string()})));
            for second in 'a'..='z' {
                short
                    .0
                    .push(encode("texts", json!({"id": format!("{first}{second}")})));
            }
            short.1.push(encode(
                "texts",
                json!({"id": first.to_ascii_uppercase().to_string()}),
            ));
        }
        short.1.push(encode("texts", json!({"id": ""})));

        let mut all = (Vec::new(), Vec::new());
        for (name, (present, absent)) in [
            ("ints", &ints),
            ("floats", &floats),
            ("texts", &texts),
            ("pairs", &pairs),
            ("short", &short),
        ] {
            let (doublings, longest) = check_key_index(present.clone(), absent);
            assert!(longest < 32, "{name}: a run of {longest} probes");
            assert!(
                doublings >= 11 || name == "short",
                "{name}: {doublings} doublings"
            );
            all.0.extend(present.iter().cloned());
            all.1.extend(absent.iter().cloned());
        }
        let (doublings, longest) = check_key_index(all.0, &all.1);
        assert!(longest < 32, "all shapes: a run of {longest} probes");
        assert!(doublings >= 13, "all shapes: {doublings} doublings");
    }

    #[test]
    fn ordered_walks_follow_key_order_however_keys_arrive() {
        let storage = storage();
        let paged = storage.table("items").unwrap();
        let key = |id: i64| encode_primary_key(&paged.schema, &row(json!({"id": id}))).unwrap();
        let item = |id: i64, name: &str| row(json!({"id": id, "name": name}));
        let committed = BTreeMap::from([(key(1), item(1, "one")), (key(2), item(2, "two"))]);

        // Checks every walk of the overlay against `oracle`, what the transaction stages for each
        // key: a row, or none for a delete.
        let check = |transaction: &PagedTransaction, oracle: &BTreeMap<Vec<u8>, Option<Row>>| {
            let overlay = transaction.table("items").unwrap();
            assert_eq!(overlay.rows.len(), oracle.len());
            assert_eq!(overlay.order.borrow().len(), oracle.len());
            assert_eq!(
                overlay.deletes,
                oracle.values().filter(|staged| staged.is_none()).count()
            );
            // The positions in key order name the oracle's keys in its order, and leave no tail.
            assert_eq!(
                overlay
                    .ordered()
                    .iter()
                    .map(|&position| overlay.rows[position as usize].0.as_slice())
                    .collect::<Vec<_>>(),
                oracle.keys().map(Vec::as_slice).collect::<Vec<_>>()
            );
            assert_eq!(overlay.sorted.get(), overlay.rows.len());
            // The keys whose staged row differs from the committed one, in key order.
            let changed = oracle
                .iter()
                .filter(|(key, staged)| committed.get(*key) != staged.as_ref())
                .collect::<Vec<_>>();
            let changed_rows = transaction.changed_rows();
            let rows = changed_rows
                .iter()
                .find(|(table, _)| *table == "items")
                .map_or(&[][..], |(_, rows)| rows.as_slice());
            assert_eq!(rows.len(), changed.len());
            for ((key, change), (expected_key, expected)) in rows.iter().zip(&changed) {
                assert_eq!(*key, expected_key.as_slice());
                let next = change
                    .next
                    .as_ref()
                    .map(|record| paged.record(key, record).unwrap().to_row().unwrap());
                assert_eq!(next, **expected);
            }
            let keys = transaction.changed_keys(&storage).unwrap();
            let values = keys
                .get("items")
                .map(|keys| keys.values.clone())
                .unwrap_or_default();
            assert_eq!(
                values,
                changed
                    .iter()
                    .map(|(key, _)| key_row(paged, key).unwrap()["id"].clone())
                    .collect::<Vec<_>>()
            );
            // A scan presents the committed rows not replaced, then the staged rows, in key order.
            let view = PagedReadView::new(&storage, Some(transaction));
            let mut expected = Vec::new();
            for (key, committed) in &committed {
                if !changed.iter().any(|(changed, _)| *changed == key) {
                    expected.push(committed.clone());
                }
            }
            for (_, staged) in &changed {
                if let Some(staged) = staged {
                    expected.push(staged.clone());
                }
            }
            assert_eq!(view.scan_table("items").unwrap(), expected);
            let live = committed
                .keys()
                .chain(oracle.keys())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|key| oracle.get(*key).is_none_or(|staged| staged.is_some()))
                .count();
            assert_eq!(view.table_row_count("items").unwrap(), live);
            for (key, staged) in oracle {
                assert_eq!(
                    view.holds_encoded_key("items", key).unwrap(),
                    staged.is_some()
                );
                assert_eq!(
                    view.lookup_primary_key("items", &key_row(paged, key).unwrap())
                        .unwrap(),
                    *staged
                );
            }
            let fingerprint = transaction.fingerprint();
            let lines = fingerprint
                .lines()
                .filter(|line| line.starts_with("items ["))
                .collect::<Vec<_>>();
            assert_eq!(lines.len(), oracle.len());
            for (line, key) in lines.iter().zip(oracle.keys()) {
                assert!(line.starts_with(&format!("items {key:?} old=")), "{line}");
            }
            assert!(
                fingerprint
                    .lines()
                    .any(|line| line == format!("items deletes={}", overlay.deletes))
            );
        };

        // Each statement's changes by key, a row to upsert or none to delete, with whether a tail
        // is expected once they are staged; and the scans between them, which merge it in.
        type Statement<'a> = (Vec<(i64, Option<&'a str>)>, bool);
        let mut statements: Vec<Statement> = vec![
            // Ascending keys, one and several per statement, keep the arrival order.
            (vec![(100, Some("hundred"))], false),
            (vec![(101, Some("hundred and one"))], false),
            (
                vec![(102, Some("a")), (103, Some("b")), (104, Some("c"))],
                false,
            ),
            (vec![], false),
            // Keys below the last, singly and together, with a scan between them.
            (vec![(50, Some("fifty"))], true),
            (vec![], false),
            (vec![(49, Some("forty-nine"))], true),
            (vec![(48, Some("forty-eight"))], true),
            (vec![], false),
            // A key above every other keeps the rebuilt order.
            (vec![(200, Some("two hundred"))], false),
            // Keys interleaving the staged ones, changing and deleting committed rows.
            (
                vec![
                    (75, Some("seventy-five")),
                    (10, Some("ten")),
                    (1, Some("changed")),
                    (2, None),
                ],
                true,
            ),
            (vec![], false),
            // A replacement, a committed row put back, a staged row deleted, and its key reused:
            // none of them adds a row or a position.
            (vec![(100, Some("replaced"))], false),
            (vec![(2, Some("two"))], false),
            (vec![(50, None)], false),
            (vec![(50, Some("fifty again"))], false),
            (vec![], false),
            (
                vec![(60, Some("sixty")), (300, Some("three hundred"))],
                true,
            ),
            (vec![], false),
        ];
        // A key above every other, then a key scattered below it before each scan, as the
        // statements of a transaction that updates by spread keys and scans between them stage
        // them: each scan merges a tail of one position.
        statements.push((vec![(2000, Some("two thousand"))], false));
        for index in 1..=40 {
            statements.push((vec![(1000 + (index * 919) % 1000, Some("spread"))], true));
            statements.push((vec![], false));
        }
        // A long run of ascending keys after a merge keeps the order whole, however many
        // statements stage it, so a scan after it merges nothing.
        statements.push(((3000..3050).map(|id| (id, Some("run"))).collect(), false));
        statements.extend((3050..3100).map(|id| (vec![(id, Some("run"))], false)));
        statements.push((vec![], false));
        // A key below the run, staged in one statement with a second run, begins the tail, and
        // the run lengthens it, since a statement's entries arrive in key order: the scan merges
        // a tail of one key among the head's and a hundred above them, in one pass.
        statements.push((
            std::iter::once((500, Some("five hundred")))
                .chain((3100..3200).map(|id| (id, Some("run"))))
                .collect(),
            true,
        ));
        statements.push((vec![], false));
        // Checked after every statement, which merges the tail each time, and then only at the
        // scans, so that several keys arrive out of order before a merge.
        for check_every_statement in [true, false] {
            let mut transaction = PagedTransaction::new(storage.revision());
            let mut oracle = BTreeMap::new();
            // How many keys joined the rows since the last walk: the most a merge can search for.
            let mut appended = 0;
            for (changes, expected_tail) in &statements {
                if !changes.is_empty() {
                    for (id, name) in changes {
                        if oracle
                            .insert(key(*id), name.map(|name| item(*id, name)))
                            .is_none()
                        {
                            appended += 1;
                        }
                    }
                    let staged = changes
                        .iter()
                        .map(|(id, name)| match name {
                            Some(name) => RowChange::Upsert {
                                table: "items".into(),
                                row: item(*id, name),
                            },
                            None => RowChange::Delete {
                                table: "items".into(),
                                key: row(json!({"id": id})),
                            },
                        })
                        .collect();
                    transaction.stage(&storage, staged, Vec::new()).unwrap();
                    let overlay = transaction.table("items").unwrap();
                    assert_eq!(
                        overlay.sorted.get() < overlay.rows.len(),
                        *expected_tail,
                        "{changes:?}"
                    );
                    assert_eq!(overlay.rows.len(), oracle.len());
                    assert_eq!(overlay.order.borrow().len(), oracle.len());
                    if !check_every_statement {
                        continue;
                    }
                }
                let overlay = transaction.table("items").unwrap();
                let tail = overlay.rows.len() - overlay.sorted.get();
                let searches = overlay.searches.get();
                check(&transaction, &oracle);
                // A merge searches the head once for each tail position, which the keys that
                // joined since the last walk bound; a walk of a whole order searches nothing.
                assert_eq!(overlay.searches.get() - searches, tail);
                assert!(tail <= appended, "a tail of {tail} from {appended} keys");
                appended = 0;
            }
        }
    }

    #[test]
    fn tails_of_every_length_merge_in_at_every_place() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut random = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        let mut overlay = TableOverlay::default();
        let mut oracle = BTreeSet::new();
        // How many keys joined the rows since the last walk.
        let mut appended = 0;
        // Above every key staged so far.
        let mut above = 1_000_000;
        // How many walks found no tail, a tail wholly below the head, and a tail among it.
        let (mut whole, mut below, mut among) = (0, 0, 0);
        for statement in 0..1_000 {
            // A statement's keys, in key order: a run above every key so far, as inserts stage,
            // and first of all, so that the keys after it fall below the whole head; a few
            // scattered ones, as a range update does; or one, as a statement by key does, any
            // of which may be staged already. A run's keys are text of one length, above every
            // scattered key, which is the digits of its number: keys of several lengths, which
            // share prefixes and are prefixes of one another, merge in among each other.
            let keys = match if statement == 0 { 0 } else { random(4) } {
                0 => {
                    let run = 1 + random(50);
                    above += run;
                    (above - run..above)
                        .map(|id| format!("r{id:07}").into_bytes())
                        .collect::<BTreeSet<_>>()
                }
                1 => (0..random(20))
                    .map(|_| random(5_000).to_string().into_bytes())
                    .collect(),
                _ => BTreeSet::from([random(5_000).to_string().into_bytes()]),
            };
            appended += keys.iter().filter(|key| !oracle.contains(*key)).count();
            oracle.extend(keys.iter().cloned());
            overlay.install(
                &mut keys.into_iter().map(|key| (key, blank_entry())).collect(),
                0,
                None,
            );
            assert_eq!(overlay.rows.len(), oracle.len());
            if random(3) == 0 || statement == 999 {
                let tail = overlay.rows.len() - overlay.sorted.get();
                let key = |position: u32| overlay.rows[position as usize].0.as_slice();
                let order = overlay.order.borrow();
                let (head, tail_positions) = order.split_at(overlay.sorted.get());
                if tail_positions.is_empty() {
                    whole += 1;
                } else if tail_positions
                    .iter()
                    .all(|&position| key(position) < key(head[0]))
                {
                    below += 1;
                } else {
                    among += 1;
                }
                // The walk merges the tail under its own borrow.
                drop(order);
                let searches = overlay.searches.get();
                assert_eq!(
                    overlay
                        .ordered()
                        .iter()
                        .map(|&position| overlay.rows[position as usize].0.as_slice())
                        .collect::<Vec<_>>(),
                    oracle.iter().map(Vec::as_slice).collect::<Vec<_>>()
                );
                assert_eq!(overlay.sorted.get(), overlay.rows.len());
                assert_eq!(overlay.searches.get() - searches, tail);
                assert!(tail <= appended, "a tail of {tail} from {appended} keys");
                appended = 0;
            }
        }
        assert!(
            whole > 0 && below > 0 && among > 0,
            "{whole} whole, {below} below, {among} among"
        );
    }
}
