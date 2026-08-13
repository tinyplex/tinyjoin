use std::{
    cell::{RefCell, RefMut},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use crate::{
    AllocationBitmap, CandidateId, DEFAULT_PAGE_CACHE_BYTES, EngineError, FIRST_DATA_PAGE_ID,
    MAX_PAGE_COUNT, PAGE_SIZE, Page, PageCache, PageDevice, PageId, RawMetadataSlot,
    RecoveredMetadata, Result, SUPERBLOCK_PAGE_COUNT, Superblock, SuperblockSlot,
    build_next_metadata, recover_metadata,
};

/// A bounded, copy-on-write page store with crash-safe paired metadata roots.
///
/// `Pager` deliberately permits only one [`PagerWriteTransaction`] at a time. Data pages are
/// written to locations which are unreachable from the active superblock, then published by
/// writing the inactive allocation bitmap and superblock. A failure after the superblock write is
/// attempted poisons the in-memory pager: callers must reopen the same device to discover whether
/// the old or new root became durable.
pub struct Pager<D: PageDevice> {
    device: SharedPageDevice<D>,
    cache: PageCache<SharedPageDevice<D>>,
    active: RecoveredMetadata,
    next_candidate_id: CandidateId,
    recovery_required: bool,
    #[cfg(test)]
    fail_install_once: bool,
}

impl<D: PageDevice> Pager<D> {
    pub fn open_or_create(device: D) -> Result<Self> {
        Self::with_cache_capacity(device, DEFAULT_PAGE_CACHE_BYTES)
    }

    pub fn with_cache_capacity(device: D, cache_capacity_bytes: usize) -> Result<Self> {
        // Constructing the cache first validates the requested bound before a new device is
        // mutated. The shared wrapper gives the pager direct metadata I/O while keeping all data
        // page I/O inside the cache.
        let device = SharedPageDevice::new(device);
        let cache = PageCache::with_capacity(device.clone(), cache_capacity_bytes)?;
        let page_count = device.page_count();
        if page_count > MAX_PAGE_COUNT {
            return Err(pager_error(format!(
                "The page device contains {page_count} pages, exceeding the {MAX_PAGE_COUNT}-page limit"
            )));
        }

        let active = if page_count == 0 {
            initialize_empty_device(&device)?
        } else if page_count < FIRST_DATA_PAGE_ID {
            if is_recoverable_empty_bootstrap(&device)? {
                initialize_empty_device(&device)?
            } else {
                return Err(pager_error(format!(
                    "The incomplete {page_count}-page bootstrap contains data which is not part of a deterministic empty database"
                )));
            }
        } else {
            match recover_device_metadata(&device) {
                Ok(active) => active,
                Err(_)
                    if page_count == FIRST_DATA_PAGE_ID
                        && is_recoverable_empty_bootstrap(&device)? =>
                {
                    initialize_empty_device(&device)?
                }
                Err(error) => return Err(error),
            }
        };
        validate_physical_coverage(&device, &active.allocation_bitmap)?;

        Ok(Self {
            device,
            cache,
            active,
            next_candidate_id: 1,
            recovery_required: false,
            #[cfg(test)]
            fail_install_once: false,
        })
    }

    pub fn database_revision(&self) -> u64 {
        self.active.superblock.database_revision
    }

    pub fn applied_journal_sequence(&self) -> u64 {
        self.active.superblock.applied_journal_sequence
    }

    pub fn generation(&self) -> u64 {
        self.active.superblock.generation
    }

    pub fn catalog_root_page_id(&self) -> Option<PageId> {
        self.active.superblock.catalog_root_page_id
    }

    pub fn physical_page_count(&self) -> PageId {
        self.device.page_count()
    }

    pub fn is_recovery_required(&self) -> bool {
        self.recovery_required
    }

    pub fn active_metadata(&self) -> &RecoveredMetadata {
        &self.active
    }

    pub fn read_page(&mut self, id: PageId) -> Result<Page> {
        self.ensure_usable()?;
        ensure_data_page(id)?;
        if !self.active.allocation_bitmap.is_allocated(id)? {
            return Err(page_not_allocated(id));
        }
        let bytes = self.cache.read_page(id)?;
        decode_expected_page(id, bytes)
    }

    pub fn begin_write(&mut self) -> Result<PagerWriteTransaction<'_, D>> {
        self.ensure_usable()?;
        let candidate = self.next_candidate_id;
        self.next_candidate_id = self.next_candidate_id.checked_add(1).ok_or_else(|| {
            pager_error("The in-memory write transaction identifier space is exhausted")
        })?;
        let next_bitmap = self.active.allocation_bitmap.clone();
        Ok(PagerWriteTransaction {
            pager: self,
            candidate,
            next_bitmap,
            new_pages: BTreeSet::new(),
            written_pages: BTreeSet::new(),
            btree_versions: BTreeMap::new(),
            next_allocation_page_id: FIRST_DATA_PAGE_ID,
            failed: false,
            finished: false,
        })
    }

    /// Returns the underlying device after flushing or publishing is no longer required.
    ///
    /// This does not add a durability barrier. It is chiefly useful for closing a pager before
    /// transferring its device to another owner or reopening it after a simulated process exit.
    pub fn into_device(self) -> D {
        let Self {
            device,
            cache,
            active: _,
            next_candidate_id: _,
            recovery_required: _,
            #[cfg(test)]
                fail_install_once: _,
        } = self;
        drop(cache);
        device.into_inner()
    }

    fn ensure_usable(&self) -> Result<()> {
        if self.recovery_required {
            Err(recovery_required(
                "The previous metadata publication had an unknown outcome",
            ))
        } else {
            Ok(())
        }
    }
}

