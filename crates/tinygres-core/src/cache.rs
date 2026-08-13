use std::collections::{HashMap, HashSet};

use crate::{
    AllocationBitmap, EngineError, FIRST_DATA_PAGE_ID, PAGE_SIZE, PageDevice, PageId, Result,
};

pub type CandidateId = u64;

pub const DEFAULT_PAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PAGE_CACHE_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_PAGE_CACHE_PAGES: usize = DEFAULT_PAGE_CACHE_BYTES / PAGE_SIZE;
pub const MAX_PAGE_CACHE_PAGES: usize = MAX_PAGE_CACHE_BYTES / PAGE_SIZE;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Owner {
    Committed,
    Candidate(CandidateId),
}

#[derive(Debug)]
struct CacheEntry {
    id: PageId,
    owner: Owner,
    bytes: Box<[u8; PAGE_SIZE]>,
    referenced: bool,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Reservation {
    candidate: CandidateId,
    written: bool,
}

#[derive(Debug)]
pub struct PageCache<D: PageDevice> {
    device: D,
    entries: Vec<CacheEntry>,
    lookup: HashMap<(Owner, PageId), usize>,
    reservations: HashMap<PageId, Reservation>,
    /// Owners with pages which reached the device but have not been covered by a successful
    /// device-wide durability barrier. A PageDevice flush is global, so one successful flush can
    /// make writes from several interleaved owners durable at once.
    unflushed_owners: HashSet<Owner>,
    capacity: usize,
    hand: usize,
}

impl<D: PageDevice> PageCache<D> {
    pub fn new(device: D) -> Result<Self> {
        Self::with_capacity(device, DEFAULT_PAGE_CACHE_BYTES)
    }

    pub fn with_capacity(device: D, capacity_bytes: usize) -> Result<Self> {
        if !(PAGE_SIZE..=MAX_PAGE_CACHE_BYTES).contains(&capacity_bytes)
            || !capacity_bytes.is_multiple_of(PAGE_SIZE)
        {
            return Err(cache_error(format!(
                "Page cache capacity must be a multiple of {PAGE_SIZE} bytes between {PAGE_SIZE} and {MAX_PAGE_CACHE_BYTES}"
            )));
        }
        let capacity = capacity_bytes / PAGE_SIZE;
        Ok(Self {
            device,
            entries: Vec::with_capacity(capacity),
            lookup: HashMap::with_capacity(capacity),
            reservations: HashMap::new(),
            unflushed_owners: HashSet::new(),
            capacity,
            hand: 0,
        })
    }

    pub fn capacity_pages(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    pub fn read_page(&mut self, id: PageId) -> Result<&[u8; PAGE_SIZE]> {
        self.ensure_not_reserved(id)?;
        self.read_owned(Owner::Committed, id)
    }

    /// Reads a page ID allocated exclusively to this copy-on-write candidate.
    /// Shared committed page IDs must be read with [`Self::read_page`].
    pub fn read_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
    ) -> Result<&[u8; PAGE_SIZE]> {
        self.ensure_reserved_by(candidate, id)?;
        if !self
            .reservations
            .get(&id)
            .expect("the candidate reservation was checked")
            .written
        {
            return Err(cache_error(format!(
                "Candidate {candidate} must initialize reserved page {id} before reading it"
            )));
        }
        self.read_owned(Owner::Candidate(candidate), id)
    }

