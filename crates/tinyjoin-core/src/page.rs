use crate::{EngineError, Result, checksum::crc32_update};

// Keep browser corruption diagnostics static and let the stable error code
// carry the precise class. Native builds retain the detailed values.
#[cfg(all(target_arch = "wasm32", feature = "compact-storage-diagnostics"))]
macro_rules! storage_diagnostic {
    ($($argument:tt)*) => {{
        if false {
            let _ = ::std::format!($($argument)*);
        }
        String::from("Page validation failed")
    }};
}

#[cfg(not(all(target_arch = "wasm32", feature = "compact-storage-diagnostics")))]
macro_rules! storage_diagnostic {
    ($($argument:tt)*) => {
        ::std::format!($($argument)*)
    };
}

pub type PageId = u64;

pub const PAGE_SIZE: usize = 4_096;
pub const MAX_PAGE_COUNT: PageId = 65_536;
pub(crate) const PAGE_HEADER_SIZE: usize = 32;
pub(crate) const MAX_PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

pub(crate) const SUPERBLOCK_PAGE_COUNT: usize = 2;
/// The chunks of the allocation bitmap beyond the part each superblock carries, each kept in two
/// slots of its own.
pub(crate) const BITMAP_CHUNK_COUNT: usize = 2;
pub(crate) const BITMAP_PAGE_COUNT: usize = BITMAP_CHUNK_COUNT * 2;
pub(crate) const FIRST_DATA_PAGE_ID: PageId =
    SUPERBLOCK_PAGE_COUNT as PageId + BITMAP_PAGE_COUNT as PageId;

const PAGE_MAGIC: &[u8; 8] = b"TGRPAGE\0";
const PAGE_FORMAT_VERSION: u16 = 1;
const PAGE_FLAGS: u16 = 0;
const PAGE_CRC_OFFSET: usize = 28;

const SUPERBLOCK_MAGIC: &[u8; 8] = b"TGRSUPR\0";
// Page format 3 stores rows as packed records and keys in an order-preserving encoding, and
// carries the start of the allocation bitmap in each superblock. Earlier databases are refused
// with UNSUPPORTED_PAGE rather than read under the wrong layout.
const SUPERBLOCK_FORMAT_VERSION: u16 = 3;
const SUPERBLOCK_FLAGS: u16 = 0;
/// The superblock's own fields, which the start of the allocation bitmap follows to fill its page.
const SUPERBLOCK_FIELDS_SIZE: usize = 128;
const SUPERBLOCK_BITMAP_BYTES: usize = MAX_PAGE_PAYLOAD_SIZE - SUPERBLOCK_FIELDS_SIZE;
/// Where each bitmap chunk's entry, sixteen bytes, begins among the superblock's fields.
const BITMAP_CHUNK_ENTRIES_OFFSET: usize = 72;
const BITMAP_CHUNK_ENTRY_SIZE: usize = 16;

pub(crate) const ALLOCATION_BITMAP_BYTES: usize = MAX_PAGE_COUNT as usize / 8;
const BITMAP_CHUNK_HEADER_SIZE: usize = 16;
const MAX_BITMAP_CHUNK_BYTES: usize = MAX_PAGE_PAYLOAD_SIZE - BITMAP_CHUNK_HEADER_SIZE;
const _: () = assert!(
    SUPERBLOCK_BITMAP_BYTES + (BITMAP_CHUNK_COUNT - 1) * MAX_BITMAP_CHUNK_BYTES
        < ALLOCATION_BITMAP_BYTES
        && SUPERBLOCK_BITMAP_BYTES + BITMAP_CHUNK_COUNT * MAX_BITMAP_CHUNK_BYTES
            >= ALLOCATION_BITMAP_BYTES
        && BITMAP_CHUNK_ENTRIES_OFFSET + BITMAP_CHUNK_COUNT * BITMAP_CHUNK_ENTRY_SIZE
            <= SUPERBLOCK_FIELDS_SIZE
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum PageType {
    Superblock = 1,
    AllocationBitmap = 2,
    BtreeInternal = 3,
    BtreeLeaf = 4,
    Overflow = 5,
}

impl TryFrom<u8> for PageType {
    type Error = EngineError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Superblock),
            2 => Ok(Self::AllocationBitmap),
            3 => Ok(Self::BtreeInternal),
            4 => Ok(Self::BtreeLeaf),
            5 => Ok(Self::Overflow),
            _ => Err(invalid_page(storage_diagnostic!(
                "Unknown page type {value}"
            ))),
        }
    }
}

/// The checksum a sealed page carries.
pub(crate) fn page_checksum(bytes: &[u8; PAGE_SIZE]) -> u32 {
    u32::from_le_bytes(
        bytes[PAGE_CRC_OFFSET..PAGE_CRC_OFFSET + 4]
            .try_into()
            .expect("a checksum is four bytes"),
    )
}

/// The fingerprint of the data pages a commit wrote, from each page's ID and checksum in ascending
/// page order. A superblock records its commit's, so that recovery can tell whether every page the
/// commit wrote became durable with it.
pub(crate) struct CommitHash(crate::hash::Hasher);

impl CommitHash {
    pub(crate) fn new() -> Self {
        Self(crate::hash::Hasher::new())
    }

    /// Adds the next page, which must follow every page added before it.
    pub(crate) fn page(&mut self, id: PageId, checksum: u32) {
        self.0.write_u64(id);
        self.0.write_u64(checksum.into());
    }

    pub(crate) fn finish(self) -> u64 {
        self.0.finish()
    }
}

/// Writes a physical page's checksum, which covers the page with its own field read as zero.
/// Writes `value` at `offset` as one store of its size, where copying it from a slice would call
/// `memcpy` for a few bytes.
#[inline(always)]
fn put<const N: usize>(bytes: &mut [u8], offset: usize, value: [u8; N]) {
    *<&mut [u8; N]>::try_from(&mut bytes[offset..offset + N]).expect("bounded write") = value;
}

/// Writes a page's header, which is all but its checksum, for a payload of `payload_length`
/// bytes that fits the page.
fn write_page_header(
    bytes: &mut [u8; PAGE_SIZE],
    id: PageId,
    page_type: PageType,
    payload_length: usize,
) {
    debug_assert!(payload_length <= MAX_PAGE_PAYLOAD_SIZE);
    bytes[..8].copy_from_slice(PAGE_MAGIC);
    put(bytes, 8, PAGE_FORMAT_VERSION.to_le_bytes());
    put(bytes, 10, PAGE_FLAGS.to_le_bytes());
    put(bytes, 12, id.to_le_bytes());
    bytes[20] = page_type as u8;
    put(bytes, 24, (payload_length as u32).to_le_bytes());
}

pub(crate) fn seal(bytes: &mut [u8; PAGE_SIZE]) {
    let checksum = crc32_update(u32::MAX, &bytes[..PAGE_CRC_OFFSET]);
    let checksum = crc32_update(checksum, &[0; 4]);
    let checksum = !crc32_update(checksum, &bytes[PAGE_CRC_OFFSET + 4..]);
    bytes[PAGE_CRC_OFFSET..PAGE_CRC_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Page {
    pub id: PageId,
    pub page_type: PageType,
    pub payload: Vec<u8>,
}

impl Page {
    pub(crate) fn new(id: PageId, page_type: PageType, payload: Vec<u8>) -> Result<Self> {
        validate_page_id(id)?;
        if payload.len() > MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_page(format!(
                "Page payload is {} bytes, exceeding the {}-byte limit",
                payload.len(),
                MAX_PAGE_PAYLOAD_SIZE
            )));
        }
        Ok(Self {
            id,
            page_type,
            payload,
        })
    }

    #[cfg(test)]
    pub(crate) fn encode(&self) -> Result<[u8; PAGE_SIZE]> {
        let mut bytes = self.encode_unsealed()?;
        seal(&mut bytes);
        Ok(bytes)
    }

    /// Encodes the page with its checksum field zero, for a page that is [sealed](seal) only when
    /// it is written to the device. A page rewritten many times before then is checksummed once.
    pub(crate) fn encode_unsealed(&self) -> Result<[u8; PAGE_SIZE]> {
        validate_page_id(self.id)?;
        if self.payload.len() > MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_page(format!(
                "Page payload is {} bytes, exceeding the {}-byte limit",
                self.payload.len(),
                MAX_PAGE_PAYLOAD_SIZE
            )));
        }

        let mut bytes = [0; PAGE_SIZE];
        write_page_header(&mut bytes, self.id, self.page_type, self.payload.len());
        bytes[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + self.payload.len()]
            .copy_from_slice(&self.payload);
        Ok(bytes)
    }

    #[cfg(test)]
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        Self::verify(bytes)?;
        Self::decode_verified(bytes)
    }

    /// Checks a physical page's checksum and every envelope field.
    ///
    /// The page cache runs this once, when it loads a page from the device, and keeps only pages
    /// which pass. Pages the engine encodes are correct as written, so cached pages are decoded
    /// with [`Self::decode_verified`] instead of being checked again on every access.
    pub(crate) fn verify(bytes: &[u8]) -> Result<()> {
        if bytes.len() != PAGE_SIZE {
            return Err(invalid_page(storage_diagnostic!(
                "A physical page must be exactly {PAGE_SIZE} bytes, not {}",
                bytes.len()
            )));
        }
        // Treat the envelope fields as authoritative only after the physical
        // page checksum succeeds. A torn write can otherwise turn a version
        // byte into an apparently coherent unsupported format and prevent
        // recovery from the other metadata slot. The checksum covers the page
        // with its own field read as zero.
        let expected_checksum = read_u32(bytes, PAGE_CRC_OFFSET);
        let checksum = crc32_update(u32::MAX, &bytes[..PAGE_CRC_OFFSET]);
        let checksum = crc32_update(checksum, &[0; 4]);
        let checksum = !crc32_update(checksum, &bytes[PAGE_CRC_OFFSET + 4..]);
        if checksum != expected_checksum {
            return Err(invalid_page("Page checksum does not match"));
        }
        if &bytes[..8] != PAGE_MAGIC {
            return Err(invalid_page("Page magic does not match"));
        }
        let version = read_u16(bytes, 8);
        if version != PAGE_FORMAT_VERSION {
            return Err(unsupported_page(format!(
                "Page format version {version} is not supported"
            )));
        }
        let flags = read_u16(bytes, 10);
        if flags != PAGE_FLAGS {
            return Err(unsupported_page(format!(
                "Page flags {flags:#06x} are not supported"
            )));
        }
        let id = read_u64(bytes, 12);
        validate_page_id(id)?;
        PageType::try_from(bytes[20])?;
        if bytes[21..24].iter().any(|byte| *byte != 0) {
            return Err(invalid_page("Page header reserved bytes must be zero"));
        }
        let payload_length = read_u32(bytes, 24) as usize;
        if payload_length > MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_page(storage_diagnostic!(
                "Page payload length {payload_length} exceeds the {MAX_PAGE_PAYLOAD_SIZE}-byte limit"
            )));
        }
        let payload_end = PAGE_HEADER_SIZE + payload_length;
        if bytes[payload_end..].iter().any(|byte| *byte != 0) {
            return Err(invalid_page("Page padding bytes must be zero"));
        }
        Ok(())
    }

    /// Decodes a page which [`Self::verify`] accepted, or which the engine encoded itself.
    ///
    /// Only the bounds needed to slice the payload safely are checked again.
    pub(crate) fn decode_verified(bytes: &[u8]) -> Result<Self> {
        PageRef::decode_verified(bytes).map(PageRef::to_page)
    }

    pub(crate) fn as_page_ref(&self) -> PageRef<'_> {
        PageRef {
            id: self.id,
            page_type: self.page_type,
            payload: &self.payload,
        }
    }
}