/// A single copy-on-write candidate rooted in a [`Pager`].
///
/// Dropping an uncommitted transaction abandons its cache reservations. Physical pages already
/// written by eviction or dense-file extension remain harmless orphans because the active
/// allocation bitmap cannot reach them.
pub struct PagerWriteTransaction<'a, D: PageDevice> {
    pager: &'a mut Pager<D>,
    candidate: CandidateId,
    next_bitmap: AllocationBitmap,
    new_pages: BTreeSet<PageId>,
    written_pages: BTreeSet<PageId>,
    btree_versions: BTreeMap<u64, u64>,
    next_allocation_page_id: PageId,
    failed: bool,
    finished: bool,
}

impl<D: PageDevice> PagerWriteTransaction<'_, D> {
    pub fn candidate_id(&self) -> CandidateId {
        self.candidate
    }

    /// Returns the generation which this candidate will publish.
    pub fn generation(&self) -> Result<u64> {
        self.ensure_open()?;
        self.pager
            .active
            .superblock
            .generation
            .checked_add(1)
            .ok_or_else(|| pager_error("The page generation space is exhausted"))
    }

    /// Reports whether `id` was allocated by this write transaction.
    pub fn owns_page(&self, id: PageId) -> bool {
        !self.finished && self.new_pages.contains(&id)
    }

    /// Prevents a partially-built higher-level structure from being published.
    pub(crate) fn mark_failed(&mut self) {
        self.failed = true;
    }

    /// Returns the transaction-local mutation version for one B-tree.
    ///
    /// Detached B-tree cursors use this to remain valid while unrelated trees are written, but
    /// fail closed if their own tree changes within the same candidate generation.
    pub(crate) fn btree_version(&self, tree_id: u64) -> Result<u64> {
        self.ensure_open()?;
        Ok(self.btree_versions.get(&tree_id).copied().unwrap_or(0))
    }

    /// Advances the transaction-local mutation version for one B-tree.
    pub(crate) fn mark_btree_mutated(&mut self, tree_id: u64) -> Result<()> {
        self.ensure_open()?;
        let version = self.btree_versions.entry(tree_id).or_default();
        *version = version
            .checked_add(1)
            .ok_or_else(|| pager_error("The B-tree mutation version space is exhausted"))?;
        Ok(())
    }

    /// Allocates the next safe page ID, wrapping once to reuse lower free pages.
    ///
    /// Pages freed by this same transaction are not reusable until a later generation because
    /// they remain reachable from the currently active root. When allocation extends the physical
    /// file, a zero-filled placeholder is appended immediately so later cache eviction can write
    /// candidate pages in any order without violating a dense PageDevice contract.
    pub fn allocate_page(&mut self) -> Result<PageId> {
        self.ensure_open()?;
        let start = self.next_allocation_page_id;
        let mut selected = None;
        for id in (start..MAX_PAGE_COUNT).chain(FIRST_DATA_PAGE_ID..start) {
            if !self.pager.active.allocation_bitmap.is_allocated(id)?
                && !self.next_bitmap.is_allocated(id)?
            {
                selected = Some(id);
                break;
            }
        }
        let id = selected.ok_or_else(|| {
            EngineError::new(
                "DATABASE_FULL",
                format!(
                    "The database has reached its fixed limit of {MAX_PAGE_COUNT} physical pages"
                ),
            )
        })?;
        self.next_allocation_page_id = if id + 1 == MAX_PAGE_COUNT {
            FIRST_DATA_PAGE_ID
        } else {
            id + 1
        };

        let page_count = self.pager.device.page_count();
        if id > page_count {
            return Err(pager_error(format!(
                "Allocator selected non-dense page {id} after a {page_count}-page physical file"
            )));
        }
        if id == page_count {
            self.pager.device.write_page(id, &[0; PAGE_SIZE])?;
        }
        self.pager.cache.reserve_candidate_page(
            self.candidate,
            id,
            &self.pager.active.allocation_bitmap,
        )?;
        self.next_bitmap.set_allocated(id, true)?;
        self.new_pages.insert(id);
        Ok(id)
    }

    pub fn write_new_page(&mut self, page: &Page) -> Result<()> {
        self.ensure_open()?;
        if !self.new_pages.contains(&page.id) {
            return Err(pager_error(format!(
                "Page {} was not allocated by this write transaction",
                page.id
            )));
        }
        let bytes = page.encode()?;
        self.pager
            .cache
            .write_candidate_page(self.candidate, page.id, &bytes)?;
        self.written_pages.insert(page.id);
        Ok(())
    }

    pub fn read_page(&mut self, id: PageId) -> Result<Page> {
        self.ensure_open()?;
        ensure_data_page(id)?;
        if !self.next_bitmap.is_allocated(id)? {
            return Err(page_not_allocated(id));
        }
        let bytes = if self.new_pages.contains(&id) {
            self.pager.cache.read_candidate_page(self.candidate, id)?
        } else if self.pager.active.allocation_bitmap.is_allocated(id)? {
            self.pager.cache.read_page(id)?
        } else {
            return Err(pager_error(format!(
                "Page {id} is allocated in the candidate bitmap without candidate ownership"
            )));
        };
        decode_expected_page(id, bytes)
    }

    /// Removes a page shared from the active generation from the candidate root.
    ///
    /// Newly allocated pages must instead be released with [`Self::release_new_page`].
    pub fn free_shared_page(&mut self, id: PageId) -> Result<()> {
        self.ensure_open()?;
        ensure_data_page(id)?;
        if self.new_pages.contains(&id) {
            return Err(pager_error(format!(
                "New candidate page {id} cannot be freed before publication"
            )));
        }
        if !self.pager.active.allocation_bitmap.is_allocated(id)? {
            return Err(page_not_allocated(id));
        }
        self.next_bitmap.set_allocated(id, false)
    }

    /// Releases a newly allocated page which is no longer reachable from this candidate.
    ///
    /// The caller must remove every candidate reference to the page first. Any bytes already
    /// written through cache eviction remain an unreachable physical orphan and are safe to reuse.
    pub fn release_new_page(&mut self, id: PageId) -> Result<()> {
        self.ensure_open()?;
        ensure_data_page(id)?;
        if !self.new_pages.contains(&id) {
            return Err(pager_error(format!(
                "Page {id} was not allocated by this write transaction"
            )));
        }
        self.pager
            .cache
            .release_candidate_page(self.candidate, id)?;
        self.next_bitmap.set_allocated(id, false)?;
        self.new_pages.remove(&id);
        self.written_pages.remove(&id);
        self.next_allocation_page_id = self.next_allocation_page_id.min(id);
        Ok(())
    }

    pub fn abort(mut self) {
        self.pager.cache.invalidate_candidate(self.candidate);
        self.finished = true;
    }

    /// Publishes this candidate in the only crash-safe order:
    ///
    /// 1. write and flush candidate data pages;
    /// 2. write the inactive bitmap chunks and flush them;
    /// 3. write the inactive superblock and flush it;
    /// 4. install the candidate cache view and swap the in-memory active root.
    pub fn commit(
        mut self,
        database_revision: u64,
        applied_journal_sequence: u64,
        catalog_root_page_id: Option<PageId>,
    ) -> Result<()> {
        self.ensure_open()?;
        if let Err(error) = crate::revision::validate_database_revision(database_revision) {
            return self.fail_before_superblock(error);
        }
        if self.new_pages != self.written_pages {
            let unwritten = self
                .new_pages
                .difference(&self.written_pages)
                .copied()
                .collect::<Vec<_>>();
            return self.fail_before_superblock(pager_error(format!(
                "Every allocated page must be initialized before commit; unwritten pages: {unwritten:?}"
            )));
        }

        let pending = match build_next_metadata(
            &self.pager.active,
            database_revision,
            applied_journal_sequence,
            catalog_root_page_id,
            &self.next_bitmap,
        ) {
            Ok(pending) => pending,
            Err(error) => return self.fail_before_superblock(error),
        };
        // Serialize every metadata page before performing any publication I/O. This prevents a
        // local validation error from appearing after candidate data has been flushed.
        let bitmap_pages = match pending.allocation_bitmap.encode_pages() {
            Ok(pages) => pages,
            Err(error) => return self.fail_before_superblock(error),
        };
        let superblock_page = match pending.superblock.encode_page() {
            Ok(page) => page,
            Err(error) => return self.fail_before_superblock(error),
        };

        if let Err(error) = self.pager.cache.flush_candidate(self.candidate) {
            return self.fail_before_superblock(error);
        }
        for (chunk, bytes) in bitmap_pages.iter().enumerate() {
            if let Err(error) = self
                .pager
                .device
                .write_page(pending.superblock.bitmap_slot.page_id(chunk), bytes)
            {
                return self.fail_before_superblock(error);
            }
        }
        if let Err(error) = self.pager.device.flush() {
            return self.fail_before_superblock(error);
        }

        // From this point onward the inactive superblock may already name the new bitmap even if
        // the device reports failure. Continuing to use the old in-memory view could overwrite
        // pages needed by whichever generation actually became durable.
        if let Err(error) = self
            .pager
            .device
            .write_page(pending.superblock.slot.page_id(), &superblock_page)
        {
            return self.fail_after_superblock("writing the new superblock", error);
        }
        if let Err(error) = self.pager.device.flush() {
            return self.fail_after_superblock("flushing the new superblock", error);
        }

        #[cfg(test)]
        if std::mem::take(&mut self.pager.fail_install_once) {
            return self.fail_after_superblock(
                "installing the published cache view",
                pager_error("injected cache installation failure"),
            );
        }
        if let Err(error) = self
            .pager
            .cache
            .install_candidate(self.candidate, &pending.allocation_bitmap)
        {
            return self.fail_after_superblock("installing the published cache view", error);
        }

        self.pager.active = RecoveredMetadata {
            superblock: pending.superblock,
            allocation_bitmap: pending.allocation_bitmap,
        };
        self.finished = true;
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        if self.finished {
            Err(pager_error("The write transaction is already finished"))
        } else if self.failed {
            Err(EngineError::new(
                "TRANSACTION_FAILED",
                "The page transaction encountered an error and must be aborted",
            ))
        } else {
            self.pager.ensure_usable()
        }
    }

    fn fail_before_superblock<T>(&mut self, error: EngineError) -> Result<T> {
        self.pager.cache.invalidate_candidate(self.candidate);
        self.finished = true;
        Err(error)
    }

    fn fail_after_superblock<T>(&mut self, stage: &str, error: EngineError) -> Result<T> {
        self.pager.recovery_required = true;
        self.pager.cache.invalidate_candidate(self.candidate);
        self.finished = true;
        Err(recovery_required(format!(
            "Metadata publication failed while {stage}; reopen the page device before continuing ({error})"
        )))
    }
}