    /// Reserves an unallocated data page exclusively for a copy-on-write candidate.
    ///
    /// Reservation ownership is independent of cache residency: evicting a candidate page does
    /// not make the physical page available to another candidate. The active allocation bitmap is
    /// consulted on every reservation so a candidate can never overwrite a page reachable from
    /// the committed superblock.
    pub fn reserve_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
        active_bitmap: &AllocationBitmap,
    ) -> Result<()> {
        if id < FIRST_DATA_PAGE_ID {
            return Err(cache_error(format!(
                "Candidate pages must not use metadata page ID {id}"
            )));
        }
        if active_bitmap.is_allocated(id)? {
            return Err(cache_error(format!(
                "Candidate {candidate} cannot reserve allocated page {id}"
            )));
        }
        if self.lookup.contains_key(&(Owner::Committed, id)) {
            return Err(cache_error(format!(
                "Candidate {candidate} cannot reserve page {id} while it is present in the committed cache view"
            )));
        }
        if let Some(reservation) = self
            .reservations
            .values()
            .find(|reservation| reservation.candidate != candidate)
        {
            return Err(cache_error(format!(
                "Candidate {candidate} cannot reserve page {id}; candidate {} is already active",
                reservation.candidate
            )));
        }
        match self.reservations.get(&id) {
            Some(reservation) if reservation.candidate == candidate => Ok(()),
            Some(reservation) => Err(cache_error(format!(
                "Candidate {candidate} cannot reserve page {id}; candidate {} already reserved it",
                reservation.candidate
            ))),
            None => {
                self.reservations.insert(
                    id,
                    Reservation {
                        candidate,
                        written: false,
                    },
                );
                Ok(())
            }
        }
    }

