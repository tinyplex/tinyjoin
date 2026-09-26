use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasherDefault, Hasher},
};

use crate::{
    AllocationBitmap, EngineError, FIRST_DATA_PAGE_ID, PAGE_SIZE, PageDevice, PageId, Result,
};

// Keep browser cache diagnostics static and let PAGE_CACHE_ERROR carry the
// failure class. Native builds retain page and reservation details.
#[cfg(all(target_arch = "wasm32", feature = "compact-storage-diagnostics"))]
macro_rules! storage_diagnostic {
    ($($argument:tt)*) => {{
        if false {
            let _ = ::std::format!($($argument)*);
        }
        String::from("Page cache operation failed")
    }};
}

#[cfg(not(all(target_arch = "wasm32", feature = "compact-storage-diagnostics")))]
macro_rules! storage_diagnostic {
    ($($argument:tt)*) => {
        ::std::format!($($argument)*)
    };
}

pub(crate) type CandidateId = u64;

pub(crate) const DEFAULT_PAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_PAGE_CACHE_BYTES: usize = 32 * 1024 * 1024;
#[cfg(test)]
pub(crate) const DEFAULT_PAGE_CACHE_PAGES: usize = DEFAULT_PAGE_CACHE_BYTES / PAGE_SIZE;

/// A multiplicative hash for page-keyed maps.
///
/// Page IDs and candidate numbers are integers the engine assigns, not keys an adversary chooses,
/// so these maps do not need SipHash's flooding resistance. Its cost dominated cache lookups.
#[derive(Default)]
struct PageKeyHasher(u64);

impl Hasher for PageKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_isize(&mut self, value: isize) {
        self.write_u64(value as u64);
    }
}

type PageKeyMap<K, V> = HashMap<K, V, BuildHasherDefault<PageKeyHasher>>;

impl CacheEntry {
    fn seal(&mut self) {
        if self.unsealed {
            crate::page::seal(&mut self.bytes);
            self.unsealed = false;
        }
    }
}

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
    /// Whether the page still needs its checksum, which it receives when written to the device.
    unsealed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Reservation {
    candidate: CandidateId,
    written: bool,
}

#[derive(Debug)]
pub(crate) struct PageCache<D: PageDevice> {
    device: D,
    entries: Vec<CacheEntry>,
    lookup: PageKeyMap<(Owner, PageId), usize>,
    reservations: PageKeyMap<PageId, Reservation>,
    /// Owners with pages which reached the device but have not been covered by a successful
    /// device-wide durability barrier. A PageDevice flush is global, so one successful flush can
    /// make writes from several interleaved owners durable at once.
    unflushed_owners: HashSet<Owner>,
    capacity: usize,
    hand: usize,
}

impl<D: PageDevice> PageCache<D> {
    #[cfg(test)]
    pub(crate) fn new(device: D) -> Result<Self> {
        Self::with_capacity(device, DEFAULT_PAGE_CACHE_BYTES)
    }