/// A page read in place: its envelope, and its payload borrowed from the bytes holding it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageRef<'a> {
    pub(crate) id: PageId,
    pub(crate) page_type: PageType,
    pub(crate) payload: &'a [u8],
}

impl<'a> PageRef<'a> {
    /// Reads a page as [`Page::decode_verified`] does, without copying its payload.
    pub(crate) fn decode_verified(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() != PAGE_SIZE {
            return Err(invalid_page(storage_diagnostic!(
                "A physical page must be exactly {PAGE_SIZE} bytes, not {}",
                bytes.len()
            )));
        }
        let payload_length = read_u32(bytes, 24) as usize;
        if payload_length > MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_page(storage_diagnostic!(
                "Page payload length {payload_length} exceeds the {MAX_PAGE_PAYLOAD_SIZE}-byte limit"
            )));
        }
        Ok(Self {
            id: read_u64(bytes, 12),
            page_type: PageType::try_from(bytes[20])?,
            payload: &bytes[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + payload_length],
        })
    }

    pub(crate) fn to_page(self) -> Page {
        Page {
            id: self.id,
            page_type: self.page_type,
            payload: self.payload.to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum SuperblockSlot {
    A = 0,
    B = 1,
}

impl SuperblockSlot {
    pub(crate) const fn page_id(self) -> PageId {
        self as PageId
    }

    pub(crate) const fn inactive(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    pub(crate) const fn bitmap_slot(self) -> BitmapSlot {
        match self {
            Self::A => BitmapSlot::A,
            Self::B => BitmapSlot::B,
        }
    }
}

impl TryFrom<u8> for SuperblockSlot {
    type Error = EngineError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::A),
            1 => Ok(Self::B),
            _ => Err(invalid_page(storage_diagnostic!(
                "Unknown superblock slot {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum BitmapSlot {
    A = 0,
    B = 1,
}

impl BitmapSlot {
    pub(crate) const fn first_page_id(self) -> PageId {
        match self {
            Self::A => SUPERBLOCK_PAGE_COUNT as PageId,
            Self::B => SUPERBLOCK_PAGE_COUNT as PageId + BITMAP_CHUNK_COUNT as PageId,
        }
    }

    pub(crate) const fn page_id(self, chunk: usize) -> PageId {
        self.first_page_id() + chunk as PageId
    }

    pub(crate) const fn inactive(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

impl TryFrom<u8> for BitmapSlot {
    type Error = EngineError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::A),
            1 => Ok(Self::B),
            _ => Err(invalid_page(storage_diagnostic!(
                "Unknown allocation bitmap slot {value}"
            ))),
        }
    }
}

/// Where a superblock finds one chunk of its allocation bitmap: the slot that holds it, the
/// generation whose commit wrote it, and the checksum of the page that commit wrote.
///
/// A commit writes only the chunks it changes, each in the slot beside the one its predecessor
/// reads, so a chunk that no commit has changed since is shared by both roots. The checksum ties a
/// root to the very page its commit wrote, which a page an abandoned commit wrote in the same slot
/// at the same generation cannot pass for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BitmapChunk {
    pub slot: BitmapSlot,
    pub generation: u64,
    pub checksum: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Superblock {
    pub slot: SuperblockSlot,
    pub generation: u64,
    pub database_revision: u64,
    /// The fingerprint of every row in every table.
    ///
    /// This is maintained so that two databases can be compared without reading either one, and
    /// so that a comparison which differs can be narrowed to a table and then to a key range. It
    /// is derived from the catalog and is not authoritative: a reader which distrusts it can
    /// recompute it from the table fingerprints in the catalog.
    pub database_hash: u64,
    pub catalog_root_page_id: Option<PageId>,
    pub live_data_page_count: u32,
    pub max_page_count: PageId,
    /// The [commit hash](CommitHash) of the data pages this generation's commit wrote: those
    /// its allocation bitmap holds and its predecessor's does not.
    pub commit_hash: u64,
    /// The chunks of the allocation bitmap beyond the start the superblock carries itself.
    pub bitmap_chunks: [BitmapChunk; BITMAP_CHUNK_COUNT],
}

impl Superblock {
    /// Encodes the superblock with the start of `bitmap`, the allocation bitmap it describes.
    pub(crate) fn encode_page(&self, bitmap: &AllocationBitmap) -> Result<[u8; PAGE_SIZE]> {
        self.validate()?;
        self.validate_bitmap(bitmap)?;
        // Written in place, as every commit writes one.
        let mut bytes = [0; PAGE_SIZE];
        write_page_header(
            &mut bytes,
            self.slot.page_id(),
            PageType::Superblock,
            MAX_PAGE_PAYLOAD_SIZE,
        );
        let payload = &mut bytes[PAGE_HEADER_SIZE..];
        payload[..8].copy_from_slice(SUPERBLOCK_MAGIC);
        put(payload, 8, SUPERBLOCK_FORMAT_VERSION.to_le_bytes());
        put(payload, 10, SUPERBLOCK_FLAGS.to_le_bytes());
        put(payload, 12, (PAGE_SIZE as u32).to_le_bytes());
        put(payload, 16, self.max_page_count.to_le_bytes());
        put(payload, 24, self.generation.to_le_bytes());
        put(payload, 32, self.database_revision.to_le_bytes());
        put(payload, 40, self.database_hash.to_le_bytes());
        put(
            payload,
            48,
            self.catalog_root_page_id.unwrap_or(u64::MAX).to_le_bytes(),
        );
        put(payload, 56, self.live_data_page_count.to_le_bytes());
        put(payload, 60, (BITMAP_CHUNK_COUNT as u16).to_le_bytes());
        payload[62] = self.slot as u8;
        put(payload, 64, self.commit_hash.to_le_bytes());
        for (chunk, entry) in self.bitmap_chunks.iter().enumerate() {
            let at = BITMAP_CHUNK_ENTRIES_OFFSET + chunk * BITMAP_CHUNK_ENTRY_SIZE;
            put(payload, at, entry.generation.to_le_bytes());
            put(payload, at + 8, entry.checksum.to_le_bytes());
            payload[at + 12] = entry.slot as u8;
        }
        payload[SUPERBLOCK_FIELDS_SIZE..].copy_from_slice(bitmap.superblock_bits());
        seal(&mut bytes);
        Ok(bytes)
    }

    /// Reads a superblock page, and the start of the allocation bitmap it carries.
    pub(crate) fn decode_page(bytes: &[u8]) -> Result<(Self, &[u8])> {
        Page::verify(bytes)?;
        let page = PageRef::decode_verified(bytes)?;
        if page.id >= SUPERBLOCK_PAGE_COUNT as PageId {
            return Err(invalid_page(storage_diagnostic!(
                "Superblock must occupy page 0 or 1, not page {}",
                page.id
            )));
        }
        if page.page_type != PageType::Superblock {
            return Err(invalid_page("Superblock page has the wrong page type"));
        }
        let payload = page.payload;
        // The format is read before anything it lays out, so that a superblock another format
        // lays out differently, even at another length, is refused as unsupported.
        if payload.len() < 12 || &payload[..8] != SUPERBLOCK_MAGIC {
            return Err(invalid_page("Superblock magic does not match"));
        }
        let version = read_u16(payload, 8);
        if version != SUPERBLOCK_FORMAT_VERSION {
            return Err(unsupported_page(format!(
                "Superblock format version {version} is not supported"
            )));
        }
        let flags = read_u16(payload, 10);
        if flags != SUPERBLOCK_FLAGS {
            return Err(unsupported_page(format!(
                "Superblock flags {flags:#06x} are not supported"
            )));
        }
        if payload.len() != MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_page("Superblock payload must fill its page"));
        }
        if read_u32(payload, 12) as usize != PAGE_SIZE {
            return Err(unsupported_page(
                "Superblock page size does not match this build",
            ));
        }
        let max_page_count = read_u64(payload, 16);
        if max_page_count != MAX_PAGE_COUNT {
            return Err(unsupported_page(
                "Superblock maximum page count does not match this build",
            ));
        }
        if read_u16(payload, 60) as usize != BITMAP_CHUNK_COUNT {
            return Err(unsupported_page(
                "Superblock allocation bitmap chunk count is not supported",
            ));
        }
        let (entries, reserved) = payload[BITMAP_CHUNK_ENTRIES_OFFSET..SUPERBLOCK_FIELDS_SIZE]
            .split_at(BITMAP_CHUNK_COUNT * BITMAP_CHUNK_ENTRY_SIZE);
        let entries = entries.as_chunks::<BITMAP_CHUNK_ENTRY_SIZE>().0;
        if payload[63] != 0
            || reserved.iter().any(|byte| *byte != 0)
            || entries
                .iter()
                .any(|entry| entry[13..].iter().any(|byte| *byte != 0))
        {
            return Err(invalid_page("Superblock reserved bytes must be zero"));
        }
        let slot = SuperblockSlot::try_from(payload[62])?;
        if page.id != slot.page_id() {
            return Err(invalid_page(storage_diagnostic!(
                "Superblock slot {slot:?} must occupy page {}, not page {}",
                slot.page_id(),
                page.id
            )));
        }
        let mut bitmap_chunks = [BitmapChunk {
            slot: BitmapSlot::A,
            generation: 0,
            checksum: 0,
        }; BITMAP_CHUNK_COUNT];
        for (chunk, entry) in bitmap_chunks.iter_mut().zip(entries) {
            *chunk = BitmapChunk {
                slot: BitmapSlot::try_from(entry[12])?,
                generation: read_u64(entry, 0),
                checksum: read_u32(entry, 8),
            };
        }
        let catalog_root_page_id = match read_u64(payload, 48) {
            u64::MAX => None,
            id => Some(id),
        };
        let superblock = Self {
            slot,
            generation: read_u64(payload, 24),
            database_revision: read_u64(payload, 32),
            database_hash: read_u64(payload, 40),
            catalog_root_page_id,
            live_data_page_count: read_u32(payload, 56),
            max_page_count,
            commit_hash: read_u64(payload, 64),
            bitmap_chunks,
        };
        superblock.validate()?;
        Ok((superblock, &payload[SUPERBLOCK_FIELDS_SIZE..]))
    }

    fn validate(&self) -> Result<()> {
        crate::revision::validate_database_revision(self.database_revision).map_err(|error| {
            unsupported_page(format!(
                "Superblock database revision is unsupported: {}",
                error.message
            ))
        })?;
        if self.generation == 0 {
            return Err(invalid_page("Superblock generation must be non-zero"));
        }
        if self
            .bitmap_chunks
            .iter()
            .any(|chunk| chunk.generation == 0 || chunk.generation > self.generation)
        {
            return Err(invalid_page(
                "Allocation bitmap chunk generations must be non-zero and no newer than their superblock",
            ));
        }
        if self.max_page_count != MAX_PAGE_COUNT {
            return Err(unsupported_page(
                "Superblock maximum page count does not match this build",
            ));
        }
        if let Some(root_page_id) = self.catalog_root_page_id {
            validate_page_id(root_page_id)?;
            if root_page_id < FIRST_DATA_PAGE_ID {
                return Err(invalid_page(
                    "The database root cannot occupy a metadata page",
                ));
            }
        }
        let maximum_live_pages = (MAX_PAGE_COUNT - FIRST_DATA_PAGE_ID) as u32;
        if self.live_data_page_count > maximum_live_pages {
            return Err(invalid_page(storage_diagnostic!(
                "Live data page count {} exceeds the supported maximum {maximum_live_pages}",
                self.live_data_page_count
            )));
        }
        Ok(())
    }

    pub(crate) fn validate_bitmap(&self, bitmap: &AllocationBitmap) -> Result<()> {
        let live_data_pages = bitmap
            .allocated_page_count()
            .checked_sub(FIRST_DATA_PAGE_ID as u32)
            .ok_or_else(|| {
                invalid_page("Allocation bitmap does not reserve every metadata page")
            })?;
        if live_data_pages != self.live_data_page_count {
            return Err(invalid_page(
                "Superblock live page count does not match its allocation bitmap",
            ));
        }
        bitmap.validate_metadata_pages()?;
        if let Some(root_page_id) = self.catalog_root_page_id
            && !bitmap.is_allocated(root_page_id)?
        {
            return Err(invalid_page(
                "Superblock catalog root is not allocated in its bitmap",
            ));
        }
        Ok(())
    }
}

/// Which pages a root allocates, a bit for each. Each superblock carries the bits of the first
/// pages, and [chunk pages](BitmapChunk) the rest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AllocationBitmap {
    bits: Vec<u8>,
    /// How many bits are set, which every commit reads and records.
    allocated: u32,
}

impl AllocationBitmap {
    /// A bitmap that allocates only the metadata pages.
    pub(crate) fn new() -> Self {
        let mut bits = vec![0; ALLOCATION_BITMAP_BYTES];
        for id in 0..FIRST_DATA_PAGE_ID as usize {
            bits[id / 8] |= 1 << (id % 8);
        }
        Self {
            bits,
            allocated: FIRST_DATA_PAGE_ID as u32,
        }
    }

    pub(crate) fn is_allocated(&self, id: PageId) -> Result<bool> {
        validate_page_id(id)?;
        let byte = id as usize / 8;
        let bit = id as usize % 8;
        Ok(self.bits[byte] & (1 << bit) != 0)
    }

    pub(crate) fn set_allocated(&mut self, id: PageId, allocated: bool) -> Result<()> {
        validate_page_id(id)?;
        if id < FIRST_DATA_PAGE_ID && !allocated {
            return Err(invalid_page("Metadata pages cannot be deallocated"));
        }
        let byte = id as usize / 8;
        let mask = 1 << (id as usize % 8);
        if (self.bits[byte] & mask != 0) != allocated {
            self.bits[byte] ^= mask;
            if allocated {
                self.allocated += 1;
            } else {
                self.allocated -= 1;
            }
        }
        Ok(())
    }

    pub(crate) fn allocated_page_count(&self) -> u32 {
        self.allocated
    }

    /// The first page from `start` up to `end` that neither this bitmap nor `other` allocates.
    ///
    /// Allocated pages are passed over 64 at a time: a database's pages are mostly allocated, and
    /// a transaction looks for its first new page from the start of the file.
    pub(crate) fn first_free(&self, other: &Self, start: PageId, end: PageId) -> Option<PageId> {
        let (words, _) = self.bits.as_chunks::<8>();
        let (other_words, _) = other.bits.as_chunks::<8>();
        let end = end.min(MAX_PAGE_COUNT) as usize;
        let mut id = start as usize;
        while id < end {
            let word = id / 64;
            // The pages before `id` in its word count as allocated.
            let taken = u64::from_le_bytes(words[word])
                | u64::from_le_bytes(other_words[word])
                | ((1_u64 << (id % 64)) - 1);
            if taken != u64::MAX {
                let free = word * 64 + (!taken).trailing_zeros() as usize;
                return (free < end).then_some(free as PageId);
            }
            id = (word + 1) * 64;
        }
        None
    }

    /// The pages allocated here but not in `base`, below `end`, in order.
    pub(crate) fn allocated_since(&self, base: &Self, end: PageId) -> Vec<PageId> {
        let mut pages = Vec::new();
        let end = (end as usize).div_ceil(8).min(self.bits.len());
        for (byte, (bits, base)) in self.bits[..end].iter().zip(&base.bits).enumerate() {
            let mut fresh = bits & !base;
            while fresh != 0 {
                pages.push((byte * 8) as PageId + PageId::from(fresh.trailing_zeros()));
                fresh &= fresh - 1;
            }
        }
        pages
    }

    /// The bits each superblock carries.
    fn superblock_bits(&self) -> &[u8] {
        &self.bits[..SUPERBLOCK_BITMAP_BYTES]
    }

    fn chunk_bits(&self, chunk: usize) -> &[u8] {
        &self.bits[bitmap_chunk_range(chunk)]
    }

    /// Chunk `chunk` as the page that holds it in `slot`, written by the commit of `generation`.
    fn encode_chunk(&self, chunk: usize, slot: BitmapSlot, generation: u64) -> [u8; PAGE_SIZE] {
        let bits = self.chunk_bits(chunk);
        // Written in place, as each commit writes the chunks it changes.
        let mut bytes = [0; PAGE_SIZE];
        write_page_header(
            &mut bytes,
            slot.page_id(chunk),
            PageType::AllocationBitmap,
            BITMAP_CHUNK_HEADER_SIZE + bits.len(),
        );
        let payload = &mut bytes[PAGE_HEADER_SIZE..];
        put(payload, 0, generation.to_le_bytes());
        payload[8] = slot as u8;
        payload[9] = chunk as u8;
        payload[10] = BITMAP_CHUNK_COUNT as u8;
        put(payload, 12, (bits.len() as u16).to_le_bytes());
        payload[BITMAP_CHUNK_HEADER_SIZE..BITMAP_CHUNK_HEADER_SIZE + bits.len()]
            .copy_from_slice(bits);
        seal(&mut bytes);
        bytes
    }

    /// The bitmap of every page's bits, which must allocate each metadata page.
    fn from_bits(bits: Vec<u8>) -> Result<Self> {
        if bits.len() != ALLOCATION_BITMAP_BYTES {
            return Err(invalid_page("Allocation bitmap has the wrong total length"));
        }
        // Eight bytes at a time, as a bitmap is counted only when it is read.
        let (words, rest) = bits.as_chunks::<8>();
        let allocated = words
            .iter()
            .map(|word| u64::from_le_bytes(*word).count_ones())
            .sum::<u32>()
            + rest.iter().map(|byte| byte.count_ones()).sum::<u32>();
        let bitmap = Self { bits, allocated };
        bitmap.validate_metadata_pages()?;
        Ok(bitmap)
    }

    fn validate_metadata_pages(&self) -> Result<()> {
        for id in 0..FIRST_DATA_PAGE_ID {
            if !self.is_allocated(id)? {
                return Err(invalid_page(
                    "Allocation bitmap does not reserve every metadata page",
                ));
            }
        }
        Ok(())
    }
}

/// The bytes of the allocation bitmap that chunk `chunk` holds.
fn bitmap_chunk_range(chunk: usize) -> std::ops::Range<usize> {
    let start = SUPERBLOCK_BITMAP_BYTES + chunk * MAX_BITMAP_CHUNK_BYTES;
    start..(start + MAX_BITMAP_CHUNK_BYTES).min(ALLOCATION_BITMAP_BYTES)
}

/// The first page whose bit chunk `chunk` holds.
#[cfg(test)]
pub(crate) fn first_page_of_bitmap_chunk(chunk: usize) -> PageId {
    (bitmap_chunk_range(chunk).start * 8) as PageId
}

/// Reads the bits of chunk `chunk` from the page `entry` names, which must be the very page its
/// commit wrote there.
fn decode_bitmap_chunk<'a>(bytes: &'a [u8], chunk: usize, entry: &BitmapChunk) -> Result<&'a [u8]> {
    Page::verify(bytes)?;
    if read_u32(bytes, PAGE_CRC_OFFSET) != entry.checksum {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} is not the page its superblock recorded"
        )));
    }
    let page = PageRef::decode_verified(bytes)?;
    if page.id != entry.slot.page_id(chunk) {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} has page ID {}, expected {}",
            page.id,
            entry.slot.page_id(chunk)
        )));
    }
    if page.page_type != PageType::AllocationBitmap {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} has the wrong page type"
        )));
    }
    let payload = page.payload;
    if payload.len() < BITMAP_CHUNK_HEADER_SIZE {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} header is truncated"
        )));
    }
    if read_u64(payload, 0) != entry.generation {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} was written by another generation"
        )));
    }
    if BitmapSlot::try_from(payload[8])? != entry.slot {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} belongs to a different slot"
        )));
    }
    if payload[9] as usize != chunk || payload[10] as usize != BITMAP_CHUNK_COUNT {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} has inconsistent chunk metadata"
        )));
    }
    if payload[11] != 0 || payload[14..16].iter().any(|byte| *byte != 0) {
        return Err(invalid_page(
            "Allocation bitmap flags and reserved bytes must be zero",
        ));
    }
    let length = bitmap_chunk_range(chunk).len();
    if read_u16(payload, 12) as usize != length
        || payload.len() != BITMAP_CHUNK_HEADER_SIZE + length
    {
        return Err(invalid_page(storage_diagnostic!(
            "Allocation bitmap chunk {chunk} has an invalid payload length"
        )));
    }
    Ok(&payload[BITMAP_CHUNK_HEADER_SIZE..])
}