    pub fn write_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
        bytes: &[u8],
    ) -> Result<()> {
        self.ensure_reserved_by(candidate, id)?;
        self.write_owned(Owner::Candidate(candidate), id, bytes)?;
        self.reservations
            .get_mut(&id)
            .expect("the candidate reservation was checked")
            .written = true;
        Ok(())
    }

    /// Releases one page reserved by a candidate after its owner has proved that no candidate
    /// page references it. A previously-evicted write may remain as an unreachable physical
    /// orphan, but it is removed from the candidate's logical allocation set by the pager.
    pub fn release_candidate_page(&mut self, candidate: CandidateId, id: PageId) -> Result<()> {
        self.ensure_reserved_by(candidate, id)?;
        self.reservations.remove(&id);
        self.entries
            .retain(|entry| entry.owner != Owner::Candidate(candidate) || entry.id != id);
        if !self
            .reservations
            .values()
            .any(|reservation| reservation.candidate == candidate)
        {
            self.unflushed_owners.remove(&Owner::Candidate(candidate));
        }
        self.rebuild_lookup();
        Ok(())
    }

    pub fn flush_candidate(&mut self, candidate: CandidateId) -> Result<()> {
        self.flush_owner(Owner::Candidate(candidate))
    }

    pub fn install_candidate(
        &mut self,
        candidate: CandidateId,
        next_bitmap: &AllocationBitmap,
    ) -> Result<()> {
        let owner = Owner::Candidate(candidate);
        if self
            .entries
            .iter()
            .any(|entry| entry.owner == owner && entry.dirty)
            || self.unflushed_owners.contains(&owner)
        {
            return Err(cache_error(format!(
                "Candidate {candidate} must be flushed before it is installed"
            )));
        }

        let candidate_ids = self
            .reservations
            .iter()
            .filter(|(_, reservation)| reservation.candidate == candidate)
            .map(|(id, reservation)| (*id, reservation.written))
            .collect::<Vec<_>>();
        if candidate_ids
            .iter()
            .any(|(id, _)| self.lookup.contains_key(&(Owner::Committed, *id)))
        {
            return Err(cache_error(format!(
                "Candidate {candidate} overlaps a page in the committed cache view"
            )));
        }
        if let Some((id, _)) = candidate_ids.iter().find(|(_, written)| !written) {
            return Err(cache_error(format!(
                "Candidate {candidate} reserved page {id} but never wrote it"
            )));
        }
        for (id, _) in &candidate_ids {
            if !next_bitmap.is_allocated(*id)? {
                return Err(cache_error(format!(
                    "Candidate {candidate} page {id} is missing from the next allocation bitmap"
                )));
            }
        }
        let mut deallocated = HashSet::new();
        for entry in &self.entries {
            if entry.owner == Owner::Committed && !next_bitmap.is_allocated(entry.id)? {
                deallocated.insert(entry.id);
            }
        }
        self.entries
            .retain(|entry| entry.owner != Owner::Committed || !deallocated.contains(&entry.id));
        for entry in &mut self.entries {
            if entry.owner == owner {
                entry.owner = Owner::Committed;
                entry.referenced = true;
            }
        }
        self.reservations
            .retain(|_, reservation| reservation.candidate != candidate);
        self.rebuild_lookup();
        Ok(())
    }

    pub fn invalidate_candidate(&mut self, candidate: CandidateId) {
        self.unflushed_owners.remove(&Owner::Candidate(candidate));
        self.entries
            .retain(|entry| entry.owner != Owner::Candidate(candidate));
        self.reservations
            .retain(|_, reservation| reservation.candidate != candidate);
        self.rebuild_lookup();
    }

    pub fn candidate_page_count(&self, candidate: CandidateId) -> usize {
        self.reservations
            .iter()
            .filter(|(_, reservation)| reservation.candidate == candidate)
            .count()
    }

    fn ensure_not_reserved(&self, id: PageId) -> Result<()> {
        if let Some(reservation) = self.reservations.get(&id) {
            return Err(cache_error(format!(
                "Page {id} is exclusively reserved by candidate {}",
                reservation.candidate
            )));
        }
        Ok(())
    }

    fn ensure_reserved_by(&self, candidate: CandidateId, id: PageId) -> Result<()> {
        match self.reservations.get(&id) {
            Some(reservation) if reservation.candidate == candidate => Ok(()),
            Some(reservation) => Err(cache_error(format!(
                "Page {id} is reserved by candidate {}, not candidate {candidate}",
                reservation.candidate
            ))),
            None => Err(cache_error(format!(
                "Candidate {candidate} must reserve page {id} before accessing it"
            ))),
        }
    }

    fn read_owned(&mut self, owner: Owner, id: PageId) -> Result<&[u8; PAGE_SIZE]> {
        if let Some(index) = self.lookup.get(&(owner, id)).copied() {
            self.entries[index].referenced = true;
            return Ok(&self.entries[index].bytes);
        }
        let mut bytes = Box::new([0; PAGE_SIZE]);
        self.device.read_page(id, bytes.as_mut_slice())?;
        let index = self.insert(CacheEntry {
            id,
            owner,
            bytes,
            referenced: true,
            dirty: false,
        })?;
        Ok(&self.entries[index].bytes)
    }

    fn write_owned(&mut self, owner: Owner, id: PageId, bytes: &[u8]) -> Result<()> {
        if bytes.len() != PAGE_SIZE {
            return Err(cache_error(format!(
                "Cached pages must be exactly {PAGE_SIZE} bytes, not {}",
                bytes.len()
            )));
        }
        if let Some(index) = self.lookup.get(&(owner, id)).copied() {
            self.entries[index].bytes.copy_from_slice(bytes);
            self.entries[index].referenced = true;
            self.entries[index].dirty = true;
            return Ok(());
        }
        self.insert(CacheEntry {
            id,
            owner,
            bytes: Box::new(bytes.try_into().expect("validated page length")),
            referenced: true,
            dirty: true,
        })?;
        Ok(())
    }

    fn insert(&mut self, entry: CacheEntry) -> Result<usize> {
        if self.entries.len() < self.capacity {
            let index = self.entries.len();
            self.lookup.insert((entry.owner, entry.id), index);
            self.entries.push(entry);
            return Ok(index);
        }

        let index = self.find_victim()?;
        let previous = &self.entries[index];
        self.lookup.remove(&(previous.owner, previous.id));
        self.lookup.insert((entry.owner, entry.id), index);
        self.entries[index] = entry;
        self.hand = (index + 1) % self.entries.len();
        Ok(index)
    }

    fn find_victim(&mut self) -> Result<usize> {
        let attempts = self.entries.len() * 2;
        for _ in 0..attempts {
            let index = self.hand;
            self.hand = (self.hand + 1) % self.entries.len();
            let entry = &mut self.entries[index];
            if entry.referenced {
                entry.referenced = false;
                continue;
            }
            if entry.dirty {
                self.device.write_page(entry.id, entry.bytes.as_slice())?;
                entry.dirty = false;
                self.unflushed_owners.insert(entry.owner);
            }
            return Ok(index);
        }
        Err(cache_error(
            "Page cache could not select an eviction candidate",
        ))
    }

    fn flush_owner(&mut self, owner: Owner) -> Result<()> {
        for entry in &mut self.entries {
            if entry.owner == owner && entry.dirty {
                self.device.write_page(entry.id, entry.bytes.as_slice())?;
                entry.dirty = false;
                self.unflushed_owners.insert(entry.owner);
            }
        }
        self.device.flush()?;
        self.unflushed_owners.clear();
        Ok(())
    }

    fn rebuild_lookup(&mut self) {
        self.lookup.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            self.lookup.insert((entry.owner, entry.id), index);
        }
        if self.entries.is_empty() {
            self.hand = 0;
        } else {
            self.hand %= self.entries.len();
        }
    }
}