    pub(crate) fn with_capacity(device: D, capacity_bytes: usize) -> Result<Self> {
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
            lookup: PageKeyMap::with_capacity_and_hasher(capacity, Default::default()),
            reservations: PageKeyMap::default(),
            unflushed_owners: HashSet::new(),
            capacity,
            hand: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn capacity_pages(&self) -> usize {
        self.capacity
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn device(&self) -> &D {
        &self.device
    }

    #[cfg(test)]
    pub(crate) fn read_page(&mut self, id: PageId) -> Result<&[u8; PAGE_SIZE]> {
        self.read_page_verified(id, |_| Ok(()))
    }

    /// Reads a committed page, running `verify` only when the page is loaded from the device.
    ///
    /// Bytes are cached only after they pass, so a page which fails verification is reported on
    /// every read rather than served from the cache. Bytes written through the cache were produced
    /// by the engine and are trusted as written.
    pub(crate) fn read_page_verified(
        &mut self,
        id: PageId,
        verify: impl FnOnce(&[u8; PAGE_SIZE]) -> Result<()>,
    ) -> Result<&[u8; PAGE_SIZE]> {
        self.ensure_not_reserved(id)?;
        self.read_owned(Owner::Committed, id, verify)
    }

    #[cfg(test)]
    pub(crate) fn read_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
    ) -> Result<&[u8; PAGE_SIZE]> {
        self.read_candidate_page_verified(candidate, id, |_| Ok(()))
    }

    /// Reads a page ID allocated exclusively to this copy-on-write candidate, verifying it as
    /// [`Self::read_page_verified`] does if an eviction sent it to the device.
    /// Shared committed page IDs must be read with [`Self::read_page_verified`].
    pub(crate) fn read_candidate_page_verified(
        &mut self,
        candidate: CandidateId,
        id: PageId,
        verify: impl FnOnce(&[u8; PAGE_SIZE]) -> Result<()>,
    ) -> Result<&[u8; PAGE_SIZE]> {
        self.ensure_reserved_by(candidate, id)?;
        if !self
            .reservations
            .get(&id)
            .expect("the candidate reservation was checked")
            .written
        {
            return Err(cache_error(storage_diagnostic!(
                "Candidate {candidate} must initialize reserved page {id} before reading it"
            )));
        }
        self.read_owned(Owner::Candidate(candidate), id, verify)
    }

    /// Reserves an unallocated data page exclusively for a copy-on-write candidate.
    ///
    /// Reservation ownership is independent of cache residency: evicting a candidate page does
    /// not make the physical page available to another candidate. The active allocation bitmap is
    /// consulted on every reservation so a candidate can never overwrite a page reachable from
    /// the committed superblock.
    pub(crate) fn reserve_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
        active_bitmap: &AllocationBitmap,
    ) -> Result<()> {
        if id < FIRST_DATA_PAGE_ID {
            return Err(cache_error(storage_diagnostic!(
                "Candidate pages must not use metadata page ID {id}"
            )));
        }
        if active_bitmap.is_allocated(id)? {
            return Err(cache_error(storage_diagnostic!(
                "Candidate {candidate} cannot reserve allocated page {id}"
            )));
        }
        if self.lookup.contains_key(&(Owner::Committed, id)) {
            return Err(cache_error(storage_diagnostic!(
                "Candidate {candidate} cannot reserve page {id} while it is present in the committed cache view"
            )));
        }
        if let Some(reservation) = self
            .reservations
            .values()
            .find(|reservation| reservation.candidate != candidate)
        {
            return Err(cache_error(storage_diagnostic!(
                "Candidate {candidate} cannot reserve page {id}; candidate {} is already active",
                reservation.candidate
            )));
        }
        match self.reservations.get(&id) {
            Some(reservation) if reservation.candidate == candidate => Ok(()),
            Some(reservation) => Err(cache_error(storage_diagnostic!(
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

    #[cfg(test)]
    pub(crate) fn write_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
        bytes: &[u8],
    ) -> Result<()> {
        self.write_candidate(candidate, id, bytes, false)
    }

    /// Writes a candidate page whose checksum field is still zero. The cache seals it only when it
    /// writes the page to the device, so a page rewritten many times is checksummed once.
    pub(crate) fn write_unsealed_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
        bytes: &[u8],
    ) -> Result<()> {
        self.write_candidate(candidate, id, bytes, true)
    }

    fn write_candidate(
        &mut self,
        candidate: CandidateId,
        id: PageId,
        bytes: &[u8],
        unsealed: bool,
    ) -> Result<()> {
        self.ensure_reserved_by(candidate, id)?;
        self.write_owned(Owner::Candidate(candidate), id, bytes, unsealed)?;
        self.reservations
            .get_mut(&id)
            .expect("the candidate reservation was checked")
            .written = true;
        Ok(())
    }

    /// Releases one page reserved by a candidate after its owner has proved that no candidate
    /// page references it. A previously-evicted write may remain as an unreachable physical
    /// orphan, but it is removed from the candidate's logical allocation set by the pager.
    pub(crate) fn release_candidate_page(
        &mut self,
        candidate: CandidateId,
        id: PageId,
    ) -> Result<()> {
        self.ensure_reserved_by(candidate, id)?;
        self.reservations.remove(&id);
        self.remove_entry((Owner::Candidate(candidate), id));
        if !self
            .reservations
            .values()
            .any(|reservation| reservation.candidate == candidate)
        {
            self.unflushed_owners.remove(&Owner::Candidate(candidate));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn flush_candidate(&mut self, candidate: CandidateId) -> Result<()> {
        self.flush_owner(Owner::Candidate(candidate))
    }

    /// Writes a candidate's dirty pages to the device without flushing it, so that a commit can
    /// make them durable in the same flush as its other pages.
    pub(crate) fn write_candidate_pages(&mut self, candidate: CandidateId) -> Result<()> {
        self.write_owner(Owner::Candidate(candidate))
    }

    /// Flushes the device, making every page written through it durable.
    pub(crate) fn flush_device(&mut self) -> Result<()> {
        self.device.flush()?;
        self.unflushed_owners.clear();
        Ok(())
    }

    pub(crate) fn install_candidate(
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
            return Err(cache_error(storage_diagnostic!(
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
            return Err(cache_error(storage_diagnostic!(
                "Candidate {candidate} overlaps a page in the committed cache view"
            )));
        }
        if let Some((id, _)) = candidate_ids.iter().find(|(_, written)| !written) {
            return Err(cache_error(storage_diagnostic!(
                "Candidate {candidate} reserved page {id} but never wrote it"
            )));
        }
        for (id, _) in &candidate_ids {
            if !next_bitmap.is_allocated(*id)? {
                return Err(cache_error(storage_diagnostic!(
                    "Candidate {candidate} page {id} is missing from the next allocation bitmap"
                )));
            }
        }
        let mut deallocated = Vec::new();
        for entry in &self.entries {
            if entry.owner == Owner::Committed && !next_bitmap.is_allocated(entry.id)? {
                deallocated.push(entry.id);
            }
        }
        for id in deallocated {
            self.remove_entry((Owner::Committed, id));
        }
        // Candidate pages are cached only under reserved IDs, so re-keying each reserved ID moves
        // every candidate entry into the committed view without scanning the cache.
        for (id, _) in candidate_ids {
            if let Some(index) = self.lookup.remove(&(owner, id)) {
                let entry = &mut self.entries[index];
                entry.owner = Owner::Committed;
                entry.referenced = true;
                self.lookup.insert((Owner::Committed, id), index);
            }
        }
        self.reservations
            .retain(|_, reservation| reservation.candidate != candidate);
        self.debug_assert_no_entries(owner);
        Ok(())
    }

    pub(crate) fn invalidate_candidate(&mut self, candidate: CandidateId) {
        let owner = Owner::Candidate(candidate);
        self.unflushed_owners.remove(&owner);
        // Only reserved IDs can hold candidate entries, so a read-only candidate, which reserves
        // nothing, is released without touching the cache.
        let reserved = self
            .reservations
            .iter()
            .filter(|(_, reservation)| reservation.candidate == candidate)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in reserved {
            self.reservations.remove(&id);
            self.remove_entry((owner, id));
        }
        self.debug_assert_no_entries(owner);
    }

    #[cfg(test)]
    pub(crate) fn candidate_page_count(&self, candidate: CandidateId) -> usize {
        self.reservations
            .iter()
            .filter(|(_, reservation)| reservation.candidate == candidate)
            .count()
    }

    fn ensure_not_reserved(&self, id: PageId) -> Result<()> {
        if let Some(reservation) = self.reservations.get(&id) {
            return Err(cache_error(storage_diagnostic!(
                "Page {id} is exclusively reserved by candidate {}",
                reservation.candidate
            )));
        }
        Ok(())
    }

    fn ensure_reserved_by(&self, candidate: CandidateId, id: PageId) -> Result<()> {
        match self.reservations.get(&id) {
            Some(reservation) if reservation.candidate == candidate => Ok(()),
            Some(reservation) => Err(cache_error(storage_diagnostic!(
                "Page {id} is reserved by candidate {}, not candidate {candidate}",
                reservation.candidate
            ))),
            None => Err(cache_error(storage_diagnostic!(
                "Candidate {candidate} must reserve page {id} before accessing it"
            ))),
        }
    }

    fn read_owned(
        &mut self,
        owner: Owner,
        id: PageId,
        verify: impl FnOnce(&[u8; PAGE_SIZE]) -> Result<()>,
    ) -> Result<&[u8; PAGE_SIZE]> {
        if let Some(index) = self.lookup.get(&(owner, id)).copied() {
            self.entries[index].referenced = true;
            return Ok(&self.entries[index].bytes);
        }
        let mut bytes = Box::new([0; PAGE_SIZE]);
        self.device.read_page(id, bytes.as_mut_slice())?;
        verify(&bytes)?;
        let index = self.insert(CacheEntry {
            id,
            owner,
            bytes,
            referenced: true,
            dirty: false,
            unsealed: false,
        })?;
        Ok(&self.entries[index].bytes)
    }

    fn write_owned(
        &mut self,
        owner: Owner,
        id: PageId,
        bytes: &[u8],
        unsealed: bool,
    ) -> Result<()> {
        if bytes.len() != PAGE_SIZE {
            return Err(cache_error(storage_diagnostic!(
                "Cached pages must be exactly {PAGE_SIZE} bytes, not {}",
                bytes.len()
            )));
        }
        if let Some(index) = self.lookup.get(&(owner, id)).copied() {
            let entry = &mut self.entries[index];
            entry.bytes.copy_from_slice(bytes);
            entry.referenced = true;
            entry.dirty = true;
            entry.unsealed = unsealed;
            return Ok(());
        }
        self.insert(CacheEntry {
            id,
            owner,
            bytes: Box::new(bytes.try_into().expect("validated page length")),
            referenced: true,
            dirty: true,
            unsealed,
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
                entry.seal();
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

    fn write_owner(&mut self, owner: Owner) -> Result<()> {
        for entry in &mut self.entries {
            if entry.owner == owner && entry.dirty {
                entry.seal();
                self.device.write_page(entry.id, entry.bytes.as_slice())?;
                entry.dirty = false;
                self.unflushed_owners.insert(entry.owner);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn flush_owner(&mut self, owner: Owner) -> Result<()> {
        self.write_owner(owner)?;
        self.flush_device()
    }

    /// Removes one cached entry, if present, moving the last entry into its slot so that only one
    /// lookup index changes.
    fn remove_entry(&mut self, key: (Owner, PageId)) {
        let Some(index) = self.lookup.remove(&key) else {
            return;
        };
        self.entries.swap_remove(index);
        if let Some(moved) = self.entries.get(index) {
            self.lookup.insert((moved.owner, moved.id), index);
        }
        if self.hand >= self.entries.len() {
            self.hand = 0;
        }
    }

    /// Confirms in debug builds that an owner has no cached pages left and that the lookup still
    /// indexes every entry, since both are maintained incrementally.
    fn debug_assert_no_entries(&self, owner: Owner) {
        debug_assert!(self.entries.iter().all(|entry| entry.owner != owner));
        debug_assert_eq!(self.lookup.len(), self.entries.len());
        debug_assert!(
            self.entries
                .iter()
                .enumerate()
                .all(|(index, entry)| self.lookup.get(&(entry.owner, entry.id)) == Some(&index))
        );
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