impl<D: PageDevice> Drop for PagerWriteTransaction<'_, D> {
    fn drop(&mut self) {
        if !self.finished {
            self.pager.cache.invalidate_candidate(self.candidate);
            self.finished = true;
        }
    }
}

struct SharedPageDevice<D>(Rc<RefCell<D>>);

impl<D> SharedPageDevice<D> {
    fn new(device: D) -> Self {
        Self(Rc::new(RefCell::new(device)))
    }

    fn borrow_mut(&self) -> RefMut<'_, D> {
        self.0.borrow_mut()
    }

    fn into_inner(self) -> D {
        match Rc::try_unwrap(self.0) {
            Ok(device) => device.into_inner(),
            Err(_) => unreachable!("the pager owns the final shared page-device handle"),
        }
    }
}

impl<D> Clone for SharedPageDevice<D> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl<D: PageDevice> PageDevice for SharedPageDevice<D> {
    fn page_count(&self) -> PageId {
        self.0.borrow().page_count()
    }

    fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
        self.borrow_mut().read_page(id, destination)
    }

    fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
        self.borrow_mut().write_page(id, source)
    }

    fn flush(&mut self) -> Result<()> {
        self.borrow_mut().flush()
    }
}

fn initialize_empty_device<D: PageDevice>(
    device: &SharedPageDevice<D>,
) -> Result<RecoveredMetadata> {
    let (active, pages) = empty_metadata_layout()?;

    // Establish the fixed metadata extent densely, but leave both superblocks invalid until both
    // bitmaps have crossed a durability barrier. Re-running this sequence is safe only after
    // `is_recoverable_empty_bootstrap` proves that every existing byte is either zero or its
    // corresponding byte in this deterministic generation-1 empty database. That includes a
    // target page torn while transitioning in either direction.
    let zero = [0; PAGE_SIZE];
    let mut direct = device.clone();
    for id in 0..FIRST_DATA_PAGE_ID {
        direct.write_page(id, &zero)?;
    }
    for id in SUPERBLOCK_PAGE_COUNT as PageId..FIRST_DATA_PAGE_ID {
        direct.write_page(id, &pages[id as usize])?;
    }
    direct.flush()?;
    for id in 0..SUPERBLOCK_PAGE_COUNT as PageId {
        direct.write_page(id, &pages[id as usize])?;
    }
    direct.flush()?;

    Ok(active)
}

