use crate::{EngineError, Result, snapshot::crc32};

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
pub const MAX_DATABASE_BYTES: u64 = MAX_PAGE_COUNT * PAGE_SIZE as u64;
pub const PAGE_HEADER_SIZE: usize = 32;
pub const MAX_PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

pub const SUPERBLOCK_PAGE_COUNT: usize = 2;
pub const BITMAP_CHUNK_COUNT: usize = 3;
pub const BITMAP_PAGE_COUNT: usize = BITMAP_CHUNK_COUNT * 2;
pub const FIRST_DATA_PAGE_ID: PageId =
    SUPERBLOCK_PAGE_COUNT as PageId + BITMAP_PAGE_COUNT as PageId;

const PAGE_MAGIC: &[u8; 8] = b"TGRPAGE\0";
const PAGE_FORMAT_VERSION: u16 = 1;
const PAGE_FLAGS: u16 = 0;
const PAGE_CRC_OFFSET: usize = 28;

const SUPERBLOCK_MAGIC: &[u8; 8] = b"TGRSUPR\0";
const SUPERBLOCK_FORMAT_VERSION: u16 = 1;
const SUPERBLOCK_FLAGS: u16 = 0;
const SUPERBLOCK_PAYLOAD_SIZE: usize = 96;

pub const ALLOCATION_BITMAP_BYTES: usize = MAX_PAGE_COUNT as usize / 8;
const BITMAP_CHUNK_HEADER_SIZE: usize = 16;
const MAX_BITMAP_CHUNK_BYTES: usize = MAX_PAGE_PAYLOAD_SIZE - BITMAP_CHUNK_HEADER_SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PageType {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page {
    pub id: PageId,
    pub page_type: PageType,
    pub payload: Vec<u8>,
}

impl Page {
    pub fn new(id: PageId, page_type: PageType, payload: Vec<u8>) -> Result<Self> {
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

    pub fn encode(&self) -> Result<[u8; PAGE_SIZE]> {
        validate_page_id(self.id)?;
        if self.payload.len() > MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_page(format!(
                "Page payload is {} bytes, exceeding the {}-byte limit",
                self.payload.len(),
                MAX_PAGE_PAYLOAD_SIZE
            )));
        }

        let mut bytes = [0; PAGE_SIZE];
        bytes[..8].copy_from_slice(PAGE_MAGIC);
        bytes[8..10].copy_from_slice(&PAGE_FORMAT_VERSION.to_le_bytes());
        bytes[10..12].copy_from_slice(&PAGE_FLAGS.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.id.to_le_bytes());
        bytes[20] = self.page_type as u8;
        bytes[24..28].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        bytes[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + self.payload.len()]
            .copy_from_slice(&self.payload);
        let checksum = crc32(&bytes);
        bytes[PAGE_CRC_OFFSET..PAGE_CRC_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != PAGE_SIZE {
            return Err(invalid_page(storage_diagnostic!(
                "A physical page must be exactly {PAGE_SIZE} bytes, not {}",
                bytes.len()
            )));
        }
        // Treat the envelope fields as authoritative only after the physical
        // page checksum succeeds. A torn write can otherwise turn a version
        // byte into an apparently coherent unsupported format and prevent
        // recovery from the other metadata slot.
        let expected_checksum = read_u32(bytes, PAGE_CRC_OFFSET);
        let mut checksum_bytes = [0; PAGE_SIZE];
        checksum_bytes.copy_from_slice(bytes);
        checksum_bytes[PAGE_CRC_OFFSET..PAGE_CRC_OFFSET + 4].fill(0);
        if crc32(&checksum_bytes) != expected_checksum {
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
        let page_type = PageType::try_from(bytes[20])?;
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

        Ok(Self {
            id,
            page_type,
            payload: bytes[PAGE_HEADER_SIZE..payload_end].to_vec(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SuperblockSlot {
    A = 0,
    B = 1,
}

impl SuperblockSlot {
    pub const fn page_id(self) -> PageId {
        self as PageId
    }

    pub const fn inactive(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    pub const fn bitmap_slot(self) -> BitmapSlot {
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
pub enum BitmapSlot {
    A = 0,
    B = 1,
}

impl BitmapSlot {
    pub const fn first_page_id(self) -> PageId {
        match self {
            Self::A => SUPERBLOCK_PAGE_COUNT as PageId,
            Self::B => SUPERBLOCK_PAGE_COUNT as PageId + BITMAP_CHUNK_COUNT as PageId,
        }
    }

    pub const fn page_id(self, chunk: usize) -> PageId {
        self.first_page_id() + chunk as PageId
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Superblock {
    pub slot: SuperblockSlot,
    pub generation: u64,
    pub database_revision: u64,
    pub applied_journal_sequence: u64,
    pub bitmap_slot: BitmapSlot,
    pub bitmap_generation: u64,
    pub catalog_root_page_id: Option<PageId>,
    pub live_data_page_count: u32,
    pub max_page_count: PageId,
}

impl Superblock {
    pub fn new(slot: SuperblockSlot) -> Self {
        let bitmap_slot = match slot {
            SuperblockSlot::A => BitmapSlot::A,
            SuperblockSlot::B => BitmapSlot::B,
        };
        Self {
            slot,
            generation: 1,
            database_revision: 0,
            applied_journal_sequence: 0,
            bitmap_slot,
            bitmap_generation: 1,
            catalog_root_page_id: None,
            live_data_page_count: 0,
            max_page_count: MAX_PAGE_COUNT,
        }
    }

    pub fn encode_page(&self) -> Result<[u8; PAGE_SIZE]> {
        self.validate()?;
        let mut payload = vec![0; SUPERBLOCK_PAYLOAD_SIZE];
        payload[..8].copy_from_slice(SUPERBLOCK_MAGIC);
        payload[8..10].copy_from_slice(&SUPERBLOCK_FORMAT_VERSION.to_le_bytes());
        payload[10..12].copy_from_slice(&SUPERBLOCK_FLAGS.to_le_bytes());
        payload[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        payload[16..24].copy_from_slice(&self.max_page_count.to_le_bytes());
        payload[24..32].copy_from_slice(&self.generation.to_le_bytes());
        payload[32..40].copy_from_slice(&self.database_revision.to_le_bytes());
        payload[40..48].copy_from_slice(&self.applied_journal_sequence.to_le_bytes());
        payload[48..56].copy_from_slice(&self.bitmap_generation.to_le_bytes());
        payload[56..64]
            .copy_from_slice(&self.catalog_root_page_id.unwrap_or(u64::MAX).to_le_bytes());
        payload[64..68].copy_from_slice(&self.live_data_page_count.to_le_bytes());
        payload[68..70].copy_from_slice(&(BITMAP_CHUNK_COUNT as u16).to_le_bytes());
        payload[70] = self.slot as u8;
        payload[71] = self.bitmap_slot as u8;
        Page::new(self.slot.page_id(), PageType::Superblock, payload)?.encode()
    }

    pub fn decode_page(bytes: &[u8]) -> Result<Self> {
        let page = Page::decode(bytes)?;
        if page.id >= SUPERBLOCK_PAGE_COUNT as PageId {
            return Err(invalid_page(storage_diagnostic!(
                "Superblock must occupy page 0 or 1, not page {}",
                page.id
            )));
        }
        if page.page_type != PageType::Superblock {
            return Err(invalid_page("Superblock page has the wrong page type"));
        }
        if page.payload.len() != SUPERBLOCK_PAYLOAD_SIZE {
            return Err(invalid_page(storage_diagnostic!(
                "Superblock payload must be {SUPERBLOCK_PAYLOAD_SIZE} bytes"
            )));
        }
        let payload = &page.payload;
        if &payload[..8] != SUPERBLOCK_MAGIC {
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
        if payload[72..].iter().any(|byte| *byte != 0) {
            return Err(invalid_page("Superblock reserved bytes must be zero"));
        }
        if read_u16(payload, 68) as usize != BITMAP_CHUNK_COUNT {
            return Err(unsupported_page(
                "Superblock allocation bitmap chunk count is not supported",
            ));
        }
        let slot = SuperblockSlot::try_from(payload[70])?;
        if page.id != slot.page_id() {
            return Err(invalid_page(storage_diagnostic!(
                "Superblock slot {slot:?} must occupy page {}, not page {}",
                slot.page_id(),
                page.id
            )));
        }
        let catalog_root_page_id = match read_u64(payload, 56) {
            u64::MAX => None,
            id => Some(id),
        };
        let superblock = Self {
            slot,
            generation: read_u64(payload, 24),
            database_revision: read_u64(payload, 32),
            applied_journal_sequence: read_u64(payload, 40),
            bitmap_generation: read_u64(payload, 48),
            bitmap_slot: BitmapSlot::try_from(payload[71])?,
            catalog_root_page_id,
            live_data_page_count: read_u32(payload, 64),
            max_page_count,
        };
        superblock.validate()?;
        Ok(superblock)
    }

    fn validate(&self) -> Result<()> {
        crate::revision::validate_database_revision(self.database_revision).map_err(|error| {
            unsupported_page(format!(
                "Superblock database revision is unsupported: {}",
                error.message
            ))
        })?;
        if self.generation == 0 || self.bitmap_generation == 0 {
            return Err(invalid_page(
                "Superblock and allocation bitmap generations must be non-zero",
            ));
        }
        if self.bitmap_generation != self.generation {
            return Err(invalid_page(
                "Allocation bitmap generation must match the superblock generation",
            ));
        }
        let expected_bitmap_slot = match self.slot {
            SuperblockSlot::A => BitmapSlot::A,
            SuperblockSlot::B => BitmapSlot::B,
        };
        if self.bitmap_slot != expected_bitmap_slot {
            return Err(invalid_page(
                "Superblock and allocation bitmap slots must match",
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

    pub fn validate_bitmap(&self, bitmap: &AllocationBitmap) -> Result<()> {
        if bitmap.slot() != self.bitmap_slot || bitmap.generation() != self.bitmap_generation {
            return Err(invalid_page(
                "Superblock allocation bitmap slot or generation does not match",
            ));
        }
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllocationBitmap {
    generation: u64,
    slot: BitmapSlot,
    bits: Vec<u8>,
}

impl AllocationBitmap {
    pub fn new(generation: u64, slot: BitmapSlot) -> Result<Self> {
        if generation == 0 {
            return Err(invalid_page(
                "Allocation bitmap generation must be non-zero",
            ));
        }
        let mut bitmap = Self {
            generation,
            slot,
            bits: vec![0; ALLOCATION_BITMAP_BYTES],
        };
        for id in 0..FIRST_DATA_PAGE_ID {
            bitmap.set_allocated(id, true)?;
        }
        Ok(bitmap)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn slot(&self) -> BitmapSlot {
        self.slot
    }

    pub fn is_allocated(&self, id: PageId) -> Result<bool> {
        validate_page_id(id)?;
        let byte = id as usize / 8;
        let bit = id as usize % 8;
        Ok(self.bits[byte] & (1 << bit) != 0)
    }

    pub fn set_allocated(&mut self, id: PageId, allocated: bool) -> Result<()> {
        validate_page_id(id)?;
        if id < FIRST_DATA_PAGE_ID && !allocated {
            return Err(invalid_page("Metadata pages cannot be deallocated"));
        }
        let byte = id as usize / 8;
        let mask = 1 << (id as usize % 8);
        if allocated {
            self.bits[byte] |= mask;
        } else {
            self.bits[byte] &= !mask;
        }
        Ok(())
    }

    pub fn allocated_page_count(&self) -> u32 {
        self.bits.iter().map(|byte| byte.count_ones()).sum()
    }

    pub fn encode_pages(&self) -> Result<Vec<[u8; PAGE_SIZE]>> {
        if self.generation == 0 {
            return Err(invalid_page(
                "Allocation bitmap generation must be non-zero",
            ));
        }
        self.validate_metadata_pages()?;
        let mut pages = Vec::with_capacity(BITMAP_CHUNK_COUNT);
        for chunk in 0..BITMAP_CHUNK_COUNT {
            let start = chunk * MAX_BITMAP_CHUNK_BYTES;
            let end = (start + MAX_BITMAP_CHUNK_BYTES).min(ALLOCATION_BITMAP_BYTES);
            let bits = &self.bits[start..end];
            let mut payload = Vec::with_capacity(BITMAP_CHUNK_HEADER_SIZE + bits.len());
            payload.extend_from_slice(&self.generation.to_le_bytes());
            payload.push(self.slot as u8);
            payload.push(chunk as u8);
            payload.push(BITMAP_CHUNK_COUNT as u8);
            payload.push(0);
            payload.extend_from_slice(&(bits.len() as u16).to_le_bytes());
            payload.extend_from_slice(&[0, 0]);
            payload.extend_from_slice(bits);
            pages.push(
                Page::new(
                    self.slot.page_id(chunk),
                    PageType::AllocationBitmap,
                    payload,
                )?
                .encode()?,
            );
        }
        Ok(pages)
    }

    pub fn decode_pages(slot: BitmapSlot, pages: &[[u8; PAGE_SIZE]]) -> Result<Self> {
        let pages = pages.iter().map(|page| page.as_slice()).collect::<Vec<_>>();
        Self::decode_page_slices(slot, &pages)
    }

    fn decode_page_slices(slot: BitmapSlot, pages: &[&[u8]]) -> Result<Self> {
        if pages.len() != BITMAP_CHUNK_COUNT {
            return Err(invalid_page(storage_diagnostic!(
                "Allocation bitmap slot must contain {BITMAP_CHUNK_COUNT} chunks, not {}",
                pages.len()
            )));
        }
        let mut generation = None;
        let mut bits = Vec::with_capacity(ALLOCATION_BITMAP_BYTES);
        for (chunk, bytes) in pages.iter().enumerate() {
            let page = Page::decode(bytes)?;
            if page.id != slot.page_id(chunk) {
                return Err(invalid_page(storage_diagnostic!(
                    "Allocation bitmap chunk {chunk} has page ID {}, expected {}",
                    page.id,
                    slot.page_id(chunk)
                )));
            }
            if page.page_type != PageType::AllocationBitmap {
                return Err(invalid_page(storage_diagnostic!(
                    "Allocation bitmap chunk {chunk} has the wrong page type"
                )));
            }
            if page.payload.len() < BITMAP_CHUNK_HEADER_SIZE {
                return Err(invalid_page(storage_diagnostic!(
                    "Allocation bitmap chunk {chunk} header is truncated"
                )));
            }
            let chunk_generation = read_u64(&page.payload, 0);
            if chunk_generation == 0 {
                return Err(invalid_page(
                    "Allocation bitmap generation must be non-zero",
                ));
            }
            if let Some(expected) = generation {
                if chunk_generation != expected {
                    return Err(invalid_page(
                        "Allocation bitmap chunks have different generations",
                    ));
                }
            } else {
                generation = Some(chunk_generation);
            }
            if BitmapSlot::try_from(page.payload[8])? != slot {
                return Err(invalid_page(storage_diagnostic!(
                    "Allocation bitmap chunk {chunk} belongs to a different slot"
                )));
            }
            if page.payload[9] as usize != chunk || page.payload[10] as usize != BITMAP_CHUNK_COUNT
            {
                return Err(invalid_page(storage_diagnostic!(
                    "Allocation bitmap chunk {chunk} has inconsistent chunk metadata"
                )));
            }
            if page.payload[11] != 0 || page.payload[14..16].iter().any(|byte| *byte != 0) {
                return Err(invalid_page(
                    "Allocation bitmap flags and reserved bytes must be zero",
                ));
            }
            let expected_length = bitmap_chunk_length(chunk);
            let length = read_u16(&page.payload, 12) as usize;
            if length != expected_length
                || page.payload.len() != BITMAP_CHUNK_HEADER_SIZE + expected_length
            {
                return Err(invalid_page(storage_diagnostic!(
                    "Allocation bitmap chunk {chunk} has an invalid payload length"
                )));
            }
            bits.extend_from_slice(&page.payload[BITMAP_CHUNK_HEADER_SIZE..]);
        }
        if bits.len() != ALLOCATION_BITMAP_BYTES {
            return Err(invalid_page("Allocation bitmap has the wrong total length"));
        }
        let bitmap = Self {
            generation: generation.expect("the chunk count is non-zero"),
            slot,
            bits,
        };
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

    fn retarget(&self, generation: u64, slot: BitmapSlot) -> Result<Self> {
        if generation == 0 {
            return Err(invalid_page(
                "Allocation bitmap generation must be non-zero",
            ));
        }
        self.validate_metadata_pages()?;
        Ok(Self {
            generation,
            slot,
            bits: self.bits.clone(),
        })
    }

    fn logically_matches(&self, other: &Self) -> bool {
        self.generation == other.generation && self.bits == other.bits
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RawMetadataSlot<'a> {
    pub superblock: Option<&'a [u8]>,
    pub bitmap_chunks: [Option<&'a [u8]>; BITMAP_CHUNK_COUNT],
}

impl<'a> RawMetadataSlot<'a> {
    pub const fn empty() -> Self {
        Self {
            superblock: None,
            bitmap_chunks: [None; BITMAP_CHUNK_COUNT],
        }
    }

    pub const fn new(superblock: &'a [u8], bitmap_chunks: [&'a [u8]; BITMAP_CHUNK_COUNT]) -> Self {
        Self {
            superblock: Some(superblock),
            bitmap_chunks: [
                Some(bitmap_chunks[0]),
                Some(bitmap_chunks[1]),
                Some(bitmap_chunks[2]),
            ],
        }
    }

    fn is_present(&self) -> bool {
        self.superblock.is_some() || self.bitmap_chunks.iter().any(Option::is_some)
    }
}

impl Default for RawMetadataSlot<'_> {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredMetadata {
    pub superblock: Superblock,
    pub allocation_bitmap: AllocationBitmap,
}

impl RecoveredMetadata {
    fn logically_matches(&self, other: &Self) -> bool {
        self.superblock.generation == other.superblock.generation
            && self.superblock.database_revision == other.superblock.database_revision
            && self.superblock.applied_journal_sequence == other.superblock.applied_journal_sequence
            && self.superblock.bitmap_generation == other.superblock.bitmap_generation
            && self.superblock.catalog_root_page_id == other.superblock.catalog_root_page_id
            && self.superblock.live_data_page_count == other.superblock.live_data_page_count
            && self.superblock.max_page_count == other.superblock.max_page_count
            && self
                .allocation_bitmap
                .logically_matches(&other.allocation_bitmap)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingMetadata {
    pub superblock: Superblock,
    pub allocation_bitmap: AllocationBitmap,
}

impl PendingMetadata {
    pub const fn publication_plan(&self) -> [MetadataPublicationStep; BITMAP_CHUNK_COUNT + 4] {
        [
            MetadataPublicationStep::WriteDataPages,
            MetadataPublicationStep::WriteBitmapChunk {
                slot: self.superblock.bitmap_slot,
                chunk: 0,
                page_id: self.superblock.bitmap_slot.page_id(0),
            },
            MetadataPublicationStep::WriteBitmapChunk {
                slot: self.superblock.bitmap_slot,
                chunk: 1,
                page_id: self.superblock.bitmap_slot.page_id(1),
            },
            MetadataPublicationStep::WriteBitmapChunk {
                slot: self.superblock.bitmap_slot,
                chunk: 2,
                page_id: self.superblock.bitmap_slot.page_id(2),
            },
            MetadataPublicationStep::FlushDataAndBitmap,
            MetadataPublicationStep::WriteSuperblock {
                slot: self.superblock.slot,
                page_id: self.superblock.slot.page_id(),
            },
            MetadataPublicationStep::FlushSuperblock,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataPublicationStep {
    WriteDataPages,
    WriteBitmapChunk {
        slot: BitmapSlot,
        chunk: usize,
        page_id: PageId,
    },
    FlushDataAndBitmap,
    WriteSuperblock {
        slot: SuperblockSlot,
        page_id: PageId,
    },
    FlushSuperblock,
}

pub fn recover_metadata(
    slot_a: RawMetadataSlot<'_>,
    slot_b: RawMetadataSlot<'_>,
) -> Result<Option<RecoveredMetadata>> {
    let metadata_present = slot_a.is_present() || slot_b.is_present();
    if !metadata_present {
        return Ok(None);
    }

    let candidate_a = decode_metadata_candidate(SuperblockSlot::A, slot_a);
    let candidate_b = decode_metadata_candidate(SuperblockSlot::B, slot_b);
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
                Ok(Some(candidate_a))
            } else if candidate_a.superblock.generation > candidate_b.superblock.generation {
                Ok(Some(candidate_a))
            } else {
                Ok(Some(candidate_b))
            }
        }
        (Ok(candidate), Err(_)) | (Err(_), Ok(candidate)) => Ok(Some(candidate)),
        (Err(error_a), Err(error_b)) => Err(invalid_page(format!(
            "No valid metadata root remains; slot A: {}: {}; slot B: {}: {}",
            error_a.code, error_a.message, error_b.code, error_b.message
        ))),
    }
}

pub fn build_next_metadata(
    active: &RecoveredMetadata,
    database_revision: u64,
    applied_journal_sequence: u64,
    catalog_root_page_id: Option<PageId>,
    allocation_bitmap: &AllocationBitmap,
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
    if applied_journal_sequence < active.superblock.applied_journal_sequence {
        return Err(invalid_page(
            "Applied journal sequence cannot move backwards during metadata publication",
        ));
    }

    let slot = active.superblock.slot.inactive();
    let bitmap_slot = slot.bitmap_slot();
    let allocation_bitmap = allocation_bitmap.retarget(generation, bitmap_slot)?;
    let live_data_page_count = allocation_bitmap
        .allocated_page_count()
        .checked_sub(FIRST_DATA_PAGE_ID as u32)
        .ok_or_else(|| invalid_page("Allocation bitmap does not reserve every metadata page"))?;
    let superblock = Superblock {
        slot,
        generation,
        database_revision,
        applied_journal_sequence,
        bitmap_slot,
        bitmap_generation: generation,
        catalog_root_page_id,
        live_data_page_count,
        max_page_count: MAX_PAGE_COUNT,
    };
    superblock.validate()?;
    superblock.validate_bitmap(&allocation_bitmap)?;
    Ok(PendingMetadata {
        superblock,
        allocation_bitmap,
    })
}

fn decode_metadata_candidate(
    expected_slot: SuperblockSlot,
    raw: RawMetadataSlot<'_>,
) -> Result<RecoveredMetadata> {
    let superblock_bytes = raw.superblock.ok_or_else(|| {
        invalid_page(storage_diagnostic!(
            "Metadata slot {expected_slot:?} has no superblock"
        ))
    })?;
    let superblock = Superblock::decode_page(superblock_bytes)?;
    if superblock.slot != expected_slot {
        return Err(invalid_page(storage_diagnostic!(
            "Metadata candidate {expected_slot:?} contains superblock {:?}",
            superblock.slot
        )));
    }
    let chunks = raw
        .bitmap_chunks
        .iter()
        .enumerate()
        .map(|(chunk, bytes)| {
            bytes.ok_or_else(|| {
                invalid_page(storage_diagnostic!(
                    "Metadata slot {expected_slot:?} has no bitmap chunk {chunk}"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let allocation_bitmap = AllocationBitmap::decode_page_slices(superblock.bitmap_slot, &chunks)?;
    superblock.validate_bitmap(&allocation_bitmap)?;
    Ok(RecoveredMetadata {
        superblock,
        allocation_bitmap,
    })
}

fn bitmap_chunk_length(chunk: usize) -> usize {
    let start = chunk * MAX_BITMAP_CHUNK_BYTES;
    (ALLOCATION_BITMAP_BYTES - start).min(MAX_BITMAP_CHUNK_BYTES)
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

    struct EncodedMetadata {
        superblock: [u8; PAGE_SIZE],
        bitmap_chunks: Vec<[u8; PAGE_SIZE]>,
    }

    impl EncodedMetadata {
        fn raw(&self) -> RawMetadataSlot<'_> {
            RawMetadataSlot::new(
                &self.superblock,
                [
                    &self.bitmap_chunks[0],
                    &self.bitmap_chunks[1],
                    &self.bitmap_chunks[2],
                ],
            )
        }
    }

    fn encoded_metadata(
        slot: SuperblockSlot,
        generation: u64,
        database_revision: u64,
        applied_journal_sequence: u64,
        extra_page: bool,
    ) -> EncodedMetadata {
        let mut allocation_bitmap = AllocationBitmap::new(generation, slot.bitmap_slot()).unwrap();
        allocation_bitmap
            .set_allocated(FIRST_DATA_PAGE_ID, true)
            .unwrap();
        if extra_page {
            allocation_bitmap
                .set_allocated(FIRST_DATA_PAGE_ID + 1, true)
                .unwrap();
        }
        let superblock = Superblock {
            slot,
            generation,
            database_revision,
            applied_journal_sequence,
            bitmap_slot: slot.bitmap_slot(),
            bitmap_generation: generation,
            catalog_root_page_id: Some(FIRST_DATA_PAGE_ID),
            live_data_page_count: if extra_page { 2 } else { 1 },
            max_page_count: MAX_PAGE_COUNT,
        };
        superblock.validate_bitmap(&allocation_bitmap).unwrap();
        EncodedMetadata {
            superblock: superblock.encode_page().unwrap(),
            bitmap_chunks: allocation_bitmap.encode_pages().unwrap(),
        }
    }

    fn rewrite_crc(bytes: &mut [u8; PAGE_SIZE]) {
        bytes[PAGE_CRC_OFFSET..PAGE_CRC_OFFSET + 4].fill(0);
        let checksum = crc32(bytes);
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
    fn superblock_round_trips_and_rejects_invalid_metadata() {
        for slot in [SuperblockSlot::A, SuperblockSlot::B] {
            let mut expected = Superblock::new(slot);
            expected.generation = 8;
            expected.database_revision = 31;
            expected.applied_journal_sequence = 29;
            expected.bitmap_generation = 8;
            expected.catalog_root_page_id = Some(FIRST_DATA_PAGE_ID + 9);
            expected.live_data_page_count = 17;
            let page = expected.encode_page().unwrap();
            assert_eq!(Superblock::decode_page(&page).unwrap(), expected);
        }

        let expected = Superblock::new(SuperblockSlot::A);
        let page = expected.encode_page().unwrap();
        for (offset, value, code) in [
            (8, 2, "UNSUPPORTED_PAGE"),
            (10, 1, "UNSUPPORTED_PAGE"),
            (12, 1, "UNSUPPORTED_PAGE"),
            (16, 1, "UNSUPPORTED_PAGE"),
            (24, 0, "INVALID_PAGE"),
            (48, 2, "INVALID_PAGE"),
            (68, 2, "UNSUPPORTED_PAGE"),
            (70, 2, "INVALID_PAGE"),
            (71, 1, "INVALID_PAGE"),
            (72, 1, "INVALID_PAGE"),
        ] {
            let mut corrupted = page;
            mutate_payload(&mut corrupted, offset, value);
            assert_eq!(Superblock::decode_page(&corrupted).unwrap_err().code, code);
        }

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

        let mut maximum = Superblock::new(SuperblockSlot::A);
        maximum.database_revision = crate::revision::MAX_DATABASE_REVISION;
        let page = maximum.encode_page().unwrap();
        assert_eq!(
            Superblock::decode_page(&page).unwrap().database_revision,
            crate::revision::MAX_DATABASE_REVISION
        );
        maximum.database_revision = crate::revision::MAX_DATABASE_REVISION + 1;
        assert_eq!(maximum.encode_page().unwrap_err().code, "UNSUPPORTED_PAGE");

        let mut short_payload = page;
        short_payload[24..28].copy_from_slice(&95_u32.to_le_bytes());
        short_payload[PAGE_HEADER_SIZE + 95] = 0;
        rewrite_crc(&mut short_payload);
        assert_eq!(
            Superblock::decode_page(&short_payload).unwrap_err().code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn superblock_validates_its_paired_bitmap_generation_slot_and_live_count() {
        let mut superblock = Superblock::new(SuperblockSlot::B);
        superblock.generation = 11;
        superblock.bitmap_generation = 11;
        superblock.live_data_page_count = 1;
        superblock.catalog_root_page_id = Some(FIRST_DATA_PAGE_ID);
        let mut bitmap = AllocationBitmap::new(11, BitmapSlot::B).unwrap();
        bitmap.set_allocated(FIRST_DATA_PAGE_ID, true).unwrap();
        superblock.validate_bitmap(&bitmap).unwrap();

        let wrong_generation = AllocationBitmap::new(10, BitmapSlot::B).unwrap();
        assert_eq!(
            superblock
                .validate_bitmap(&wrong_generation)
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
        let wrong_slot = AllocationBitmap::new(11, BitmapSlot::A).unwrap();
        assert_eq!(
            superblock.validate_bitmap(&wrong_slot).unwrap_err().code,
            "INVALID_PAGE"
        );
        bitmap.set_allocated(FIRST_DATA_PAGE_ID + 1, true).unwrap();
        assert_eq!(
            superblock.validate_bitmap(&bitmap).unwrap_err().code,
            "INVALID_PAGE"
        );

        let mut missing_root = AllocationBitmap::new(11, BitmapSlot::B).unwrap();
        missing_root
            .set_allocated(FIRST_DATA_PAGE_ID + 1, true)
            .unwrap();
        assert_eq!(
            superblock.validate_bitmap(&missing_root).unwrap_err().code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn paired_allocation_bitmaps_round_trip_generation_slot_and_chunks() {
        for slot in [BitmapSlot::A, BitmapSlot::B] {
            let mut bitmap = AllocationBitmap::new(17, slot).unwrap();
            bitmap.set_allocated(FIRST_DATA_PAGE_ID, true).unwrap();
            bitmap.set_allocated(MAX_PAGE_COUNT - 1, true).unwrap();
            let pages = bitmap.encode_pages().unwrap();
            assert_eq!(pages.len(), BITMAP_CHUNK_COUNT);
            assert_eq!(
                AllocationBitmap::decode_pages(slot, &pages).unwrap(),
                bitmap
            );
            assert_eq!(bitmap.allocated_page_count(), FIRST_DATA_PAGE_ID as u32 + 2);
        }
    }

    #[test]
    fn allocation_bitmap_rejects_generation_slot_and_chunk_corruption() {
        let bitmap = AllocationBitmap::new(17, BitmapSlot::A).unwrap();
        let pages = bitmap.encode_pages().unwrap();
        assert_eq!(
            AllocationBitmap::decode_pages(BitmapSlot::A, &pages[..2])
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );

        for (chunk, offset, value) in [
            (0, 0, 0),
            (0, 8, 1),
            (1, 0, 18),
            (1, 9, 0),
            (1, 10, 2),
            (1, 11, 1),
            (1, 12, 1),
            (1, 14, 1),
        ] {
            let mut corrupted = pages.clone();
            mutate_payload(&mut corrupted[chunk], offset, value);
            assert_eq!(
                AllocationBitmap::decode_pages(BitmapSlot::A, &corrupted)
                    .unwrap_err()
                    .code,
                "INVALID_PAGE"
            );
        }

        let mut wrong_id = pages.clone();
        wrong_id[0][12..20].copy_from_slice(&3_u64.to_le_bytes());
        rewrite_crc(&mut wrong_id[0]);
        assert_eq!(
            AllocationBitmap::decode_pages(BitmapSlot::A, &wrong_id)
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );

        let mut wrong_type = pages.clone();
        wrong_type[0][20] = PageType::BtreeLeaf as u8;
        rewrite_crc(&mut wrong_type[0]);
        assert_eq!(
            AllocationBitmap::decode_pages(BitmapSlot::A, &wrong_type)
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );

        let mut freed_metadata = pages;
        freed_metadata[0][PAGE_HEADER_SIZE + BITMAP_CHUNK_HEADER_SIZE] &= !1;
        rewrite_crc(&mut freed_metadata[0]);
        assert_eq!(
            AllocationBitmap::decode_pages(BitmapSlot::A, &freed_metadata)
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn allocation_bitmap_bounds_page_ids() {
        let mut bitmap = AllocationBitmap::new(1, BitmapSlot::A).unwrap();
        assert!(bitmap.is_allocated(0).unwrap());
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
        let older = encoded_metadata(SuperblockSlot::A, 8, 20, 19, false);
        let newer = encoded_metadata(SuperblockSlot::B, 9, 21, 20, false);
        let recovered = recover_metadata(older.raw(), newer.raw()).unwrap().unwrap();
        assert_eq!(recovered.superblock.slot, SuperblockSlot::B);
        assert_eq!(recovered.superblock.generation, 9);
        assert_eq!(recovered.superblock.database_revision, 21);
        assert_eq!(recovered.superblock.applied_journal_sequence, 20);
    }

    #[test]
    fn metadata_recovery_falls_back_from_each_torn_or_corrupt_newer_component() {
        let older = encoded_metadata(SuperblockSlot::A, 8, 20, 19, false);
        let newer = encoded_metadata(SuperblockSlot::B, 9, 21, 20, false);

        for length in 0..PAGE_HEADER_SIZE {
            let torn = RawMetadataSlot {
                superblock: Some(&newer.superblock[..length]),
                bitmap_chunks: [
                    Some(&newer.bitmap_chunks[0]),
                    Some(&newer.bitmap_chunks[1]),
                    Some(&newer.bitmap_chunks[2]),
                ],
            };
            assert_eq!(
                recover_metadata(older.raw(), torn)
                    .unwrap()
                    .unwrap()
                    .superblock
                    .slot,
                SuperblockSlot::A
            );
        }
        for torn_chunk in 0..BITMAP_CHUNK_COUNT {
            for length in 0..PAGE_HEADER_SIZE + BITMAP_CHUNK_HEADER_SIZE {
                let mut chunks: [Option<&[u8]>; BITMAP_CHUNK_COUNT] = [
                    Some(&newer.bitmap_chunks[0]),
                    Some(&newer.bitmap_chunks[1]),
                    Some(&newer.bitmap_chunks[2]),
                ];
                chunks[torn_chunk] = Some(&newer.bitmap_chunks[torn_chunk][..length]);
                assert_eq!(
                    recover_metadata(
                        older.raw(),
                        RawMetadataSlot {
                            superblock: Some(&newer.superblock),
                            bitmap_chunks: chunks,
                        },
                    )
                    .unwrap()
                    .unwrap()
                    .superblock
                    .slot,
                    SuperblockSlot::A
                );
            }
        }

        for component in 0..=BITMAP_CHUNK_COUNT {
            let mut corrupted_superblock = newer.superblock;
            let mut corrupted_chunks = newer.bitmap_chunks.clone();
            if component == 0 {
                corrupted_superblock[PAGE_SIZE - 1] ^= 1;
            } else {
                corrupted_chunks[component - 1][PAGE_SIZE - 1] ^= 1;
            }
            let corrupted = RawMetadataSlot::new(
                &corrupted_superblock,
                [
                    &corrupted_chunks[0],
                    &corrupted_chunks[1],
                    &corrupted_chunks[2],
                ],
            );
            assert_eq!(
                recover_metadata(older.raw(), corrupted)
                    .unwrap()
                    .unwrap()
                    .superblock
                    .slot,
                SuperblockSlot::A
            );
        }
    }

    #[test]
    fn metadata_recovery_rejects_present_metadata_when_neither_root_is_valid() {
        assert!(
            recover_metadata(RawMetadataSlot::empty(), RawMetadataSlot::empty())
                .unwrap()
                .is_none()
        );

        let partial = RawMetadataSlot {
            superblock: Some(&[0; PAGE_HEADER_SIZE]),
            bitmap_chunks: [None; BITMAP_CHUNK_COUNT],
        };
        let error = recover_metadata(partial, RawMetadataSlot::empty()).unwrap_err();
        assert_eq!(error.code, "INVALID_PAGE");
        assert!(error.message.contains("No valid metadata root remains"));
    }

    #[test]
    fn metadata_recovery_falls_back_from_a_mixed_bitmap_generation() {
        let older = encoded_metadata(SuperblockSlot::A, 8, 20, 19, false);
        let newer = encoded_metadata(SuperblockSlot::B, 9, 21, 20, false);
        let stale_bitmap = encoded_metadata(SuperblockSlot::B, 8, 20, 19, false);
        let mixed = RawMetadataSlot::new(
            &newer.superblock,
            [
                &stale_bitmap.bitmap_chunks[0],
                &stale_bitmap.bitmap_chunks[1],
                &stale_bitmap.bitmap_chunks[2],
            ],
        );
        let recovered = recover_metadata(older.raw(), mixed).unwrap().unwrap();
        assert_eq!(recovered.superblock.slot, SuperblockSlot::A);
        assert_eq!(recovered.superblock.generation, 8);
    }

    #[test]
    fn equal_generation_roots_must_be_logically_identical() {
        let slot_a = encoded_metadata(SuperblockSlot::A, 12, 30, 29, false);
        let slot_b = encoded_metadata(SuperblockSlot::B, 12, 30, 29, false);
        let recovered = recover_metadata(slot_a.raw(), slot_b.raw())
            .unwrap()
            .unwrap();
        assert_eq!(recovered.superblock.generation, 12);

        let different_root = encoded_metadata(SuperblockSlot::B, 12, 31, 29, false);
        assert_eq!(
            recover_metadata(slot_a.raw(), different_root.raw())
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
        let different_bitmap = encoded_metadata(SuperblockSlot::B, 12, 30, 29, true);
        assert_eq!(
            recover_metadata(slot_a.raw(), different_bitmap.raw())
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn unsupported_metadata_fails_closed_even_with_an_older_valid_root() {
        let older = encoded_metadata(SuperblockSlot::A, 8, 20, 19, false);
        let newer = encoded_metadata(SuperblockSlot::B, 9, 21, 20, false);
        for component in 0..=BITMAP_CHUNK_COUNT {
            let mut unsupported_superblock = newer.superblock;
            let mut unsupported_chunks = newer.bitmap_chunks.clone();
            if component == 0 {
                unsupported_superblock[8..10].copy_from_slice(&2_u16.to_le_bytes());
                rewrite_crc(&mut unsupported_superblock);
            } else {
                unsupported_chunks[component - 1][8..10].copy_from_slice(&2_u16.to_le_bytes());
                rewrite_crc(&mut unsupported_chunks[component - 1]);
            }
            let unsupported = RawMetadataSlot::new(
                &unsupported_superblock,
                [
                    &unsupported_chunks[0],
                    &unsupported_chunks[1],
                    &unsupported_chunks[2],
                ],
            );
            assert_eq!(
                recover_metadata(older.raw(), unsupported).unwrap_err().code,
                "UNSUPPORTED_PAGE"
            );
        }

        let mut unsupported_superblock_payload = newer.superblock;
        mutate_payload(&mut unsupported_superblock_payload, 8, 2);
        let unsupported = RawMetadataSlot::new(
            &unsupported_superblock_payload,
            [
                &newer.bitmap_chunks[0],
                &newer.bitmap_chunks[1],
                &newer.bitmap_chunks[2],
            ],
        );
        assert_eq!(
            recover_metadata(older.raw(), unsupported).unwrap_err().code,
            "UNSUPPORTED_PAGE"
        );

        let older = encoded_metadata(
            SuperblockSlot::A,
            8,
            crate::revision::MAX_DATABASE_REVISION,
            19,
            false,
        );
        let newer = encoded_metadata(
            SuperblockSlot::B,
            9,
            crate::revision::MAX_DATABASE_REVISION,
            20,
            false,
        );
        let mut unsupported_revision = newer.superblock;
        unsupported_revision[PAGE_HEADER_SIZE + 32..PAGE_HEADER_SIZE + 40]
            .copy_from_slice(&(crate::revision::MAX_DATABASE_REVISION + 1).to_le_bytes());
        rewrite_crc(&mut unsupported_revision);
        let unsupported = RawMetadataSlot::new(
            &unsupported_revision,
            [
                &newer.bitmap_chunks[0],
                &newer.bitmap_chunks[1],
                &newer.bitmap_chunks[2],
            ],
        );
        assert_eq!(
            recover_metadata(older.raw(), unsupported).unwrap_err().code,
            "UNSUPPORTED_PAGE"
        );
    }

    #[test]
    fn next_metadata_targets_the_inactive_pair_and_has_a_fixed_publication_order() {
        let active_bytes = encoded_metadata(SuperblockSlot::A, 8, 20, 19, false);
        let active = recover_metadata(active_bytes.raw(), RawMetadataSlot::empty())
            .unwrap()
            .unwrap();
        let mut allocation_bitmap = active.allocation_bitmap.clone();
        allocation_bitmap
            .set_allocated(FIRST_DATA_PAGE_ID + 1, true)
            .unwrap();
        let pending = build_next_metadata(
            &active,
            21,
            20,
            Some(FIRST_DATA_PAGE_ID + 1),
            &allocation_bitmap,
        )
        .unwrap();
        assert_eq!(pending.superblock.slot, SuperblockSlot::B);
        assert_eq!(pending.superblock.bitmap_slot, BitmapSlot::B);
        assert_eq!(pending.superblock.generation, 9);
        assert_eq!(pending.allocation_bitmap.generation(), 9);
        assert_eq!(pending.allocation_bitmap.slot(), BitmapSlot::B);
        assert_eq!(pending.superblock.live_data_page_count, 2);
        assert_eq!(
            pending.publication_plan(),
            [
                MetadataPublicationStep::WriteDataPages,
                MetadataPublicationStep::WriteBitmapChunk {
                    slot: BitmapSlot::B,
                    chunk: 0,
                    page_id: 5,
                },
                MetadataPublicationStep::WriteBitmapChunk {
                    slot: BitmapSlot::B,
                    chunk: 1,
                    page_id: 6,
                },
                MetadataPublicationStep::WriteBitmapChunk {
                    slot: BitmapSlot::B,
                    chunk: 2,
                    page_id: 7,
                },
                MetadataPublicationStep::FlushDataAndBitmap,
                MetadataPublicationStep::WriteSuperblock {
                    slot: SuperblockSlot::B,
                    page_id: 1,
                },
                MetadataPublicationStep::FlushSuperblock,
            ]
        );
    }

    #[test]
    fn next_metadata_rejects_generation_wrap_and_backwards_progress() {
        let active_bytes = encoded_metadata(SuperblockSlot::A, 8, 20, 19, false);
        let active = recover_metadata(active_bytes.raw(), RawMetadataSlot::empty())
            .unwrap()
            .unwrap();
        assert_eq!(
            build_next_metadata(
                &active,
                19,
                20,
                Some(FIRST_DATA_PAGE_ID),
                &active.allocation_bitmap,
            )
            .unwrap_err()
            .code,
            "INVALID_PAGE"
        );
        assert_eq!(
            build_next_metadata(
                &active,
                21,
                18,
                Some(FIRST_DATA_PAGE_ID),
                &active.allocation_bitmap,
            )
            .unwrap_err()
            .code,
            "INVALID_PAGE"
        );

        let exhausted_bytes = encoded_metadata(SuperblockSlot::A, u64::MAX, 20, 19, false);
        let exhausted = recover_metadata(exhausted_bytes.raw(), RawMetadataSlot::empty())
            .unwrap()
            .unwrap();
        assert_eq!(
            build_next_metadata(
                &exhausted,
                21,
                20,
                Some(FIRST_DATA_PAGE_ID),
                &exhausted.allocation_bitmap,
            )
            .unwrap_err()
            .code,
            "PAGE_GENERATION_EXHAUSTED"
        );
    }
}