/// A device's metadata pages by page ID, as they were read. A page the device holds only in part,
/// or not at all, is shorter than a page.
pub(crate) type RawMetadata<'a> = [&'a [u8]; FIRST_DATA_PAGE_ID as usize];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredMetadata {
    pub superblock: Superblock,
    pub allocation_bitmap: AllocationBitmap,
}

impl RecoveredMetadata {
    /// The root of an empty database, at generation 1 in `slot`, with each bitmap chunk in the
    /// bitmap slot of the same name.
    pub(crate) fn empty(slot: SuperblockSlot) -> Self {
        let generation = 1;
        Self {
            superblock: Superblock {
                slot,
                generation,
                database_revision: 0,
                database_hash: crate::hash::EMPTY_HASH,
                catalog_root_page_id: None,
                live_data_page_count: 0,
                max_page_count: MAX_PAGE_COUNT,
                commit_hash: CommitHash::new().finish(),
                // Each chunk's checksum is recorded as its page is encoded.
                bitmap_chunks: [BitmapChunk {
                    slot: slot.bitmap_slot(),
                    generation,
                    checksum: 0,
                }; BITMAP_CHUNK_COUNT],
            },
            allocation_bitmap: AllocationBitmap::new(),
        }
    }

    /// Encodes the root's superblock and every chunk of its bitmap, each where the superblock
    /// names it, recording each chunk's checksum: every page that makes up the root.
    pub(crate) fn encode_pages(&mut self) -> Result<Vec<(PageId, [u8; PAGE_SIZE])>> {
        let mut pages = Vec::with_capacity(BITMAP_CHUNK_COUNT + 1);
        for (chunk, entry) in self.superblock.bitmap_chunks.iter_mut().enumerate() {
            let bytes = self
                .allocation_bitmap
                .encode_chunk(chunk, entry.slot, entry.generation);
            entry.checksum = page_checksum(&bytes);
            pages.push((entry.slot.page_id(chunk), bytes));
        }
        pages.push((
            self.superblock.slot.page_id(),
            self.superblock.encode_page(&self.allocation_bitmap)?,
        ));
        Ok(pages)
    }