fn empty_metadata_layout() -> Result<(
    RecoveredMetadata,
    [[u8; PAGE_SIZE]; FIRST_DATA_PAGE_ID as usize],
)> {
    let allocation_bitmap_a = AllocationBitmap::new(1, SuperblockSlot::A.bitmap_slot())?;
    let allocation_bitmap_b = AllocationBitmap::new(1, SuperblockSlot::B.bitmap_slot())?;
    let superblock_a = Superblock::new(SuperblockSlot::A);
    let superblock_b = Superblock::new(SuperblockSlot::B);
    let bitmap_pages_a = allocation_bitmap_a.encode_pages()?;
    let bitmap_pages_b = allocation_bitmap_b.encode_pages()?;
    let superblock_page_a = superblock_a.encode_page()?;
    let superblock_page_b = superblock_b.encode_page()?;
    let mut pages = [[0; PAGE_SIZE]; FIRST_DATA_PAGE_ID as usize];
    pages[superblock_a.slot.page_id() as usize] = superblock_page_a;
    pages[superblock_b.slot.page_id() as usize] = superblock_page_b;
    for (slot, bitmap_pages) in [
        (superblock_a.bitmap_slot, &bitmap_pages_a),
        (superblock_b.bitmap_slot, &bitmap_pages_b),
    ] {
        for (chunk, bytes) in bitmap_pages.iter().enumerate() {
            pages[slot.page_id(chunk) as usize] = *bytes;
        }
    }

    Ok((
        RecoveredMetadata {
            superblock: superblock_a,
            allocation_bitmap: allocation_bitmap_a,
        },
        pages,
    ))
}