fn cache_error(message: impl Into<String>) -> EngineError {
    EngineError::new("PAGE_CACHE_ERROR", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BitmapSlot, MemoryPageDevice};

    fn active_bitmap() -> AllocationBitmap {
        AllocationBitmap::new(1, BitmapSlot::A).unwrap()
    }

    fn bitmap_allocating(active: &AllocationBitmap, ids: &[PageId]) -> AllocationBitmap {
        let mut next = active.clone();
        for id in ids {
            next.set_allocated(*id, true).unwrap();
        }
        next
    }

    #[test]
    fn cache_enforces_capacity_bounds_without_eagerly_allocating_pages() {
        let default = PageCache::new(MemoryPageDevice::new(1).unwrap()).unwrap();
        assert_eq!(default.capacity_pages(), DEFAULT_PAGE_CACHE_PAGES);
        assert!(default.is_empty());
        assert_eq!(
            PageCache::with_capacity(MemoryPageDevice::new(1).unwrap(), PAGE_SIZE - 1)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        assert_eq!(
            PageCache::with_capacity(
                MemoryPageDevice::new(1).unwrap(),
                MAX_PAGE_CACHE_BYTES + PAGE_SIZE,
            )
            .unwrap_err()
            .code,
            "PAGE_CACHE_ERROR"
        );
    }

    #[test]
    fn clock_cache_evicts_committed_pages_without_exceeding_its_bound() {
        let mut device = MemoryPageDevice::new(3).unwrap();
        device.write_page(0, &[1; PAGE_SIZE]).unwrap();
        let mut cache = PageCache::with_capacity(device, PAGE_SIZE).unwrap();
        assert_eq!(cache.read_page(0).unwrap(), &[1; PAGE_SIZE]);
        assert_eq!(cache.read_page(1).unwrap(), &[0; PAGE_SIZE]);
        assert_eq!(cache.device().page(0).unwrap(), &[1; PAGE_SIZE]);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn candidate_access_requires_an_exclusive_unallocated_data_page_reservation() {
        let mut active = active_bitmap();
        active.set_allocated(FIRST_DATA_PAGE_ID + 1, true).unwrap();
        let mut cache = PageCache::with_capacity(
            MemoryPageDevice::new(FIRST_DATA_PAGE_ID + 3).unwrap(),
            PAGE_SIZE,
        )
        .unwrap();

        assert_eq!(
            cache
                .write_candidate_page(7, FIRST_DATA_PAGE_ID, &[7; PAGE_SIZE])
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        assert_eq!(
            cache
                .read_candidate_page(7, FIRST_DATA_PAGE_ID)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        assert_eq!(
            cache
                .reserve_candidate_page(7, FIRST_DATA_PAGE_ID - 1, &active)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        assert_eq!(
            cache
                .reserve_candidate_page(7, FIRST_DATA_PAGE_ID + 1, &active)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );

        cache
            .reserve_candidate_page(7, FIRST_DATA_PAGE_ID, &active)
            .unwrap();
        assert_eq!(
            cache
                .read_candidate_page(7, FIRST_DATA_PAGE_ID)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        cache
            .write_candidate_page(7, FIRST_DATA_PAGE_ID, &[7; PAGE_SIZE])
            .unwrap();
        assert_eq!(
            cache
                .reserve_candidate_page(8, FIRST_DATA_PAGE_ID, &active)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        assert_eq!(
            cache.read_page(FIRST_DATA_PAGE_ID).unwrap_err().code,
            "PAGE_CACHE_ERROR"
        );
    }

    #[test]
    fn reservation_survives_eviction_and_abort_releases_it_for_reuse() {
        let active = active_bitmap();
        let page_id = FIRST_DATA_PAGE_ID;
        let mut cache = PageCache::with_capacity(
            MemoryPageDevice::new(FIRST_DATA_PAGE_ID + 1).unwrap(),
            PAGE_SIZE,
        )
        .unwrap();
        cache.reserve_candidate_page(7, page_id, &active).unwrap();
        cache
            .write_candidate_page(7, page_id, &[7; PAGE_SIZE])
            .unwrap();
        assert_eq!(cache.read_page(0).unwrap(), &[0; PAGE_SIZE]);
        assert_eq!(cache.device().page(page_id).unwrap(), &[7; PAGE_SIZE]);
        assert_eq!(
            cache
                .reserve_candidate_page(8, page_id, &active)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        cache.invalidate_candidate(7);
        assert_eq!(cache.candidate_page_count(7), 0);
        cache.reserve_candidate_page(8, page_id, &active).unwrap();
        cache
            .write_candidate_page(8, page_id, &[8; PAGE_SIZE])
            .unwrap();
        cache.flush_candidate(8).unwrap();
        cache
            .install_candidate(8, &bitmap_allocating(&active, &[page_id]))
            .unwrap();
        assert_eq!(cache.read_page(page_id).unwrap(), &[8; PAGE_SIZE]);
    }

    #[test]
    fn released_candidate_page_loses_its_reservation_and_cached_view() {
        let active = active_bitmap();
        let page_id = FIRST_DATA_PAGE_ID;
        let mut cache = PageCache::with_capacity(
            MemoryPageDevice::new(FIRST_DATA_PAGE_ID + 1).unwrap(),
            PAGE_SIZE,
        )
        .unwrap();
        cache.reserve_candidate_page(7, page_id, &active).unwrap();
        cache
            .write_candidate_page(7, page_id, &[7; PAGE_SIZE])
            .unwrap();
        cache.release_candidate_page(7, page_id).unwrap();
        assert_eq!(cache.candidate_page_count(7), 0);
        assert_eq!(
            cache.read_candidate_page(7, page_id).unwrap_err().code,
            "PAGE_CACHE_ERROR"
        );
        cache.reserve_candidate_page(7, page_id, &active).unwrap();
        cache
            .write_candidate_page(7, page_id, &[8; PAGE_SIZE])
            .unwrap();
        assert_eq!(
            cache.read_candidate_page(7, page_id).unwrap(),
            &[8; PAGE_SIZE]
        );
    }

    #[test]
    fn a_candidate_larger_than_the_cache_stays_bounded_and_rereads_evicted_pages() {
        let active = active_bitmap();
        let ids = (FIRST_DATA_PAGE_ID..FIRST_DATA_PAGE_ID + 12).collect::<Vec<_>>();
        let mut cache = PageCache::with_capacity(
            MemoryPageDevice::new(FIRST_DATA_PAGE_ID + 12).unwrap(),
            PAGE_SIZE * 2,
        )
        .unwrap();
        for id in &ids {
            cache.reserve_candidate_page(21, *id, &active).unwrap();
            cache
                .write_candidate_page(21, *id, &[*id as u8; PAGE_SIZE])
                .unwrap();
            assert!(cache.len() <= 2);
        }
        assert_eq!(cache.candidate_page_count(21), ids.len());
        assert_eq!(
            cache.device().page(ids[0]).unwrap(),
            &[ids[0] as u8; PAGE_SIZE]
        );
        assert_eq!(
            cache.read_candidate_page(21, ids[0]).unwrap(),
            &[ids[0] as u8; PAGE_SIZE]
        );
        assert!(cache.len() <= 2);
        assert_eq!(
            cache
                .install_candidate(21, &bitmap_allocating(&active, &ids))
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        cache.flush_candidate(21).unwrap();
        cache
            .install_candidate(21, &bitmap_allocating(&active, &ids))
            .unwrap();
        assert_eq!(cache.read_page(ids[0]).unwrap(), &[ids[0] as u8; PAGE_SIZE]);
    }

    #[test]
    fn candidate_install_requires_durability_and_a_complete_next_bitmap() {
        let active = active_bitmap();
        let page_id = FIRST_DATA_PAGE_ID;
        let mut cache = PageCache::with_capacity(
            MemoryPageDevice::new(FIRST_DATA_PAGE_ID + 1).unwrap(),
            PAGE_SIZE * 2,
        )
        .unwrap();
        cache.reserve_candidate_page(9, page_id, &active).unwrap();
        cache
            .write_candidate_page(9, page_id, &[9; PAGE_SIZE])
            .unwrap();
        assert_eq!(
            cache.install_candidate(9, &active).unwrap_err().code,
            "PAGE_CACHE_ERROR"
        );
        cache.flush_candidate(9).unwrap();
        assert_eq!(cache.device().flush_count(), 1);
        assert_eq!(
            cache.install_candidate(9, &active).unwrap_err().code,
            "PAGE_CACHE_ERROR"
        );
        assert_eq!(cache.candidate_page_count(9), 1);
        cache
            .install_candidate(9, &bitmap_allocating(&active, &[page_id]))
            .unwrap();
        assert_eq!(cache.read_page(page_id).unwrap(), &[9; PAGE_SIZE]);
        assert_eq!(cache.candidate_page_count(9), 0);
    }

    #[derive(Debug)]
    struct FaultDevice {
        inner: MemoryPageDevice,
        fail_write: bool,
        fail_flush: bool,
    }

    impl PageDevice for FaultDevice {
        fn page_count(&self) -> PageId {
            self.inner.page_count()
        }

        fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
            self.inner.read_page(id, destination)
        }

        fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
            if std::mem::take(&mut self.fail_write) {
                return Err(EngineError::new("INJECTED", "write"));
            }
            self.inner.write_page(id, source)
        }

        fn flush(&mut self) -> Result<()> {
            if std::mem::take(&mut self.fail_flush) {
                return Err(EngineError::new("INJECTED", "flush"));
            }
            self.inner.flush()
        }
    }

    #[test]
    fn cache_preserves_dirty_state_across_device_write_and_flush_faults() {
        let active = active_bitmap();
        let page_id = FIRST_DATA_PAGE_ID;
        let device = FaultDevice {
            inner: MemoryPageDevice::new(page_id + 1).unwrap(),
            fail_write: true,
            fail_flush: true,
        };
        let mut cache = PageCache::with_capacity(device, PAGE_SIZE).unwrap();
        cache.reserve_candidate_page(7, page_id, &active).unwrap();
        cache
            .write_candidate_page(7, page_id, &[4; PAGE_SIZE])
            .unwrap();
        assert_eq!(cache.flush_candidate(7).unwrap_err().code, "INJECTED");
        cache.flush_candidate(7).unwrap_err();
        cache.flush_candidate(7).unwrap();
        assert_eq!(cache.device().inner.page(page_id).unwrap(), &[4; PAGE_SIZE]);
        assert_eq!(cache.device().inner.flush_count(), 1);
    }

    #[test]
    fn installing_a_candidate_purges_committed_pages_freed_by_its_bitmap() {
        let freed = FIRST_DATA_PAGE_ID;
        let replacement = freed + 1;
        let mut active = active_bitmap();
        active.set_allocated(freed, true).unwrap();
        let mut device = MemoryPageDevice::new(replacement + 1).unwrap();
        device.write_page(freed, &[2; PAGE_SIZE]).unwrap();
        let mut cache = PageCache::with_capacity(device, PAGE_SIZE * 2).unwrap();
        assert_eq!(cache.read_page(freed).unwrap(), &[2; PAGE_SIZE]);
        cache
            .reserve_candidate_page(31, replacement, &active)
            .unwrap();
        cache
            .write_candidate_page(31, replacement, &[3; PAGE_SIZE])
            .unwrap();
        cache.flush_candidate(31).unwrap();
        let mut next = active.clone();
        next.set_allocated(freed, false).unwrap();
        next.set_allocated(replacement, true).unwrap();
        cache.install_candidate(31, &next).unwrap();
        cache
            .reserve_candidate_page(32, freed, &next)
            .expect("a freed committed page must be reusable");
        assert_eq!(cache.read_page(freed).unwrap_err().code, "PAGE_CACHE_ERROR");
    }

    #[test]
    fn failed_global_flush_keeps_a_candidate_unsafe_to_install() {
        let active = active_bitmap();
        let page_id = FIRST_DATA_PAGE_ID;
        let device = FaultDevice {
            inner: MemoryPageDevice::new(page_id + 1).unwrap(),
            fail_write: false,
            fail_flush: true,
        };
        let mut cache = PageCache::with_capacity(device, PAGE_SIZE).unwrap();
        cache.reserve_candidate_page(41, page_id, &active).unwrap();
        cache
            .write_candidate_page(41, page_id, &[4; PAGE_SIZE])
            .unwrap();

        assert_eq!(cache.flush_candidate(41).unwrap_err().code, "INJECTED");
        assert_eq!(
            cache
                .install_candidate(41, &bitmap_allocating(&active, &[page_id]))
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        cache.flush_candidate(41).unwrap();
        cache
            .install_candidate(41, &bitmap_allocating(&active, &[page_id]))
            .unwrap();
        assert_eq!(cache.read_page(page_id).unwrap(), &[4; PAGE_SIZE]);
    }

    #[test]
    fn install_rejects_unwritten_pages_and_a_second_live_candidate_is_rejected() {
        let active = active_bitmap();
        let first = FIRST_DATA_PAGE_ID;
        let second = first + 1;
        let mut cache =
            PageCache::with_capacity(MemoryPageDevice::new(second + 1).unwrap(), PAGE_SIZE * 2)
                .unwrap();
        cache.reserve_candidate_page(51, first, &active).unwrap();
        cache.flush_candidate(51).unwrap();
        assert_eq!(
            cache
                .install_candidate(51, &bitmap_allocating(&active, &[first]))
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );

        assert_eq!(
            cache
                .reserve_candidate_page(52, second, &active)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        cache.invalidate_candidate(51);
        cache.reserve_candidate_page(52, second, &active).unwrap();
    }

    #[test]
    fn an_installed_page_cannot_be_reserved_again_even_after_cache_eviction() {
        let active = active_bitmap();
        let page_id = FIRST_DATA_PAGE_ID;
        let next = bitmap_allocating(&active, &[page_id]);
        let mut cache =
            PageCache::with_capacity(MemoryPageDevice::new(page_id + 1).unwrap(), PAGE_SIZE)
                .unwrap();
        cache.reserve_candidate_page(61, page_id, &active).unwrap();
        cache
            .write_candidate_page(61, page_id, &[6; PAGE_SIZE])
            .unwrap();
        cache.flush_candidate(61).unwrap();
        cache.install_candidate(61, &next).unwrap();
        cache.read_page(0).unwrap();

        assert_eq!(
            cache
                .reserve_candidate_page(62, page_id, &next)
                .unwrap_err()
                .code,
            "PAGE_CACHE_ERROR"
        );
        assert_eq!(cache.read_page(page_id).unwrap(), &[6; PAGE_SIZE]);
    }
}