    /// Whether two roots of the same generation hold the same database, wherever their bitmap
    /// chunks are kept.
    fn logically_matches(&self, other: &Self) -> bool {
        let (this, that) = (&self.superblock, &other.superblock);
        this.generation == that.generation
            && this.database_revision == that.database_revision
            && this.database_hash == that.database_hash
            && this.catalog_root_page_id == that.catalog_root_page_id
            && this.live_data_page_count == that.live_data_page_count
            && this.max_page_count == that.max_page_count
            && this.commit_hash == that.commit_hash
            && self.allocation_bitmap == other.allocation_bitmap
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingMetadata {
    pub superblock: Superblock,
    pub allocation_bitmap: AllocationBitmap,
    /// The page of each bitmap chunk the commit changed, to be written where the superblock names
    /// it. Every other chunk stays where the predecessor's superblock names it.
    pub bitmap_pages: [Option<[u8; PAGE_SIZE]>; BITMAP_CHUNK_COUNT],
}

/// The newest recoverable metadata root, and the root it replaced when that is recoverable too.
///
/// A commit writes its root in the slot beside its predecessor's, and makes its pages and its root
/// durable together, so an interrupted commit can leave a newest root naming pages that never
/// became durable. The predecessor is what recovery falls back to then.
pub(crate) fn recover_metadata(
    pages: RawMetadata<'_>,
) -> Result<(RecoveredMetadata, Option<RecoveredMetadata>)> {
    let candidate_a = decode_metadata_candidate(SuperblockSlot::A, &pages);
    let candidate_b = decode_metadata_candidate(SuperblockSlot::B, &pages);
    for candidate in [&candidate_a, &candidate_b] {
        if let Err(error) = candidate
            && error.code == "UNSUPPORTED_PAGE"
        {
            return Err(error.clone());
        }
    }
    match (candidate_a, candidate_b) {
        (Ok(candidate_a), Ok(candidate_b)) => {
            if candidate_a.superblock.generation == candidate_b.superblock.generation {
                if !candidate_a.logically_matches(&candidate_b) {
                    return Err(invalid_page(
                        "Equal-generation metadata slots contain different logical roots",
                    ));
                }
                Ok((candidate_a, None))
            } else {
                let (newest, older) =
                    if candidate_a.superblock.generation > candidate_b.superblock.generation {
                        (candidate_a, candidate_b)
                    } else {
                        (candidate_b, candidate_a)
                    };
                let previous = (older.superblock.generation.checked_add(1)
                    == Some(newest.superblock.generation))
                .then_some(older);
                Ok((newest, previous))
            }
        }
        (Ok(candidate), Err(_)) | (Err(_), Ok(candidate)) => Ok((candidate, None)),
        (Err(error_a), Err(error_b)) => Err(invalid_page(format!(
            "No valid metadata root remains; slot A: {}: {}; slot B: {}: {}",
            error_a.code, error_a.message, error_b.code, error_b.message
        ))),
    }
}

/// The root that publishes `allocation_bitmap` and the rest after `active`, in the other slot.
///
/// Each bitmap chunk the commit changes is encoded now, for the slot beside the one `active`
/// reads, so that `active` keeps every page it names; every other chunk stays where it is.
pub(crate) fn build_next_metadata(
    active: &RecoveredMetadata,
    database_revision: u64,
    database_hash: u64,
    catalog_root_page_id: Option<PageId>,
    allocation_bitmap: AllocationBitmap,
) -> Result<PendingMetadata> {
    crate::revision::validate_database_revision(database_revision)?;
    active
        .superblock
        .validate_bitmap(&active.allocation_bitmap)?;
    let generation = active.superblock.generation.checked_add(1).ok_or_else(|| {
        EngineError::new(
            "PAGE_GENERATION_EXHAUSTED",
            "The metadata generation cannot advance beyond u64::MAX",
        )
    })?;
    if database_revision < active.superblock.database_revision {
        return Err(invalid_page(
            "Database revision cannot move backwards during metadata publication",
        ));
    }
    let live_data_page_count = allocation_bitmap
        .allocated_page_count()
        .checked_sub(FIRST_DATA_PAGE_ID as u32)
        .ok_or_else(|| invalid_page("Allocation bitmap does not reserve every metadata page"))?;
    let mut superblock = Superblock {
        slot: active.superblock.slot.inactive(),
        generation,
        database_revision,
        database_hash,
        catalog_root_page_id,
        live_data_page_count,
        max_page_count: MAX_PAGE_COUNT,
        // The pager records the commit's pages once it has written them.
        commit_hash: CommitHash::new().finish(),
        bitmap_chunks: active.superblock.bitmap_chunks,
    };
    let mut bitmap_pages = [None; BITMAP_CHUNK_COUNT];
    for (chunk, entry) in superblock.bitmap_chunks.iter_mut().enumerate() {
        if allocation_bitmap.chunk_bits(chunk) != active.allocation_bitmap.chunk_bits(chunk) {
            let slot = entry.slot.inactive();
            let bytes = allocation_bitmap.encode_chunk(chunk, slot, generation);
            *entry = BitmapChunk {
                slot,
                generation,
                checksum: page_checksum(&bytes),
            };
            bitmap_pages[chunk] = Some(bytes);
        }
    }
    superblock.validate()?;
    superblock.validate_bitmap(&allocation_bitmap)?;
    Ok(PendingMetadata {
        superblock,
        allocation_bitmap,
        bitmap_pages,
    })
}

fn decode_metadata_candidate(
    slot: SuperblockSlot,
    pages: &RawMetadata<'_>,
) -> Result<RecoveredMetadata> {
    let (superblock, superblock_bits) = Superblock::decode_page(pages[slot.page_id() as usize])?;
    if superblock.slot != slot {
        return Err(invalid_page(storage_diagnostic!(
            "Metadata candidate {slot:?} contains superblock {:?}",
            superblock.slot
        )));
    }
    let mut bits = Vec::with_capacity(ALLOCATION_BITMAP_BYTES);
    bits.extend_from_slice(superblock_bits);
    for (chunk, entry) in superblock.bitmap_chunks.iter().enumerate() {
        let bytes = pages[entry.slot.page_id(chunk) as usize];
        bits.extend_from_slice(decode_bitmap_chunk(bytes, chunk, entry)?);
    }
    let allocation_bitmap = AllocationBitmap::from_bits(bits)?;
    superblock.validate_bitmap(&allocation_bitmap)?;
    Ok(RecoveredMetadata {
        superblock,
        allocation_bitmap,
    })
}

fn validate_page_id(id: PageId) -> Result<()> {
    if id >= MAX_PAGE_COUNT {
        return Err(invalid_page(storage_diagnostic!(
            "Page ID {id} exceeds the maximum page ID {}",
            MAX_PAGE_COUNT - 1
        )));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated length"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated length"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated length"),
    )
}

fn invalid_page(message: impl Into<String>) -> EngineError {
    EngineError::new("INVALID_PAGE", message)
}

fn unsupported_page(message: impl Into<String>) -> EngineError {
    EngineError::new("UNSUPPORTED_PAGE", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::EMPTY_HASH;

    /// A device's metadata pages.
    #[derive(Clone)]
    struct Image([[u8; PAGE_SIZE]; FIRST_DATA_PAGE_ID as usize]);

    impl Image {
        fn zeroed() -> Self {
            Self([[0; PAGE_SIZE]; FIRST_DATA_PAGE_ID as usize])
        }

        /// The metadata of an empty database, as a device is initialized with it.
        fn empty() -> Self {
            let mut image = Self::zeroed();
            for slot in [SuperblockSlot::A, SuperblockSlot::B] {
                image.write(&mut RecoveredMetadata::empty(slot));
            }
            image
        }

        fn raw(&self) -> RawMetadata<'_> {
            self.0.each_ref().map(|page| page.as_slice())
        }

        fn recover(&self) -> Result<(RecoveredMetadata, Option<RecoveredMetadata>)> {
            recover_metadata(self.raw())
        }

        /// Writes every page of `root`, recording its chunks' checksums.
        fn write(&mut self, root: &mut RecoveredMetadata) {
            for (id, bytes) in root.encode_pages().unwrap() {
                self.0[id as usize] = bytes;
            }
        }

        fn commit(&mut self, pending: &PendingMetadata) {
            for (id, bytes) in commit_writes(pending) {
                self.0[id as usize] = bytes;
            }
        }
    }

    /// The pages a commit of `pending` writes: the bitmap chunks it changed, and its superblock.
    fn commit_writes(pending: &PendingMetadata) -> Vec<(PageId, [u8; PAGE_SIZE])> {
        let mut writes = pending
            .bitmap_pages
            .iter()
            .enumerate()
            .filter_map(|(chunk, page)| {
                page.map(|page| {
                    (
                        pending.superblock.bitmap_chunks[chunk].slot.page_id(chunk),
                        page,
                    )
                })
            })
            .collect::<Vec<_>>();
        writes.push((
            pending.superblock.slot.page_id(),
            pending
                .superblock
                .encode_page(&pending.allocation_bitmap)
                .unwrap(),
        ));
        writes
    }

    /// A root at `generation` in `slot`, whose bitmap chunks are all in the bitmap slot of the
    /// same name and all written at that generation. It allocates a catalog root, and another page
    /// if `extra_page`.
    fn root(
        slot: SuperblockSlot,
        generation: u64,
        database_revision: u64,
        extra_page: bool,
    ) -> RecoveredMetadata {
        let mut root = RecoveredMetadata::empty(slot);
        root.superblock.generation = generation;
        root.superblock.database_revision = database_revision;
        for chunk in &mut root.superblock.bitmap_chunks {
            chunk.generation = generation;
        }
        root.superblock.catalog_root_page_id = Some(FIRST_DATA_PAGE_ID);
        root.allocation_bitmap
            .set_allocated(FIRST_DATA_PAGE_ID, true)
            .unwrap();
        if extra_page {
            root.allocation_bitmap
                .set_allocated(FIRST_DATA_PAGE_ID + 1, true)
                .unwrap();
        }
        root.superblock.live_data_page_count = if extra_page { 2 } else { 1 };
        root
    }

    /// An image holding a root at generation 8 in slot A, and its successor at generation 9 in
    /// slot B, which changed every chunk.
    fn older_and_newer() -> (Image, RecoveredMetadata, RecoveredMetadata) {
        let (mut older, mut newer) = (
            root(SuperblockSlot::A, 8, 20, false),
            root(SuperblockSlot::B, 9, 21, false),
        );
        let mut image = Image::zeroed();
        image.write(&mut older);
        image.write(&mut newer);
        (image, older, newer)
    }

    fn rewrite_crc(bytes: &mut [u8; PAGE_SIZE]) {
        bytes[PAGE_CRC_OFFSET..PAGE_CRC_OFFSET + 4].fill(0);
        let checksum = crate::checksum::crc32(bytes);
        bytes[PAGE_CRC_OFFSET..PAGE_CRC_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
    }

    fn mutate_payload(page: &mut [u8; PAGE_SIZE], offset: usize, value: u8) {
        page[PAGE_HEADER_SIZE + offset] = value;
        rewrite_crc(page);
    }

    #[test]
    fn page_round_trips_and_rejects_every_truncated_header() {
        let page = Page::new(42, PageType::BtreeLeaf, vec![1, 2, 3])
            .unwrap()
            .encode()
            .unwrap();
        assert_eq!(
            Page::decode(&page).unwrap(),
            Page::new(42, PageType::BtreeLeaf, vec![1, 2, 3]).unwrap()
        );
        for length in 0..PAGE_HEADER_SIZE {
            assert_eq!(
                Page::decode(&page[..length]).unwrap_err().code,
                "INVALID_PAGE"
            );
        }
        assert_eq!(
            Page::decode(&[0; PAGE_SIZE + 1]).unwrap_err().code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn page_rejects_corrupt_envelope_fields() {
        let original = Page::new(42, PageType::BtreeLeaf, vec![1, 2, 3])
            .unwrap()
            .encode()
            .unwrap();
        for (offset, value) in [
            (0, b'X'),
            (8, 2),
            (10, 1),
            (12, 43),
            (20, 99),
            (21, 1),
            (PAGE_HEADER_SIZE, 9),
            (PAGE_SIZE - 1, 1),
        ] {
            let mut corrupted = original;
            corrupted[offset] = value;
            assert_eq!(Page::decode(&corrupted).unwrap_err().code, "INVALID_PAGE");
        }

        for offset in [8, 10] {
            let mut unsupported = original;
            unsupported[offset] = if offset == 8 { 2 } else { 1 };
            rewrite_crc(&mut unsupported);
            assert_eq!(
                Page::decode(&unsupported).unwrap_err().code,
                "UNSUPPORTED_PAGE"
            );
        }

        let mut invalid_id = original;
        invalid_id[12..20].copy_from_slice(&MAX_PAGE_COUNT.to_le_bytes());
        rewrite_crc(&mut invalid_id);
        assert_eq!(Page::decode(&invalid_id).unwrap_err().code, "INVALID_PAGE");

        let mut invalid_length = original;
        invalid_length[24..28].copy_from_slice(&((MAX_PAGE_PAYLOAD_SIZE + 1) as u32).to_le_bytes());
        rewrite_crc(&mut invalid_length);
        assert_eq!(
            Page::decode(&invalid_length).unwrap_err().code,
            "INVALID_PAGE"
        );

        assert_eq!(
            Page::new(0, PageType::BtreeLeaf, vec![0; MAX_PAGE_PAYLOAD_SIZE + 1])
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn superblock_round_trips_with_the_start_of_its_bitmap() {
        for slot in [SuperblockSlot::A, SuperblockSlot::B] {
            let mut root = root(slot, 8, 31, false);
            root.superblock.database_hash = 0xfeed_face_dead_beef;
            root.superblock.commit_hash = 0x0123_4567_89ab_cdef;
            root.superblock.bitmap_chunks[1] = BitmapChunk {
                slot: slot.bitmap_slot().inactive(),
                generation: 3,
                checksum: 0xabcd_ef01,
            };
            // The last page whose bit the superblock carries.
            root.allocation_bitmap
                .set_allocated(first_page_of_bitmap_chunk(0) - 1, true)
                .unwrap();
            root.superblock.live_data_page_count += 1;
            let page = root
                .superblock
                .encode_page(&root.allocation_bitmap)
                .unwrap();
            let (superblock, bits) = Superblock::decode_page(&page).unwrap();
            assert_eq!(superblock, root.superblock);
            assert_eq!(bits, root.allocation_bitmap.superblock_bits());
        }
    }

    #[test]
    fn superblock_rejects_invalid_and_unsupported_fields() {
        let root = root(SuperblockSlot::A, 8, 20, false);
        let page = root
            .superblock
            .encode_page(&root.allocation_bitmap)
            .unwrap();
        for (offset, value, code) in [
            (0, b'X', "INVALID_PAGE"),
            // Page format 2 databases, from v0.1.0 through v0.3.0, are refused, as is a future
            // format.
            (8, 2, "UNSUPPORTED_PAGE"),
            (8, 4, "UNSUPPORTED_PAGE"),
            (10, 1, "UNSUPPORTED_PAGE"),
            (12, 1, "UNSUPPORTED_PAGE"),
            (16, 1, "UNSUPPORTED_PAGE"),
            (24, 0, "INVALID_PAGE"),
            (60, 3, "UNSUPPORTED_PAGE"),
            (62, 2, "INVALID_PAGE"),
            (63, 1, "INVALID_PAGE"),
            // A chunk written by no generation, or by a later one than the superblock's.
            (72, 0, "INVALID_PAGE"),
            (72, 9, "INVALID_PAGE"),
            (84, 2, "INVALID_PAGE"),
            (85, 1, "INVALID_PAGE"),
            (104, 1, "INVALID_PAGE"),
            (127, 1, "INVALID_PAGE"),
        ] {
            let mut corrupted = page;
            mutate_payload(&mut corrupted, offset, value);
            assert_eq!(
                Superblock::decode_page(&corrupted).unwrap_err().code,
                code,
                "payload byte {offset}"
            );
        }

        // A superblock of v0.1.0 through v0.3.0 is shorter, and is refused as unsupported
        // rather than as corrupt; one of this format at that length is corrupt.
        for (version, code) in [(2_u16, "UNSUPPORTED_PAGE"), (3, "INVALID_PAGE")] {
            let mut payload = vec![0; 96];
            payload[..8].copy_from_slice(SUPERBLOCK_MAGIC);
            payload[8..10].copy_from_slice(&version.to_le_bytes());
            payload[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
            payload[16..24].copy_from_slice(&MAX_PAGE_COUNT.to_le_bytes());
            let short = Page::new(0, PageType::Superblock, payload)
                .unwrap()
                .encode()
                .unwrap();
            assert_eq!(Superblock::decode_page(&short).unwrap_err().code, code);
        }
        let truncated = Page::new(0, PageType::Superblock, SUPERBLOCK_MAGIC.to_vec())
            .unwrap()
            .encode()
            .unwrap();
        assert_eq!(
            Superblock::decode_page(&truncated).unwrap_err().code,
            "INVALID_PAGE"
        );

        let mut wrong_page_id = page;
        wrong_page_id[12..20].copy_from_slice(&1_u64.to_le_bytes());
        rewrite_crc(&mut wrong_page_id);
        assert_eq!(
            Superblock::decode_page(&wrong_page_id).unwrap_err().code,
            "INVALID_PAGE"
        );

        let mut wrong_type = page;
        wrong_type[20] = PageType::BtreeLeaf as u8;
        rewrite_crc(&mut wrong_type);
        assert_eq!(
            Superblock::decode_page(&wrong_type).unwrap_err().code,
            "INVALID_PAGE"
        );

        let mut maximum = RecoveredMetadata::empty(SuperblockSlot::A);
        maximum.superblock.database_revision = crate::revision::MAX_DATABASE_REVISION;
        let page = maximum
            .superblock
            .encode_page(&maximum.allocation_bitmap)
            .unwrap();
        assert_eq!(
            Superblock::decode_page(&page).unwrap().0.database_revision,
            crate::revision::MAX_DATABASE_REVISION
        );
        maximum.superblock.database_revision = crate::revision::MAX_DATABASE_REVISION + 1;
        assert_eq!(
            maximum
                .superblock
                .encode_page(&maximum.allocation_bitmap)
                .unwrap_err()
                .code,
            "UNSUPPORTED_PAGE"
        );
    }

    #[test]
    fn superblock_validates_its_bitmap_live_count_and_root() {
        let root = root(SuperblockSlot::B, 11, 30, false);
        root.superblock
            .validate_bitmap(&root.allocation_bitmap)
            .unwrap();

        let mut extra = root.allocation_bitmap.clone();
        extra.set_allocated(FIRST_DATA_PAGE_ID + 1, true).unwrap();
        assert_eq!(
            root.superblock.validate_bitmap(&extra).unwrap_err().code,
            "INVALID_PAGE"
        );
        assert_eq!(
            root.superblock.encode_page(&extra).unwrap_err().code,
            "INVALID_PAGE"
        );

        let mut missing_root = AllocationBitmap::new();
        missing_root
            .set_allocated(FIRST_DATA_PAGE_ID + 1, true)
            .unwrap();
        assert_eq!(
            root.superblock
                .validate_bitmap(&missing_root)
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn allocation_bitmaps_round_trip_through_their_superblock_and_chunks() {
        for slot in [SuperblockSlot::A, SuperblockSlot::B] {
            let mut root = root(slot, 17, 40, false);
            let ids = [
                FIRST_DATA_PAGE_ID + 1,
                first_page_of_bitmap_chunk(0) - 1,
                first_page_of_bitmap_chunk(0),
                first_page_of_bitmap_chunk(1) - 1,
                first_page_of_bitmap_chunk(1),
                MAX_PAGE_COUNT - 1,
            ];
            for id in ids {
                root.allocation_bitmap.set_allocated(id, true).unwrap();
            }
            root.superblock.live_data_page_count += ids.len() as u32;
            let mut image = Image::zeroed();
            image.write(&mut root);
            let (recovered, previous) = image.recover().unwrap();
            assert_eq!(recovered, root);
            assert!(previous.is_none());
            assert_eq!(
                recovered.allocation_bitmap.allocated_page_count(),
                FIRST_DATA_PAGE_ID as u32 + 1 + ids.len() as u32
            );
        }
    }

    #[test]
    fn metadata_pages_written_in_place_match_their_pages_as_payloads() {
        // Commits write the superblock and bitmap pages in place; each must be byte for byte the
        // page its payload makes.
        let mut root = root(SuperblockSlot::B, 9, 40, false);
        root.superblock.database_hash = 0x0123_4567_89ab_cdef;
        root.superblock.commit_hash = 0xdead_beef;
        root.superblock.bitmap_chunks[0] = BitmapChunk {
            slot: BitmapSlot::A,
            generation: 7,
            checksum: 0x1234_5678,
        };
        for id in [FIRST_DATA_PAGE_ID + 70, MAX_PAGE_COUNT - 1] {
            root.allocation_bitmap.set_allocated(id, true).unwrap();
            root.superblock.live_data_page_count += 1;
        }
        let mut payload = vec![0; MAX_PAGE_PAYLOAD_SIZE];
        payload[..8].copy_from_slice(SUPERBLOCK_MAGIC);
        payload[8..10].copy_from_slice(&SUPERBLOCK_FORMAT_VERSION.to_le_bytes());
        payload[10..12].copy_from_slice(&SUPERBLOCK_FLAGS.to_le_bytes());
        payload[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        payload[16..24].copy_from_slice(&MAX_PAGE_COUNT.to_le_bytes());
        payload[24..32].copy_from_slice(&9_u64.to_le_bytes());
        payload[32..40].copy_from_slice(&40_u64.to_le_bytes());
        payload[40..48].copy_from_slice(&0x0123_4567_89ab_cdef_u64.to_le_bytes());
        payload[48..56].copy_from_slice(&FIRST_DATA_PAGE_ID.to_le_bytes());
        payload[56..60].copy_from_slice(&3_u32.to_le_bytes());
        payload[60..62].copy_from_slice(&(BITMAP_CHUNK_COUNT as u16).to_le_bytes());
        payload[62] = SuperblockSlot::B as u8;
        payload[64..72].copy_from_slice(&0xdead_beef_u64.to_le_bytes());
        payload[72..80].copy_from_slice(&7_u64.to_le_bytes());
        payload[80..84].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        payload[84] = BitmapSlot::A as u8;
        payload[88..96].copy_from_slice(&9_u64.to_le_bytes());
        payload[100] = BitmapSlot::B as u8;
        payload[SUPERBLOCK_FIELDS_SIZE..]
            .copy_from_slice(&root.allocation_bitmap.bits[..SUPERBLOCK_BITMAP_BYTES]);
        let expected = Page::new(SuperblockSlot::B.page_id(), PageType::Superblock, payload)
            .unwrap()
            .encode()
            .unwrap();
        assert_eq!(
            root.superblock
                .encode_page(&root.allocation_bitmap)
                .unwrap(),
            expected
        );

        for chunk in 0..BITMAP_CHUNK_COUNT {
            let bits = &root.allocation_bitmap.bits[bitmap_chunk_range(chunk)];
            let mut payload = 9_u64.to_le_bytes().to_vec();
            payload.extend([
                BitmapSlot::B as u8,
                chunk as u8,
                BITMAP_CHUNK_COUNT as u8,
                0,
            ]);
            payload.extend((bits.len() as u16).to_le_bytes());
            payload.extend([0, 0]);
            payload.extend_from_slice(bits);
            let expected = Page::new(
                BitmapSlot::B.page_id(chunk),
                PageType::AllocationBitmap,
                payload,
            )
            .unwrap()
            .encode()
            .unwrap();
            assert_eq!(
                root.allocation_bitmap.encode_chunk(chunk, BitmapSlot::B, 9),
                expected,
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn bitmap_chunks_must_be_the_pages_their_superblock_recorded() {
        let bitmap = AllocationBitmap::new();
        let page = bitmap.encode_chunk(0, BitmapSlot::A, 17);
        let entry = BitmapChunk {
            slot: BitmapSlot::A,
            generation: 17,
            checksum: page_checksum(&page),
        };
        assert_eq!(
            decode_bitmap_chunk(&page, 0, &entry).unwrap(),
            bitmap.chunk_bits(0)
        );

        for length in [0, PAGE_HEADER_SIZE, PAGE_SIZE - 1] {
            assert_eq!(
                decode_bitmap_chunk(&page[..length], 0, &entry)
                    .unwrap_err()
                    .code,
                "INVALID_PAGE"
            );
        }

        // A sound page other than the one recorded, as an abandoned commit at the same generation
        // could leave in the slot.
        let mut other = bitmap.clone();
        other
            .set_allocated(first_page_of_bitmap_chunk(0), true)
            .unwrap();
        assert_eq!(
            decode_bitmap_chunk(&other.encode_chunk(0, BitmapSlot::A, 17), 0, &entry)
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );

        // Each field, even of a page whose checksum the superblock recorded.
        let mut corrupted_pages = Vec::new();
        for (offset, value) in [
            (0, 0),
            (0, 16),
            (8, 1),
            (8, 2),
            (9, 1),
            (10, 3),
            (11, 1),
            (12, 0),
            (14, 1),
        ] {
            let mut corrupted = page;
            mutate_payload(&mut corrupted, offset, value);
            corrupted_pages.push(corrupted);
        }
        let mut wrong_id = page;
        wrong_id[12..20].copy_from_slice(&BitmapSlot::A.page_id(1).to_le_bytes());
        rewrite_crc(&mut wrong_id);
        corrupted_pages.push(wrong_id);
        let mut wrong_type = page;
        wrong_type[20] = PageType::BtreeLeaf as u8;
        rewrite_crc(&mut wrong_type);
        corrupted_pages.push(wrong_type);
        for corrupted in corrupted_pages {
            let entry = BitmapChunk {
                checksum: page_checksum(&corrupted),
                ..entry
            };
            assert_eq!(
                decode_bitmap_chunk(&corrupted, 0, &entry).unwrap_err().code,
                "INVALID_PAGE"
            );
        }

        let mut freed_metadata = AllocationBitmap::new().bits;
        freed_metadata[0] &= !1;
        assert_eq!(
            AllocationBitmap::from_bits(freed_metadata)
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
        assert_eq!(
            AllocationBitmap::from_bits(vec![0xff; ALLOCATION_BITMAP_BYTES - 1])
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn first_free_finds_the_page_a_page_by_page_search_finds() {
        let mut random = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random
        };
        for case in 0..32 {
            let (mut this, mut other) = (AllocationBitmap::new(), AllocationBitmap::new());
            // Every page allocated in one bitmap or the other up to a point, as a database's pages
            // are, and some free among them, then a random mix.
            let full = (next() % MAX_PAGE_COUNT) as usize;
            for (byte, (this, other)) in this.bits.iter_mut().zip(&mut other.bits).enumerate() {
                let bits = next() as u8;
                (*this, *other) = if byte * 8 < full {
                    (bits, !bits)
                } else {
                    (bits, next() as u8)
                };
            }
            for _ in 0..case % 4 {
                let id = next() as usize % full.max(1);
                this.bits[id / 8] &= !(1 << (id % 8));
                other.bits[id / 8] &= !(1 << (id % 8));
            }
            for _ in 0..16 {
                let start = next() % MAX_PAGE_COUNT;
                let end = start + next() % (MAX_PAGE_COUNT - start + 1);
                let expected = (start..end).find(|id| {
                    !this.is_allocated(*id).unwrap() && !other.is_allocated(*id).unwrap()
                });
                assert_eq!(
                    this.first_free(&other, start, end),
                    expected,
                    "case {case}, from {start} to {end}"
                );
            }
        }
        let full = AllocationBitmap {
            bits: vec![0xff; ALLOCATION_BITMAP_BYTES],
            allocated: MAX_PAGE_COUNT as u32,
        };
        assert_eq!(full.first_free(&full, 0, MAX_PAGE_COUNT), None);
        let empty = AllocationBitmap::new();
        assert_eq!(
            empty.first_free(&empty, 0, MAX_PAGE_COUNT),
            Some(FIRST_DATA_PAGE_ID)
        );
        assert_eq!(empty.first_free(&empty, 70, 70), None);
        assert_eq!(empty.first_free(&empty, 70, 71), Some(70));
    }

    #[test]
    fn allocation_bitmap_bounds_page_ids() {
        let mut bitmap = AllocationBitmap::new();
        assert!(bitmap.is_allocated(0).unwrap());
        assert_eq!(bitmap.allocated_page_count(), FIRST_DATA_PAGE_ID as u32);
        assert_eq!(
            bitmap.set_allocated(MAX_PAGE_COUNT, true).unwrap_err().code,
            "INVALID_PAGE"
        );
        assert_eq!(
            bitmap.is_allocated(MAX_PAGE_COUNT).unwrap_err().code,
            "INVALID_PAGE"
        );
        assert_eq!(
            bitmap.set_allocated(0, false).unwrap_err().code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn metadata_recovery_selects_the_highest_complete_generation() {
        let (image, _, _) = older_and_newer();
        let (recovered, previous) = image.recover().unwrap();
        assert_eq!(recovered.superblock.slot, SuperblockSlot::B);
        assert_eq!(recovered.superblock.generation, 9);
        assert_eq!(recovered.superblock.database_revision, 21);
        // The older root is the newer one's predecessor, which recovery can fall back to.
        assert_eq!(previous.unwrap().superblock.generation, 8);

        let mut distant = image;
        distant.write(&mut root(SuperblockSlot::B, 10, 22, false));
        let (recovered, previous) = distant.recover().unwrap();
        assert_eq!(recovered.superblock.generation, 10);
        assert!(previous.is_none());
    }

    #[test]
    fn metadata_recovery_falls_back_from_each_torn_or_corrupt_newer_page() {
        let (image, older, _) = older_and_newer();
        let newer_pages = [
            SuperblockSlot::B.page_id(),
            BitmapSlot::B.page_id(0),
            BitmapSlot::B.page_id(1),
        ];
        for id in newer_pages {
            let id = id as usize;
            for length in [0, 1, PAGE_HEADER_SIZE, PAGE_SIZE / 2, PAGE_SIZE - 1] {
                let mut raw = image.raw();
                raw[id] = &image.0[id][..length];
                assert_eq!(
                    recover_metadata(raw).unwrap().0,
                    older,
                    "page {id} {length}"
                );
            }
            // A page torn with a tail other than the one written.
            for split in [1, PAGE_SIZE / 2, PAGE_SIZE - 1] {
                let mut torn = image.clone();
                for byte in &mut torn.0[id][split..] {
                    *byte = !*byte;
                }
                assert_eq!(
                    torn.recover().unwrap().0,
                    older,
                    "page {id} torn at {split}"
                );
            }
            let mut corrupted = image.clone();
            corrupted.0[id][PAGE_SIZE - 1] ^= 1;
            assert_eq!(corrupted.recover().unwrap().0, older, "page {id} corrupted");
        }
    }

    #[test]
    fn metadata_recovery_fails_when_neither_root_is_valid() {
        let error = Image::zeroed().recover().unwrap_err();
        assert_eq!(error.code, "INVALID_PAGE");
        assert!(error.message.contains("No valid metadata root remains"));
    }

    #[test]
    fn metadata_recovery_refuses_a_chunk_an_abandoned_commit_wrote() {
        // A commit that failed before its superblock leaves its chunk in the slot the next commit
        // at its generation writes. If that commit's superblock becomes durable but its chunk does
        // not, the chunk left behind has the generation and slot the superblock names, but not
        // the checksum.
        let mut active = root(SuperblockSlot::A, 8, 20, false);
        let mut image = Image::zeroed();
        image.write(&mut active);
        let pending = |page| {
            let mut bitmap = active.allocation_bitmap.clone();
            bitmap.set_allocated(page, true).unwrap();
            build_next_metadata(&active, 21, EMPTY_HASH, Some(FIRST_DATA_PAGE_ID), bitmap).unwrap()
        };
        let abandoned = pending(first_page_of_bitmap_chunk(0));
        let retried = pending(first_page_of_bitmap_chunk(0) + 1);
        let chunk = BitmapSlot::B.page_id(0) as usize;
        image.0[chunk] = abandoned.bitmap_pages[0].unwrap();
        let superblock = SuperblockSlot::B.page_id() as usize;
        image.0[superblock] = retried
            .superblock
            .encode_page(&retried.allocation_bitmap)
            .unwrap();
        assert_eq!(image.recover().unwrap().0, active);

        image.0[chunk] = retried.bitmap_pages[0].unwrap();
        let (recovered, previous) = image.recover().unwrap();
        assert_eq!(recovered.superblock, retried.superblock);
        assert_eq!(recovered.allocation_bitmap, retried.allocation_bitmap);
        assert_eq!(previous, Some(active));
    }

    #[test]
    fn every_interrupted_commit_recovers_its_predecessor_or_itself() {
        // Commits that change the bits the superblock carries, either chunk, several, or none.
        // A commit writes its changed chunks and its superblock, then flushes them together, so an
        // interruption can leave any of them whole, torn, or unwritten. The root it replaces must
        // survive every combination, since the commit never writes a page that root reads.
        let mut image = Image::empty();
        let (mut active, _) = image.recover().unwrap();
        let mut next_page = [
            FIRST_DATA_PAGE_ID,
            first_page_of_bitmap_chunk(0),
            first_page_of_bitmap_chunk(1),
        ];
        let steps: [&[usize]; 11] = [
            &[0],
            &[1],
            &[0],
            &[2],
            &[1, 2],
            &[],
            &[0, 1, 2],
            &[1],
            &[1],
            &[2],
            &[0, 2],
        ];
        for (step, parts) in steps.iter().enumerate() {
            let mut bitmap = active.allocation_bitmap.clone();
            for &part in *parts {
                bitmap.set_allocated(next_page[part], true).unwrap();
                next_page[part] += 1;
            }
            let mut pending = build_next_metadata(
                &active,
                active.superblock.database_revision + 1,
                EMPTY_HASH,
                None,
                bitmap,
            )
            .unwrap();
            pending.superblock.commit_hash = step as u64;
            let writes = commit_writes(&pending);
            assert_eq!(
                writes.len(),
                1 + parts.iter().filter(|part| **part > 0).count()
            );
            for outcome in 0..3_usize.pow(writes.len() as u32) {
                let mut cut = image.clone();
                let mut code = outcome;
                for (id, bytes) in &writes {
                    let page = &mut cut.0[*id as usize];
                    match code % 3 {
                        0 => {}
                        1 => *page = *bytes,
                        _ => page[..PAGE_SIZE / 2].copy_from_slice(&bytes[..PAGE_SIZE / 2]),
                    }
                    code /= 3;
                }
                let (recovered, previous) = cut.recover().unwrap();
                if writes
                    .iter()
                    .all(|(id, bytes)| cut.0[*id as usize] == *bytes)
                {
                    assert_eq!(recovered.superblock, pending.superblock);
                    assert_eq!(recovered.allocation_bitmap, pending.allocation_bitmap);
                    assert_eq!(previous.as_ref(), Some(&active));
                } else {
                    assert_eq!(recovered, active, "step {step}, outcome {outcome}");
                }
            }
            image.commit(&pending);
            active = RecoveredMetadata {
                superblock: pending.superblock,
                allocation_bitmap: pending.allocation_bitmap,
            };
            assert_eq!(image.recover().unwrap().0, active);
        }
    }

    #[test]
    fn equal_generation_roots_must_be_logically_identical() {
        let (recovered, previous) = Image::empty().recover().unwrap();
        let mut empty = RecoveredMetadata::empty(SuperblockSlot::A);
        empty.encode_pages().unwrap();
        assert_eq!(recovered, empty);
        assert!(previous.is_none());

        let mut image = Image::zeroed();
        image.write(&mut root(SuperblockSlot::A, 12, 30, false));
        let mut same = image.clone();
        same.write(&mut root(SuperblockSlot::B, 12, 30, false));
        let (recovered, previous) = same.recover().unwrap();
        assert_eq!(recovered.superblock.generation, 12);
        assert!(previous.is_none());

        for mut different in [
            root(SuperblockSlot::B, 12, 31, false),
            root(SuperblockSlot::B, 12, 30, true),
        ] {
            let mut image = image.clone();
            image.write(&mut different);
            assert_eq!(image.recover().unwrap_err().code, "INVALID_PAGE");
        }
    }

    #[test]
    fn unsupported_metadata_fails_closed_even_with_an_older_valid_root() {
        let (image, _, _) = older_and_newer();
        for id in [
            SuperblockSlot::B.page_id(),
            BitmapSlot::B.page_id(0),
            BitmapSlot::B.page_id(1),
        ] {
            let mut unsupported = image.clone();
            let page = &mut unsupported.0[id as usize];
            page[8..10].copy_from_slice(&2_u16.to_le_bytes());
            rewrite_crc(page);
            assert_eq!(
                unsupported.recover().unwrap_err().code,
                "UNSUPPORTED_PAGE",
                "page {id}"
            );
        }

        let mut unsupported = image.clone();
        mutate_payload(
            &mut unsupported.0[SuperblockSlot::B.page_id() as usize],
            8,
            2,
        );
        assert_eq!(unsupported.recover().unwrap_err().code, "UNSUPPORTED_PAGE");

        let mut image = Image::zeroed();
        let maximum = crate::revision::MAX_DATABASE_REVISION;
        image.write(&mut root(SuperblockSlot::A, 8, maximum, false));
        image.write(&mut root(SuperblockSlot::B, 9, maximum, false));
        let superblock = &mut image.0[SuperblockSlot::B.page_id() as usize];
        superblock[PAGE_HEADER_SIZE + 32..PAGE_HEADER_SIZE + 40]
            .copy_from_slice(&(maximum + 1).to_le_bytes());
        rewrite_crc(superblock);
        assert_eq!(image.recover().unwrap_err().code, "UNSUPPORTED_PAGE");
    }

    #[test]
    fn next_metadata_writes_only_the_chunks_a_commit_changes() {
        let mut active = root(SuperblockSlot::A, 8, 20, false);
        Image::zeroed().write(&mut active);

        // A page whose bit the superblock carries.
        let mut bitmap = active.allocation_bitmap.clone();
        bitmap.set_allocated(FIRST_DATA_PAGE_ID + 1, true).unwrap();
        let pending = build_next_metadata(
            &active,
            21,
            EMPTY_HASH,
            Some(FIRST_DATA_PAGE_ID + 1),
            bitmap,
        )
        .unwrap();
        assert_eq!(pending.superblock.slot, SuperblockSlot::B);
        assert_eq!(pending.superblock.generation, 9);
        assert_eq!(pending.superblock.live_data_page_count, 2);
        assert_eq!(
            pending.superblock.bitmap_chunks,
            active.superblock.bitmap_chunks
        );
        assert_eq!(pending.bitmap_pages, [None; BITMAP_CHUNK_COUNT]);

        // A page in each chunk moves that chunk alone to the other slot.
        for chunk in 0..BITMAP_CHUNK_COUNT {
            let mut bitmap = active.allocation_bitmap.clone();
            bitmap
                .set_allocated(first_page_of_bitmap_chunk(chunk) + 3, true)
                .unwrap();
            let pending =
                build_next_metadata(&active, 21, EMPTY_HASH, Some(FIRST_DATA_PAGE_ID), bitmap)
                    .unwrap();
            for (other, entry) in pending.superblock.bitmap_chunks.iter().enumerate() {
                if other == chunk {
                    assert_eq!(entry.slot, BitmapSlot::B);
                    assert_eq!(entry.generation, 9);
                    let page = pending.bitmap_pages[chunk].unwrap();
                    assert_eq!(
                        decode_bitmap_chunk(&page, chunk, entry).unwrap(),
                        pending.allocation_bitmap.chunk_bits(chunk)
                    );
                } else {
                    assert_eq!(*entry, active.superblock.bitmap_chunks[other]);
                    assert!(pending.bitmap_pages[other].is_none());
                }
            }
        }
    }

    #[test]
    fn next_metadata_rejects_generation_wrap_and_backwards_revision() {
        let active = root(SuperblockSlot::A, 8, 20, false);
        assert_eq!(
            build_next_metadata(
                &active,
                19,
                EMPTY_HASH,
                Some(FIRST_DATA_PAGE_ID),
                active.allocation_bitmap.clone(),
            )
            .unwrap_err()
            .code,
            "INVALID_PAGE"
        );
        let exhausted = root(SuperblockSlot::A, u64::MAX, 20, false);
        assert_eq!(
            build_next_metadata(
                &exhausted,
                21,
                EMPTY_HASH,
                Some(FIRST_DATA_PAGE_ID),
                exhausted.allocation_bitmap.clone(),
            )
            .unwrap_err()
            .code,
            "PAGE_GENERATION_EXHAUSTED"
        );
    }
}