fn is_recoverable_empty_bootstrap<D: PageDevice>(device: &SharedPageDevice<D>) -> Result<bool> {
    let page_count = device.page_count();
    if page_count > FIRST_DATA_PAGE_ID {
        return Ok(false);
    }
    let (_, expected) = empty_metadata_layout()?;
    let mut direct = device.clone();
    let mut actual = [0; PAGE_SIZE];
    for id in 0..page_count {
        direct.read_page(id, &mut actual)?;
        if actual
            .iter()
            .zip(expected[id as usize].iter())
            .any(|(actual, expected)| *actual != 0 && actual != expected)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn recover_device_metadata<D: PageDevice>(
    device: &SharedPageDevice<D>,
) -> Result<RecoveredMetadata> {
    let page_count = device.page_count();
    if page_count < FIRST_DATA_PAGE_ID {
        return Err(pager_error(format!(
            "A non-empty page device must contain all {FIRST_DATA_PAGE_ID} metadata pages, not {page_count}"
        )));
    }
    let mut pages = [[0; PAGE_SIZE]; FIRST_DATA_PAGE_ID as usize];
    let mut direct = device.clone();
    for (id, destination) in pages.iter_mut().enumerate() {
        direct.read_page(id as PageId, destination)?;
    }
    let slot_a = RawMetadataSlot::new(
        &pages[SuperblockSlot::A.page_id() as usize],
        [
            &pages[SuperblockSlot::A.bitmap_slot().page_id(0) as usize],
            &pages[SuperblockSlot::A.bitmap_slot().page_id(1) as usize],
            &pages[SuperblockSlot::A.bitmap_slot().page_id(2) as usize],
        ],
    );
    let slot_b = RawMetadataSlot::new(
        &pages[SuperblockSlot::B.page_id() as usize],
        [
            &pages[SuperblockSlot::B.bitmap_slot().page_id(0) as usize],
            &pages[SuperblockSlot::B.bitmap_slot().page_id(1) as usize],
            &pages[SuperblockSlot::B.bitmap_slot().page_id(2) as usize],
        ],
    );
    recover_metadata(slot_a, slot_b)?.ok_or_else(|| {
        pager_error("A non-empty page device does not contain a recoverable metadata root")
    })
}

fn validate_physical_coverage<D: PageDevice>(
    device: &SharedPageDevice<D>,
    allocation_bitmap: &AllocationBitmap,
) -> Result<()> {
    let page_count = device.page_count();
    for id in FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT {
        if allocation_bitmap.is_allocated(id)? && id >= page_count {
            return Err(pager_error(format!(
                "Allocation bitmap references page {id}, but the physical device contains only {page_count} pages"
            )));
        }
    }
    Ok(())
}

fn ensure_data_page(id: PageId) -> Result<()> {
    if !(FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT).contains(&id) {
        return Err(pager_error(format!(
            "Data page ID {id} must be between {FIRST_DATA_PAGE_ID} and {}",
            MAX_PAGE_COUNT - 1
        )));
    }
    Ok(())
}

fn decode_expected_page(id: PageId, bytes: &[u8; PAGE_SIZE]) -> Result<Page> {
    let page = Page::decode(bytes)?;
    if page.id != id {
        return Err(pager_error(format!(
            "Physical page {id} contains an envelope for page {}",
            page.id
        )));
    }
    Ok(page)
}

fn page_not_allocated(id: PageId) -> EngineError {
    EngineError::new(
        "PAGE_NOT_ALLOCATED",
        format!("Data page {id} is not allocated in this root"),
    )
}

fn pager_error(message: impl Into<String>) -> EngineError {
    EngineError::new("PAGER_ERROR", message)
}

fn recovery_required(message: impl Into<String>) -> EngineError {
    EngineError::new("RECOVERY_REQUIRED", message)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, rc::Rc};

    use super::*;
    use crate::{BitmapSlot, MAX_PAGE_CACHE_BYTES, MemoryPageDevice, PageType};

    fn leaf(id: PageId, value: u8) -> Page {
        Page::new(id, PageType::BtreeLeaf, vec![value]).unwrap()
    }

    #[test]
    fn creates_reopens_and_reads_a_committed_root() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        assert_eq!(pager.physical_page_count(), FIRST_DATA_PAGE_ID);
        assert_eq!(pager.catalog_root_page_id(), None);
        assert_eq!(pager.generation(), 1);

        let root;
        {
            let mut transaction = pager.begin_write().unwrap();
            root = transaction.allocate_page().unwrap();
            transaction.write_new_page(&leaf(root, 7)).unwrap();
            assert_eq!(transaction.read_page(root).unwrap().payload, vec![7]);
            transaction.commit(1, 4, Some(root)).unwrap();
        }
        assert_eq!(pager.read_page(root).unwrap().payload, vec![7]);
        assert_eq!(pager.database_revision(), 1);
        assert_eq!(pager.applied_journal_sequence(), 4);
        assert_eq!(pager.generation(), 2);

        let device = pager.into_device();
        let mut reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(reopened.catalog_root_page_id(), Some(root));
        assert_eq!(reopened.read_page(root).unwrap().payload, vec![7]);
    }

    #[test]
    fn revision_exhaustion_rejects_publication_before_device_io() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let root;
        {
            let mut transaction = pager.begin_write().unwrap();
            root = transaction.allocate_page().unwrap();
            transaction.write_new_page(&leaf(root, 7)).unwrap();
            transaction
                .commit(crate::revision::MAX_DATABASE_REVISION, 0, Some(root))
                .unwrap();
        }
        let flushes = pager.device.0.borrow().flush_count();
        let transaction = pager.begin_write().unwrap();
        assert_eq!(
            transaction
                .commit(crate::revision::MAX_DATABASE_REVISION + 1, 0, Some(root),)
                .unwrap_err()
                .code,
            "REVISION_OVERFLOW"
        );
        assert_eq!(pager.device.0.borrow().flush_count(), flushes);
        assert!(!pager.is_recovery_required());
        assert_eq!(
            pager.database_revision(),
            crate::revision::MAX_DATABASE_REVISION
        );

        let device = pager.into_device();
        let reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(
            reopened.database_revision(),
            crate::revision::MAX_DATABASE_REVISION
        );
        assert_eq!(reopened.catalog_root_page_id(), Some(root));
    }

    #[test]
    fn abort_leaves_an_unreachable_orphan_which_can_be_reused() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let orphan;
        {
            let mut transaction = pager.begin_write().unwrap();
            orphan = transaction.allocate_page().unwrap();
            transaction.write_new_page(&leaf(orphan, 7)).unwrap();
            transaction.abort();
        }
        assert_eq!(pager.catalog_root_page_id(), None);
        assert_eq!(
            pager.read_page(orphan).unwrap_err().code,
            "PAGE_NOT_ALLOCATED"
        );
        assert_eq!(pager.physical_page_count(), orphan + 1);

        let mut transaction = pager.begin_write().unwrap();
        assert_eq!(transaction.allocate_page().unwrap(), orphan);
        transaction.write_new_page(&leaf(orphan, 8)).unwrap();
        transaction.commit(1, 1, Some(orphan)).unwrap();
        assert_eq!(pager.read_page(orphan).unwrap().payload, vec![8]);
    }

    #[test]
    fn refuses_unwritten_pages_backward_metadata_and_freed_roots_before_io() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let transaction = pager.begin_write().unwrap();
        assert_eq!(
            transaction
                .commit(0, 0, Some(FIRST_DATA_PAGE_ID))
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );

        let mut transaction = pager.begin_write().unwrap();
        let page = transaction.allocate_page().unwrap();
        assert_eq!(
            transaction.commit(1, 1, Some(page)).unwrap_err().code,
            "PAGER_ERROR"
        );

        let mut transaction = pager.begin_write().unwrap();
        let page = transaction.allocate_page().unwrap();
        transaction.write_new_page(&leaf(page, 1)).unwrap();
        transaction.commit(2, 2, Some(page)).unwrap();

        let mut transaction = pager.begin_write().unwrap();
        transaction.free_shared_page(page).unwrap();
        assert_eq!(
            transaction.commit(1, 2, Some(page)).unwrap_err().code,
            "INVALID_PAGE"
        );
        assert!(!pager.is_recovery_required());
        assert_eq!(pager.read_page(page).unwrap().payload, vec![1]);
    }

    #[test]
    fn successful_install_purges_freed_committed_cache_entries() {
        let mut pager =
            Pager::with_cache_capacity(MemoryPageDevice::new(0).unwrap(), 2 * PAGE_SIZE).unwrap();
        let (root, disposable);
        {
            let mut transaction = pager.begin_write().unwrap();
            root = transaction.allocate_page().unwrap();
            disposable = transaction.allocate_page().unwrap();
            transaction.write_new_page(&leaf(root, 1)).unwrap();
            transaction.write_new_page(&leaf(disposable, 2)).unwrap();
            transaction.commit(1, 1, Some(root)).unwrap();
        }
        assert_eq!(pager.read_page(disposable).unwrap().payload, vec![2]);
        {
            let mut transaction = pager.begin_write().unwrap();
            transaction.free_shared_page(disposable).unwrap();
            transaction.commit(2, 2, Some(root)).unwrap();
        }

        let mut transaction = pager.begin_write().unwrap();
        assert_eq!(transaction.allocate_page().unwrap(), disposable);
        transaction.write_new_page(&leaf(disposable, 9)).unwrap();
        transaction.free_shared_page(root).unwrap();
        transaction.commit(3, 3, Some(disposable)).unwrap();
        assert_eq!(pager.read_page(disposable).unwrap().payload, vec![9]);
    }

    #[test]
    fn released_new_page_is_reused_and_does_not_need_initialization() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        let released = transaction.allocate_page().unwrap();
        transaction.write_new_page(&leaf(released, 1)).unwrap();
        transaction.release_new_page(released).unwrap();
        assert!(!transaction.owns_page(released));
        assert_eq!(transaction.allocate_page().unwrap(), released);
        transaction.write_new_page(&leaf(released, 2)).unwrap();
        transaction.commit(1, 1, Some(released)).unwrap();
        assert_eq!(pager.read_page(released).unwrap().payload, vec![2]);
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailureTiming {
        Before,
        After,
    }

    #[derive(Clone, Debug)]
    struct Failure {
        operation: usize,
        timing: FailureTiming,
    }

    #[derive(Debug, Default)]
    struct FaultState {
        working_pages: Vec<[u8; PAGE_SIZE]>,
        durable_pages: Vec<[u8; PAGE_SIZE]>,
        operation: usize,
        failure: Option<Failure>,
    }

    #[derive(Clone, Debug, Default)]
    struct FaultDevice(Rc<RefCell<FaultState>>);

    impl FaultDevice {
        fn arm(&self, operation: usize, timing: FailureTiming) {
            let mut state = self.0.borrow_mut();
            state.operation = 0;
            state.failure = Some(Failure { operation, timing });
        }

        fn disarm(&self) {
            let mut state = self.0.borrow_mut();
            state.operation = 0;
            state.failure = None;
        }

        fn crash(&self) {
            let mut state = self.0.borrow_mut();
            state.working_pages = state.durable_pages.clone();
            state.operation = 0;
            state.failure = None;
        }

        fn before(&self) -> Result<Option<FailureTiming>> {
            let mut state = self.0.borrow_mut();
            state.operation += 1;
            let timing = state
                .failure
                .as_ref()
                .filter(|failure| failure.operation == state.operation)
                .map(|failure| failure.timing);
            if timing == Some(FailureTiming::Before) {
                state.failure = None;
                Err(EngineError::new("INJECTED_IO", "failure before operation"))
            } else {
                Ok(timing)
            }
        }

        fn after(&self, timing: Option<FailureTiming>) -> Result<()> {
            if timing == Some(FailureTiming::After) {
                self.0.borrow_mut().failure = None;
                Err(EngineError::new("INJECTED_IO", "failure after operation"))
            } else {
                Ok(())
            }
        }
    }

    impl PageDevice for FaultDevice {
        fn page_count(&self) -> PageId {
            self.0.borrow().working_pages.len() as PageId
        }

        fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
            if destination.len() != PAGE_SIZE || id >= self.page_count() {
                return Err(EngineError::new("FAULT_DEVICE", "invalid read"));
            }
            destination.copy_from_slice(&self.0.borrow().working_pages[id as usize]);
            Ok(())
        }

        fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
            let timing = self.before()?;
            if source.len() != PAGE_SIZE {
                return Err(EngineError::new("FAULT_DEVICE", "invalid write"));
            }
            let mut state = self.0.borrow_mut();
            if id == state.working_pages.len() as PageId && id < MAX_PAGE_COUNT {
                state.working_pages.push([0; PAGE_SIZE]);
            } else if id >= state.working_pages.len() as PageId {
                return Err(EngineError::new("FAULT_DEVICE", "non-dense write"));
            }
            state.working_pages[id as usize].copy_from_slice(source);
            drop(state);
            self.after(timing)
        }

        fn flush(&mut self) -> Result<()> {
            let timing = self.before()?;
            let mut state = self.0.borrow_mut();
            state.durable_pages = state.working_pages.clone();
            drop(state);
            self.after(timing)
        }
    }

    #[test]
    fn every_interrupted_empty_bootstrap_can_be_reopened_and_completed() {
        // Initialization performs eight zero writes, six bitmap writes, a bitmap flush, two
        // superblock writes, and a final flush. A crash after each possible before/after failure
        // may leave a bytewise mix of zero pages and exact generation-1 empty metadata pages.
        for operation in 1..=18 {
            for timing in [FailureTiming::Before, FailureTiming::After] {
                let device = FaultDevice::default();
                device.arm(operation, timing);
                let result = Pager::open_or_create(device.clone());
                if result.is_ok() {
                    assert_eq!(operation, 18);
                    assert_eq!(timing, FailureTiming::After);
                }
                drop(result);
                device.crash();

                let pager = Pager::open_or_create(device.clone()).unwrap_or_else(|error| {
                    panic!("bootstrap cut {operation} {timing:?} did not recover: {error}")
                });
                assert_eq!(pager.generation(), 1);
                assert_eq!(pager.catalog_root_page_id(), None);
                assert_eq!(pager.physical_page_count(), FIRST_DATA_PAGE_ID);
                drop(pager);
            }
        }
    }

    #[test]
    fn torn_empty_bootstrap_pages_can_be_reopened_and_completed() {
        let (_, expected) = empty_metadata_layout().unwrap();
        for (page_id, expected_page) in expected.iter().enumerate() {
            for expected_to_zero in [false, true] {
                for split in [1, PAGE_SIZE / 2, PAGE_SIZE - 1] {
                    let device = FaultDevice::default();
                    {
                        let mut state = device.0.borrow_mut();
                        state.working_pages = vec![[0; PAGE_SIZE]; FIRST_DATA_PAGE_ID as usize];
                        if expected_to_zero {
                            state.working_pages[page_id] = *expected_page;
                            state.working_pages[page_id][..split].fill(0);
                        } else {
                            state.working_pages[page_id][..split]
                                .copy_from_slice(&expected_page[..split]);
                        }
                        state.durable_pages = state.working_pages.clone();
                    }

                    let pager = Pager::open_or_create(device).unwrap_or_else(|error| {
                        panic!(
                            "torn bootstrap page {page_id} at byte {split} (expected_to_zero={expected_to_zero}) did not recover: {error}"
                        )
                    });
                    assert_eq!(pager.generation(), 1);
                    assert_eq!(pager.catalog_root_page_id(), None);
                    assert_eq!(pager.physical_page_count(), FIRST_DATA_PAGE_ID);
                }
            }
        }
    }

    #[test]
    fn bootstrap_recovery_fails_closed_on_non_deterministic_partial_metadata() {
        for page_count in [1, FIRST_DATA_PAGE_ID] {
            let device = FaultDevice::default();
            {
                let mut state = device.0.borrow_mut();
                state.working_pages = vec![[0; PAGE_SIZE]; page_count as usize];
                state.working_pages[(page_count - 1) as usize][17] = 1;
                state.durable_pages = state.working_pages.clone();
            }
            let error = match Pager::open_or_create(device) {
                Ok(_) => panic!("foreign partial metadata must not be rewritten"),
                Err(error) => error,
            };
            assert!(matches!(
                error.code.as_str(),
                "PAGER_ERROR" | "INVALID_PAGE"
            ));
        }
    }

    fn committed_fault_device() -> (FaultDevice, PageId) {
        let device = FaultDevice::default();
        let mut pager = Pager::open_or_create(device.clone()).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        let root = transaction.allocate_page().unwrap();
        transaction.write_new_page(&leaf(root, 1)).unwrap();
        transaction.commit(1, 1, Some(root)).unwrap();
        drop(pager);
        device.disarm();
        (device, root)
    }

    fn prepare_replacement<'a>(
        pager: &'a mut Pager<FaultDevice>,
        old_root: PageId,
    ) -> (PagerWriteTransaction<'a, FaultDevice>, PageId) {
        let mut transaction = pager.begin_write().unwrap();
        let new_root = transaction.allocate_page().unwrap();
        transaction.write_new_page(&leaf(new_root, 2)).unwrap();
        transaction.free_shared_page(old_root).unwrap();
        (transaction, new_root)
    }

    #[test]
    fn every_pre_superblock_publication_cut_reopens_the_old_root() {
        // commit operations: data write, data flush, three bitmap writes, bitmap flush,
        // superblock write, superblock flush.
        for operation in 1..=6 {
            for timing in [FailureTiming::Before, FailureTiming::After] {
                let (device, old_root) = committed_fault_device();
                let mut pager = Pager::open_or_create(device.clone()).unwrap();
                let (transaction, new_root) = prepare_replacement(&mut pager, old_root);
                device.arm(operation, timing);
                assert_ne!(
                    transaction.commit(2, 2, Some(new_root)).unwrap_err().code,
                    "RECOVERY_REQUIRED"
                );
                assert!(!pager.is_recovery_required());
                drop(pager);
                device.crash();

                let mut reopened = Pager::open_or_create(device).unwrap();
                assert_eq!(reopened.catalog_root_page_id(), Some(old_root));
                assert_eq!(reopened.read_page(old_root).unwrap().payload, vec![1]);
            }
        }
    }

    #[test]
    fn ambiguous_superblock_cuts_poison_and_reopen_the_old_or_new_valid_root() {
        for (operation, timing, expected_value) in [
            (7, FailureTiming::Before, 1),
            (7, FailureTiming::After, 1),
            (8, FailureTiming::Before, 1),
            (8, FailureTiming::After, 2),
        ] {
            let (device, old_root) = committed_fault_device();
            let mut pager = Pager::open_or_create(device.clone()).unwrap();
            let (transaction, new_root) = prepare_replacement(&mut pager, old_root);
            device.arm(operation, timing);
            assert_eq!(
                transaction.commit(2, 2, Some(new_root)).unwrap_err().code,
                "RECOVERY_REQUIRED"
            );
            assert!(pager.is_recovery_required());
            assert_eq!(
                pager.read_page(old_root).unwrap_err().code,
                "RECOVERY_REQUIRED"
            );
            drop(pager);
            device.crash();

            let mut reopened = Pager::open_or_create(device).unwrap();
            let expected_root = if expected_value == 1 {
                old_root
            } else {
                new_root
            };
            assert_eq!(reopened.catalog_root_page_id(), Some(expected_root));
            assert_eq!(
                reopened.read_page(expected_root).unwrap().payload,
                vec![expected_value]
            );
        }
    }

    #[test]
    fn cache_install_failure_after_publication_requires_recovery() {
        let (device, old_root) = committed_fault_device();
        let mut pager = Pager::open_or_create(device.clone()).unwrap();
        pager.fail_install_once = true;
        let (transaction, new_root) = prepare_replacement(&mut pager, old_root);
        assert_eq!(
            transaction.commit(2, 2, Some(new_root)).unwrap_err().code,
            "RECOVERY_REQUIRED"
        );
        assert!(pager.is_recovery_required());
        drop(pager);
        device.crash();

        let mut reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(reopened.catalog_root_page_id(), Some(new_root));
        assert_eq!(reopened.read_page(new_root).unwrap().payload, vec![2]);
    }

    #[test]
    fn recovery_ignores_trailing_unallocated_orphans() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        let root = transaction.allocate_page().unwrap();
        transaction.write_new_page(&leaf(root, 1)).unwrap();
        transaction.commit(1, 1, Some(root)).unwrap();
        let mut device = pager.into_device();
        let orphan = device.page_count();
        device.write_page(orphan, &[9; PAGE_SIZE]).unwrap();

        let mut reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(reopened.catalog_root_page_id(), Some(root));
        assert_eq!(reopened.read_page(root).unwrap().payload, vec![1]);
    }

    #[test]
    fn recovery_rejects_an_allocated_page_past_physical_eof() {
        let pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut device = pager.into_device();
        let missing = FIRST_DATA_PAGE_ID + 9;
        let mut bitmap = AllocationBitmap::new(2, BitmapSlot::B).unwrap();
        bitmap.set_allocated(missing, true).unwrap();
        let superblock = Superblock {
            slot: SuperblockSlot::B,
            generation: 2,
            database_revision: 1,
            applied_journal_sequence: 1,
            bitmap_slot: BitmapSlot::B,
            bitmap_generation: 2,
            catalog_root_page_id: Some(missing),
            live_data_page_count: 1,
            max_page_count: MAX_PAGE_COUNT,
        };
        for (chunk, page) in bitmap.encode_pages().unwrap().iter().enumerate() {
            device
                .write_page(BitmapSlot::B.page_id(chunk), page)
                .unwrap();
        }
        device
            .write_page(
                SuperblockSlot::B.page_id(),
                &superblock.encode_page().unwrap(),
            )
            .unwrap();
        let error = match Pager::open_or_create(device) {
            Ok(_) => panic!("metadata past physical EOF must fail recovery"),
            Err(error) => error,
        };
        assert_eq!(error.code, "PAGER_ERROR");
    }

    #[derive(Debug)]
    struct SparseFullDevice {
        pages: HashMap<PageId, [u8; PAGE_SIZE]>,
    }

    impl PageDevice for SparseFullDevice {
        fn page_count(&self) -> PageId {
            MAX_PAGE_COUNT
        }

        fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
            destination.copy_from_slice(self.pages.get(&id).unwrap_or(&[0; PAGE_SIZE]));
            Ok(())
        }

        fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
            self.pages.insert(id, source.try_into().unwrap());
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn allocator_reports_the_fixed_page_cap_without_allocating_256_mib_in_tests() {
        let mut bitmap = AllocationBitmap::new(1, BitmapSlot::A).unwrap();
        for id in FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT {
            bitmap.set_allocated(id, true).unwrap();
        }
        let superblock = Superblock {
            slot: SuperblockSlot::A,
            generation: 1,
            database_revision: 1,
            applied_journal_sequence: 1,
            bitmap_slot: BitmapSlot::A,
            bitmap_generation: 1,
            catalog_root_page_id: Some(FIRST_DATA_PAGE_ID),
            live_data_page_count: (MAX_PAGE_COUNT - FIRST_DATA_PAGE_ID) as u32,
            max_page_count: MAX_PAGE_COUNT,
        };
        let mut pages = HashMap::new();
        pages.insert(
            SuperblockSlot::A.page_id(),
            superblock.encode_page().unwrap(),
        );
        for (chunk, page) in bitmap.encode_pages().unwrap().into_iter().enumerate() {
            pages.insert(BitmapSlot::A.page_id(chunk), page);
        }
        let mut pager = Pager::open_or_create(SparseFullDevice { pages }).unwrap();
        assert_eq!(
            pager
                .begin_write()
                .unwrap()
                .allocate_page()
                .unwrap_err()
                .code,
            "DATABASE_FULL"
        );
    }

    #[test]
    fn validates_cache_capacity_before_initializing_a_new_device() {
        let device = FaultDevice::default();
        let error =
            match Pager::with_cache_capacity(device.clone(), MAX_PAGE_CACHE_BYTES + PAGE_SIZE) {
                Ok(_) => panic!("an oversized cache must be rejected"),
                Err(error) => error,
            };
        assert_eq!(error.code, "PAGE_CACHE_ERROR");
        assert_eq!(device.page_count(), 0);
    }
}
