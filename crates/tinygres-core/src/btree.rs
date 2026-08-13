use std::collections::HashSet;

use crate::{
    EngineError, FIRST_DATA_PAGE_ID, MAX_PAGE_COUNT, MAX_PAGE_PAYLOAD_SIZE, Page, PageDevice,
    PageId, PageType, Pager, PagerWriteTransaction, Result, snapshot::crc32,
};

pub type TreeId = u64;

pub const MAX_BTREE_KEY_BYTES: usize = 1_024;
pub const MAX_BTREE_INLINE_VALUE_BYTES: usize = 1_024;
pub const MAX_BTREE_INLINE_ENTRY_BYTES: usize = 1_536;
pub const MAX_BTREE_VALUE_BYTES: usize = 1024 * 1024;

const NODE_MAGIC: &[u8; 4] = b"TGBT";
const NODE_FORMAT_VERSION: u16 = 1;
const NODE_FLAGS: u8 = 0;
const NODE_HEADER_SIZE: usize = 40;
const SLOT_SIZE: usize = 2;
const LEAF_CELL_HEADER_SIZE: usize = 8;
const OVERFLOW_DESCRIPTOR_SIZE: usize = 24;
const INTERNAL_CELL_HEADER_SIZE: usize = 12;
const INLINE_CELL_FLAGS: u16 = 0;
const OVERFLOW_CELL_FLAGS: u16 = 1;
const NO_PAGE_ID: PageId = u64::MAX;
const MAX_TREE_DEPTH: usize = 64;

const OVERFLOW_MAGIC: &[u8; 4] = b"TGOV";
const OVERFLOW_FORMAT_VERSION: u16 = 1;
const OVERFLOW_FLAGS: u16 = 0;
const OVERFLOW_HEADER_SIZE: usize = 56;
const MAX_OVERFLOW_CHUNK_BYTES: usize = MAX_PAGE_PAYLOAD_SIZE - OVERFLOW_HEADER_SIZE;
const MAX_OVERFLOW_PAGE_COUNT: usize = MAX_BTREE_VALUE_BYTES.div_ceil(MAX_OVERFLOW_CHUNK_BYTES);

/// A versioned, lexicographically ordered B-tree stored in pager data pages.
///
/// Keys and values are opaque bytes. SQL-aware sortable encodings belong to the storage layer
/// above this primitive. Values up to one MiB are stored inline or in validated overflow chains.
#[derive(Clone, Copy, Debug, Default)]
pub struct Btree;

impl Btree {
    /// Creates an empty leaf and returns its candidate root page ID.
    pub fn create<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        tree_id: TreeId,
    ) -> Result<PageId> {
        validate_tree_id(tree_id)?;
        let result = (|| {
            let generation = transaction.generation()?;
            let root = transaction.allocate_page()?;
            write_node(
                transaction,
                root,
                &Node::leaf(tree_id, generation, Vec::new()),
            )?;
            Ok(root)
        })();
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }

    /// Looks up one exact key in a committed root.
    pub fn get<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        validate_tree_id(tree_id)?;
        validate_key(key)?;
        let generation = pager.generation();
        let mut page_id = root_page_id;
        let mut visited = HashSet::new();
        let mut expected_level = None;
        let mut parent_generation = None;

        for _ in 0..MAX_TREE_DEPTH {
            if !visited.insert(page_id) {
                return Err(invalid_btree(format!(
                    "Tree {tree_id} contains a cycle through page {page_id}"
                )));
            }
            let page = pager.read_page(page_id)?;
            let node = Node::decode(page, tree_id, generation, false)?;
            validate_expected_level(page_id, node.level, expected_level)?;
            validate_child_generation(page_id, node.generation, parent_generation)?;
            let node_generation = node.generation;
            match node.kind {
                NodeKind::Leaf(entries) => {
                    return match entries.binary_search_by(|entry| entry.key.as_slice().cmp(key)) {
                        Ok(index) => materialize_committed_value(
                            pager,
                            tree_id,
                            generation,
                            node_generation,
                            &entries[index].value,
                        )
                        .map(Some),
                        Err(_) => Ok(None),
                    };
                }
                NodeKind::Internal(internal) => {
                    page_id = internal.child_for(key);
                    expected_level = Some(node.level - 1);
                    parent_generation = Some(node.generation);
                }
            }
        }
        Err(invalid_btree(format!(
            "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
        )))
    }

    /// Inserts or replaces an inline value and returns the candidate root page ID.
    ///
    /// On error, the pager transaction is marked failed and must be aborted. Allocation or device
    /// failures may have left unreachable candidate pages which must not be published as part of
    /// another operation.
    pub fn upsert<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
        key: &[u8],
        value: &[u8],
    ) -> Result<PageId> {
        validate_tree_id(tree_id)?;
        validate_key(key)?;
        validate_value(value)?;
        let result = (|| {
            let generation = transaction.generation()?;
            let mut visited = HashSet::new();
            let inserted = insert_recursive(
                transaction,
                root_page_id,
                tree_id,
                generation,
                key,
                value,
                &mut visited,
                0,
                None,
                None,
            )?;
            if let Some(split) = inserted.split {
                let level = split
                    .left_level
                    .checked_add(1)
                    .ok_or_else(|| limit_error("The B-tree level cannot be represented"))?;
                if level as usize >= MAX_TREE_DEPTH {
                    return Err(limit_error(format!(
                        "The B-tree exceeds the maximum depth of {MAX_TREE_DEPTH}"
                    )));
                }
                let new_root = transaction.allocate_page()?;
                let root = Node::internal(
                    tree_id,
                    generation,
                    level,
                    inserted.page_id,
                    vec![InternalEntry {
                        key: split.separator,
                        right_child: split.right_page_id,
                    }],
                );
                write_node(transaction, new_root, &root)?;
                Ok(new_root)
            } else {
                Ok(inserted.page_id)
            }
        })();
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }

    /// Removes one exact key and returns the candidate root and whether an entry existed.
    ///
    /// Deletion is copy-on-write and deliberately does not rebalance under-full pages. Empty
    /// descendants are pruned; an internal page may retain one child and no separator, and an
    /// empty tree is represented by `None`. Separator keys are advanced when the first key in a
    /// right subtree changes. On error, the pager transaction is marked failed and must be aborted.
    pub fn delete<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
        key: &[u8],
    ) -> Result<(Option<PageId>, bool)> {
        validate_tree_id(tree_id)?;
        validate_key(key)?;
        let result = (|| {
            let generation = transaction.generation()?;
            let mut visited = HashSet::new();
            let deleted = delete_recursive(
                transaction,
                root_page_id,
                tree_id,
                generation,
                key,
                &mut visited,
                0,
                None,
                None,
            )?;
            Ok((deleted.page_id, deleted.removed))
        })();
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }

    /// Opens a detached cursor at the first key in a committed tree.
    pub fn cursor<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
    ) -> Result<BtreeCursor> {
        Self::cursor_from(pager, root_page_id, tree_id, &[])
    }

    /// Opens a detached cursor at the first key greater than or equal to `lower_bound`.
    ///
    /// The returned cursor owns bounded page IDs, traversal indexes, and one decoded leaf. It does
    /// not borrow the pager, so callers can release interior-mutability guards before invoking
    /// application visitors.
    pub fn cursor_from<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
        lower_bound: &[u8],
    ) -> Result<BtreeCursor> {
        validate_tree_id(tree_id)?;
        validate_key(lower_bound)?;
        let generation = pager.generation();
        let mut cursor = BtreeCursor {
            tree_id,
            root_page_id,
            generation,
            path: Vec::new(),
            leaf_page_id: root_page_id,
            leaf_index: 0,
            leaf_generation: generation,
            leaf_entries: Vec::new(),
            finished: false,
            visited_pages: HashSet::new(),
        };
        cursor.seek(pager, lower_bound)?;
        Ok(cursor)
    }
}

/// Resumable forward cursor state which does not hold a pager borrow.
#[derive(Clone, Debug)]
pub struct BtreeCursor {
    tree_id: TreeId,
    root_page_id: PageId,
    generation: u64,
    path: Vec<CursorFrame>,
    leaf_page_id: PageId,
    leaf_index: usize,
    leaf_generation: u64,
    leaf_entries: Vec<LeafEntry>,
    finished: bool,
    visited_pages: HashSet<PageId>,
}

impl BtreeCursor {
    pub fn tree_id(&self) -> TreeId {
        self.tree_id
    }

    pub fn root_page_id(&self) -> PageId {
        self.root_page_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the next owned key/value pair, or `None` after the final leaf.
    pub fn next<D: PageDevice>(
        &mut self,
        pager: &mut Pager<D>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if self.finished {
            return Ok(None);
        }
        self.ensure_generation(pager)?;

        loop {
            if let Some(entry) = self.leaf_entries.get(self.leaf_index) {
                let value = materialize_committed_value(
                    pager,
                    self.tree_id,
                    self.generation,
                    self.leaf_generation,
                    &entry.value,
                )?;
                self.leaf_index += 1;
                return Ok(Some((entry.key.clone(), value)));
            }
            if !self.advance_leaf(pager)? {
                self.finished = true;
                return Ok(None);
            }
        }
    }

    fn seek<D: PageDevice>(&mut self, pager: &mut Pager<D>, lower_bound: &[u8]) -> Result<()> {
        self.ensure_generation(pager)?;
        self.path.clear();
        self.visited_pages.clear();
        let mut page_id = self.root_page_id;
        let mut expected_level = None;
        let mut parent_generation = None;
        for _ in 0..MAX_TREE_DEPTH {
            if !self.visited_pages.insert(page_id) {
                return Err(invalid_btree(format!(
                    "Tree {} contains a cycle through page {page_id}",
                    self.tree_id
                )));
            }
            let node = Node::decode(
                pager.read_page(page_id)?,
                self.tree_id,
                self.generation,
                false,
            )?;
            validate_expected_level(page_id, node.level, expected_level)?;
            validate_child_generation(page_id, node.generation, parent_generation)?;
            match node.kind {
                NodeKind::Leaf(entries) => {
                    self.leaf_page_id = page_id;
                    self.leaf_index =
                        entries.partition_point(|entry| entry.key.as_slice() < lower_bound);
                    self.leaf_generation = node.generation;
                    self.leaf_entries = entries;
                    return Ok(());
                }
                NodeKind::Internal(internal) => {
                    let child_index = internal.child_index_for(lower_bound);
                    let child = internal.child(child_index)?;
                    self.path.push(CursorFrame {
                        page_id,
                        child_index,
                        level: node.level,
                        generation: node.generation,
                    });
                    page_id = child;
                    expected_level = Some(node.level - 1);
                    parent_generation = Some(node.generation);
                }
            }
        }
        Err(invalid_btree(format!(
            "Tree {} exceeds the maximum depth of {MAX_TREE_DEPTH}",
            self.tree_id
        )))
    }

    fn advance_leaf<D: PageDevice>(&mut self, pager: &mut Pager<D>) -> Result<bool> {
        while let Some(mut frame) = self.path.pop() {
            let node = Node::decode(
                pager.read_page(frame.page_id)?,
                self.tree_id,
                self.generation,
                false,
            )?;
            let NodeKind::Internal(internal) = node.kind else {
                return Err(invalid_btree(format!(
                    "Cursor path page {} is a leaf",
                    frame.page_id
                )));
            };
            validate_expected_level(frame.page_id, node.level, Some(frame.level))?;
            if node.generation != frame.generation {
                return Err(invalid_btree(format!(
                    "Cursor path page {} changed generation from {} to {}",
                    frame.page_id, frame.generation, node.generation
                )));
            }
            if frame.child_index < internal.entries.len() {
                frame.child_index += 1;
                let next_child = internal.child(frame.child_index)?;
                let expected_level = node.level - 1;
                let parent_generation = node.generation;
                self.path.push(frame);
                return self.descend_leftmost(pager, next_child, expected_level, parent_generation);
            }
        }
        Ok(false)
    }

    fn descend_leftmost<D: PageDevice>(
        &mut self,
        pager: &mut Pager<D>,
        mut page_id: PageId,
        mut expected_level: u8,
        mut parent_generation: u64,
    ) -> Result<bool> {
        for _ in self.path.len()..MAX_TREE_DEPTH {
            if !self.visited_pages.insert(page_id) {
                return Err(invalid_btree(format!(
                    "Tree {} contains a cycle through page {page_id}",
                    self.tree_id
                )));
            }
            let node = Node::decode(
                pager.read_page(page_id)?,
                self.tree_id,
                self.generation,
                false,
            )?;
            validate_expected_level(page_id, node.level, Some(expected_level))?;
            validate_child_generation(page_id, node.generation, Some(parent_generation))?;
            match node.kind {
                NodeKind::Leaf(entries) => {
                    self.leaf_page_id = page_id;
                    self.leaf_index = 0;
                    self.leaf_generation = node.generation;
                    self.leaf_entries = entries;
                    return Ok(true);
                }
                NodeKind::Internal(internal) => {
                    let child = internal.leftmost_child;
                    self.path.push(CursorFrame {
                        page_id,
                        child_index: 0,
                        level: node.level,
                        generation: node.generation,
                    });
                    page_id = child;
                    expected_level = node.level - 1;
                    parent_generation = node.generation;
                }
            }
        }
        Err(invalid_btree(format!(
            "Tree {} exceeds the maximum depth of {MAX_TREE_DEPTH}",
            self.tree_id
        )))
    }

    fn ensure_generation<D: PageDevice>(&self, pager: &Pager<D>) -> Result<()> {
        if pager.generation() != self.generation {
            return Err(EngineError::new(
                "CURSOR_INVALIDATED",
                format!(
                    "The cursor was opened at page generation {}, but the pager is now at generation {}",
                    self.generation,
                    pager.generation()
                ),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct CursorFrame {
    page_id: PageId,
    child_index: usize,
    level: u8,
    generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Node {
    tree_id: TreeId,
    generation: u64,
    level: u8,
    kind: NodeKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NodeKind {
    Leaf(Vec<LeafEntry>),
    Internal(InternalNode),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LeafEntry {
    key: Vec<u8>,
    value: LeafValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LeafValue {
    Inline(Vec<u8>),
    Overflow(OverflowDescriptor),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OverflowDescriptor {
    first_page_id: PageId,
    generation: u64,
    total_length: u32,
    checksum: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OverflowPage {
    tree_id: TreeId,
    generation: u64,
    chunk_index: u32,
    chunk_count: u32,
    next_page_id: Option<PageId>,
    total_length: u32,
    checksum: u32,
    chunk: Vec<u8>,
}

impl LeafValue {
    fn flags(&self) -> u16 {
        match self {
            Self::Inline(_) => INLINE_CELL_FLAGS,
            Self::Overflow(_) => OVERFLOW_CELL_FLAGS,
        }
    }

    fn encoded_len(&self) -> usize {
        match self {
            Self::Inline(value) => value.len(),
            Self::Overflow(_) => OVERFLOW_DESCRIPTOR_SIZE,
        }
    }

    fn encode_into(&self, destination: &mut Vec<u8>) {
        match self {
            Self::Inline(value) => destination.extend_from_slice(value),
            Self::Overflow(descriptor) => {
                destination.extend_from_slice(&descriptor.first_page_id.to_le_bytes());
                destination.extend_from_slice(&descriptor.generation.to_le_bytes());
                destination.extend_from_slice(&descriptor.total_length.to_le_bytes());
                destination.extend_from_slice(&descriptor.checksum.to_le_bytes());
            }
        }
    }
}

impl OverflowPage {
    fn encode(&self, page_id: PageId) -> Result<Page> {
        validate_overflow_page_id(page_id)?;
        validate_tree_id(self.tree_id)?;
        if self.generation == 0 {
            return Err(invalid_overflow("Overflow generation must be positive"));
        }
        if self.chunk_count == 0
            || self.chunk_count as usize > MAX_OVERFLOW_PAGE_COUNT
            || self.chunk_index >= self.chunk_count
        {
            return Err(invalid_overflow(format!(
                "Overflow chunk index {} is outside chunk count {}",
                self.chunk_index, self.chunk_count
            )));
        }
        if self.total_length == 0 || self.total_length as usize > MAX_BTREE_VALUE_BYTES {
            return Err(invalid_overflow(format!(
                "Overflow total length {} is outside 1..={MAX_BTREE_VALUE_BYTES}",
                self.total_length
            )));
        }
        let expected_chunk_count = (self.total_length as usize).div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
        if self.chunk_count as usize != expected_chunk_count {
            return Err(invalid_overflow(format!(
                "Overflow chunk count {} does not match total length {}",
                self.chunk_count, self.total_length
            )));
        }
        let expected_chunk_length = overflow_chunk_length(
            self.total_length as usize,
            self.chunk_index as usize,
            self.chunk_count as usize,
        )?;
        if self.chunk.len() != expected_chunk_length {
            return Err(invalid_overflow(format!(
                "Overflow chunk {} is {} bytes, not {expected_chunk_length}",
                self.chunk_index,
                self.chunk.len()
            )));
        }
        match (self.chunk_index + 1 == self.chunk_count, self.next_page_id) {
            (true, None) | (false, Some(_)) => {}
            (true, Some(_)) => {
                return Err(invalid_overflow("The final overflow chunk has a next page"));
            }
            (false, None) => {
                return Err(invalid_overflow(
                    "A non-final overflow chunk has no next page",
                ));
            }
        }
        if let Some(next_page_id) = self.next_page_id {
            validate_overflow_page_id(next_page_id)?;
            if next_page_id == page_id {
                return Err(invalid_overflow(format!(
                    "Overflow page {page_id} references itself"
                )));
            }
        }

        let mut payload = vec![0; OVERFLOW_HEADER_SIZE + self.chunk.len()];
        payload[..4].copy_from_slice(OVERFLOW_MAGIC);
        payload[4..6].copy_from_slice(&OVERFLOW_FORMAT_VERSION.to_le_bytes());
        payload[6..8].copy_from_slice(&OVERFLOW_FLAGS.to_le_bytes());
        payload[8..16].copy_from_slice(&self.tree_id.to_le_bytes());
        payload[16..24].copy_from_slice(&self.generation.to_le_bytes());
        payload[24..28].copy_from_slice(&self.chunk_index.to_le_bytes());
        payload[28..32].copy_from_slice(&self.chunk_count.to_le_bytes());
        payload[32..40].copy_from_slice(&self.next_page_id.unwrap_or(NO_PAGE_ID).to_le_bytes());
        payload[40..44].copy_from_slice(&self.total_length.to_le_bytes());
        payload[44..48].copy_from_slice(&self.checksum.to_le_bytes());
        payload[48..52].copy_from_slice(&(self.chunk.len() as u32).to_le_bytes());
        payload[OVERFLOW_HEADER_SIZE..].copy_from_slice(&self.chunk);
        Page::new(page_id, PageType::Overflow, payload)
    }

    fn decode(
        page: Page,
        expected_tree_id: TreeId,
        descriptor: &OverflowDescriptor,
        expected_index: u32,
        expected_count: u32,
    ) -> Result<Self> {
        validate_overflow_page_id(page.id)?;
        if page.page_type != PageType::Overflow {
            return Err(invalid_overflow(format!(
                "Overflow chain page {} has type {:?}",
                page.id, page.page_type
            )));
        }
        if page.payload.len() < OVERFLOW_HEADER_SIZE {
            return Err(invalid_overflow(format!(
                "Overflow page {} payload is truncated to {} bytes",
                page.id,
                page.payload.len()
            )));
        }
        let bytes = &page.payload;
        if &bytes[..4] != OVERFLOW_MAGIC {
            return Err(invalid_overflow(format!(
                "Overflow page {} magic does not match",
                page.id
            )));
        }
        let version = read_u16(bytes, 4);
        if version != OVERFLOW_FORMAT_VERSION {
            return Err(unsupported_overflow(format!(
                "Overflow page {} format version {version} is not supported",
                page.id
            )));
        }
        let flags = read_u16(bytes, 6);
        if flags != OVERFLOW_FLAGS {
            return Err(unsupported_overflow(format!(
                "Overflow page {} flags {flags:#06x} are not supported",
                page.id
            )));
        }
        let tree_id = read_u64(bytes, 8);
        let generation = read_u64(bytes, 16);
        let chunk_index = read_u32(bytes, 24);
        let chunk_count = read_u32(bytes, 28);
        let raw_next_page_id = read_u64(bytes, 32);
        let total_length = read_u32(bytes, 40);
        let checksum = read_u32(bytes, 44);
        let chunk_length = read_u32(bytes, 48) as usize;
        if bytes[52..56].iter().any(|byte| *byte != 0) {
            return Err(invalid_overflow(format!(
                "Overflow page {} reserved bytes must be zero",
                page.id
            )));
        }
        if tree_id != expected_tree_id
            || generation != descriptor.generation
            || chunk_index != expected_index
            || chunk_count != expected_count
            || total_length != descriptor.total_length
            || checksum != descriptor.checksum
        {
            return Err(invalid_overflow(format!(
                "Overflow page {} does not match its descriptor or chain position",
                page.id
            )));
        }
        let expected_chunk_length = overflow_chunk_length(
            total_length as usize,
            chunk_index as usize,
            chunk_count as usize,
        )?;
        if chunk_length != expected_chunk_length
            || OVERFLOW_HEADER_SIZE + chunk_length != bytes.len()
        {
            return Err(invalid_overflow(format!(
                "Overflow page {} chunk length {chunk_length} does not match expected {expected_chunk_length}",
                page.id
            )));
        }
        let next_page_id = if raw_next_page_id == NO_PAGE_ID {
            None
        } else {
            validate_overflow_page_id(raw_next_page_id)?;
            Some(raw_next_page_id)
        };
        match (chunk_index + 1 == chunk_count, next_page_id) {
            (true, None) | (false, Some(_)) => {}
            _ => {
                return Err(invalid_overflow(format!(
                    "Overflow page {} has an invalid chain terminator",
                    page.id
                )));
            }
        }
        Ok(Self {
            tree_id,
            generation,
            chunk_index,
            chunk_count,
            next_page_id,
            total_length,
            checksum,
            chunk: bytes[OVERFLOW_HEADER_SIZE..].to_vec(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InternalNode {
    leftmost_child: PageId,
    entries: Vec<InternalEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InternalEntry {
    key: Vec<u8>,
    right_child: PageId,
}

impl InternalNode {
    fn child_index_for(&self, key: &[u8]) -> usize {
        self.entries
            .partition_point(|entry| entry.key.as_slice() <= key)
    }

    fn child_for(&self, key: &[u8]) -> PageId {
        self.child(self.child_index_for(key))
            .expect("the derived child index is in range")
    }

    fn child(&self, index: usize) -> Result<PageId> {
        if index == 0 {
            Ok(self.leftmost_child)
        } else {
            self.entries
                .get(index - 1)
                .map(|entry| entry.right_child)
                .ok_or_else(|| {
                    invalid_btree(format!("Internal child index {index} is out of range"))
                })
        }
    }

    fn replace_child(&mut self, index: usize, page_id: PageId) -> Result<()> {
        if index == 0 {
            self.leftmost_child = page_id;
            Ok(())
        } else if let Some(entry) = self.entries.get_mut(index - 1) {
            entry.right_child = page_id;
            Ok(())
        } else {
            Err(invalid_btree(format!(
                "Internal child index {index} is out of range"
            )))
        }
    }
}

impl Node {
    fn leaf(tree_id: TreeId, generation: u64, entries: Vec<LeafEntry>) -> Self {
        Self {
            tree_id,
            generation,
            level: 0,
            kind: NodeKind::Leaf(entries),
        }
    }

    fn internal(
        tree_id: TreeId,
        generation: u64,
        level: u8,
        leftmost_child: PageId,
        entries: Vec<InternalEntry>,
    ) -> Self {
        Self {
            tree_id,
            generation,
            level,
            kind: NodeKind::Internal(InternalNode {
                leftmost_child,
                entries,
            }),
        }
    }

    fn encode(&self, page_id: PageId) -> Result<Page> {
        validate_tree_id(self.tree_id)?;
        if self.generation == 0 {
            return Err(invalid_btree("B-tree page generation must be positive"));
        }
        let (page_type, leftmost_child, cells) = match &self.kind {
            NodeKind::Leaf(entries) => {
                if self.level != 0 {
                    return Err(invalid_btree("A leaf node must have level zero"));
                }
                validate_sorted_leaf_entries(entries, self.generation)?;
                let cells = entries
                    .iter()
                    .map(encode_leaf_cell)
                    .collect::<Result<Vec<_>>>()?;
                (PageType::BtreeLeaf, NO_PAGE_ID, cells)
            }
            NodeKind::Internal(internal) => {
                if self.level == 0 {
                    return Err(invalid_btree("An internal node must have a positive level"));
                }
                validate_child_page(page_id, internal.leftmost_child)?;
                validate_sorted_internal_entries(
                    page_id,
                    internal.leftmost_child,
                    &internal.entries,
                )?;
                let cells = internal
                    .entries
                    .iter()
                    .map(encode_internal_cell)
                    .collect::<Result<Vec<_>>>()?;
                (PageType::BtreeInternal, internal.leftmost_child, cells)
            }
        };
        if cells.len() > u16::MAX as usize {
            return Err(limit_error("A B-tree node contains too many cells"));
        }
        let required = NODE_HEADER_SIZE
            .checked_add(cells.len() * SLOT_SIZE)
            .and_then(|size| {
                cells
                    .iter()
                    .try_fold(size, |sum, cell| sum.checked_add(cell.len()))
            })
            .ok_or_else(|| limit_error("B-tree node size overflowed"))?;
        if required > MAX_PAGE_PAYLOAD_SIZE {
            return Err(node_full(required));
        }

        let mut payload = vec![0; MAX_PAGE_PAYLOAD_SIZE];
        payload[..4].copy_from_slice(NODE_MAGIC);
        payload[4..6].copy_from_slice(&NODE_FORMAT_VERSION.to_le_bytes());
        payload[6] = self.level;
        payload[7] = NODE_FLAGS;
        payload[8..16].copy_from_slice(&self.tree_id.to_le_bytes());
        payload[16..24].copy_from_slice(&self.generation.to_le_bytes());
        payload[24..26].copy_from_slice(&(cells.len() as u16).to_le_bytes());
        let free_start = NODE_HEADER_SIZE + cells.len() * SLOT_SIZE;
        payload[26..28].copy_from_slice(&(free_start as u16).to_le_bytes());
        payload[32..40].copy_from_slice(&leftmost_child.to_le_bytes());

        let mut free_end = MAX_PAGE_PAYLOAD_SIZE;
        for (index, cell) in cells.iter().enumerate() {
            free_end -= cell.len();
            payload[free_end..free_end + cell.len()].copy_from_slice(cell);
            let slot = NODE_HEADER_SIZE + index * SLOT_SIZE;
            payload[slot..slot + SLOT_SIZE].copy_from_slice(&(free_end as u16).to_le_bytes());
        }
        payload[28..30].copy_from_slice(&(free_end as u16).to_le_bytes());
        Page::new(page_id, page_type, payload)
    }

    fn decode(
        page: Page,
        expected_tree_id: TreeId,
        view_generation: u64,
        require_view_generation: bool,
    ) -> Result<Self> {
        if page.payload.len() != MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_btree(format!(
                "B-tree page {} payload is {} bytes, not {MAX_PAGE_PAYLOAD_SIZE}",
                page.id,
                page.payload.len()
            )));
        }
        let bytes = &page.payload;
        if &bytes[..4] != NODE_MAGIC {
            return Err(invalid_btree(format!(
                "B-tree page {} magic does not match",
                page.id
            )));
        }
        let version = read_u16(bytes, 4);
        if version != NODE_FORMAT_VERSION {
            return Err(unsupported_btree(format!(
                "B-tree page {} format version {version} is not supported",
                page.id
            )));
        }
        let level = bytes[6];
        if bytes[7] != NODE_FLAGS {
            return Err(unsupported_btree(format!(
                "B-tree page {} flags {:#04x} are not supported",
                page.id, bytes[7]
            )));
        }
        let tree_id = read_u64(bytes, 8);
        if tree_id != expected_tree_id {
            return Err(invalid_btree(format!(
                "B-tree page {} belongs to tree {tree_id}, not tree {expected_tree_id}",
                page.id
            )));
        }
        let generation = read_u64(bytes, 16);
        if generation == 0 || generation > view_generation {
            return Err(invalid_btree(format!(
                "B-tree page {} generation {generation} is outside view generation {view_generation}",
                page.id
            )));
        }
        if require_view_generation && generation != view_generation {
            return Err(invalid_btree(format!(
                "Candidate B-tree page {} has generation {generation}, not {view_generation}",
                page.id
            )));
        }
        let item_count = read_u16(bytes, 24) as usize;
        let expected_free_start = NODE_HEADER_SIZE
            .checked_add(item_count * SLOT_SIZE)
            .ok_or_else(|| invalid_btree("B-tree slot array size overflowed"))?;
        let free_start = read_u16(bytes, 26) as usize;
        let free_end = read_u16(bytes, 28) as usize;
        if free_start != expected_free_start
            || free_start > free_end
            || free_end > MAX_PAGE_PAYLOAD_SIZE
        {
            return Err(invalid_btree(format!(
                "B-tree page {} has invalid free-space bounds {free_start}..{free_end}",
                page.id
            )));
        }
        if bytes[30..32].iter().any(|byte| *byte != 0) {
            return Err(invalid_btree(format!(
                "B-tree page {} reserved header bytes must be zero",
                page.id
            )));
        }
        if bytes[free_start..free_end].iter().any(|byte| *byte != 0) {
            return Err(invalid_btree(format!(
                "B-tree page {} free space must be zero-filled",
                page.id
            )));
        }
        let leftmost_child = read_u64(bytes, 32);
        let mut expected_cell_end = MAX_PAGE_PAYLOAD_SIZE;

        let kind = match page.page_type {
            PageType::BtreeLeaf => {
                if level != 0 || leftmost_child != NO_PAGE_ID {
                    return Err(invalid_btree(format!(
                        "B-tree leaf page {} has internal-node header fields",
                        page.id
                    )));
                }
                let mut entries = Vec::with_capacity(item_count);
                for index in 0..item_count {
                    let offset = read_slot(bytes, index, free_start)?;
                    let (entry, cell_end) = decode_leaf_cell(bytes, offset, generation)?;
                    validate_packed_cell(
                        page.id,
                        index,
                        offset,
                        cell_end,
                        expected_cell_end,
                        free_end,
                    )?;
                    expected_cell_end = offset;
                    entries.push(entry);
                }
                validate_cell_floor(page.id, expected_cell_end, free_end)?;
                validate_sorted_leaf_entries(&entries, generation).map_err(as_corruption)?;
                NodeKind::Leaf(entries)
            }
            PageType::BtreeInternal => {
                if level == 0 {
                    return Err(invalid_btree(format!(
                        "B-tree internal page {} has no level",
                        page.id
                    )));
                }
                validate_child_page(page.id, leftmost_child)?;
                let mut entries = Vec::with_capacity(item_count);
                for index in 0..item_count {
                    let offset = read_slot(bytes, index, free_start)?;
                    let (entry, cell_end) = decode_internal_cell(bytes, offset)?;
                    validate_child_page(page.id, entry.right_child)?;
                    validate_packed_cell(
                        page.id,
                        index,
                        offset,
                        cell_end,
                        expected_cell_end,
                        free_end,
                    )?;
                    expected_cell_end = offset;
                    entries.push(entry);
                }
                validate_cell_floor(page.id, expected_cell_end, free_end)?;
                validate_sorted_internal_entries(page.id, leftmost_child, &entries)
                    .map_err(as_corruption)?;
                NodeKind::Internal(InternalNode {
                    leftmost_child,
                    entries,
                })
            }
            other => {
                return Err(invalid_btree(format!(
                    "Page {} has type {other:?}, not a B-tree page",
                    page.id
                )));
            }
        };
        Ok(Self {
            tree_id,
            generation,
            level,
            kind,
        })
    }

    fn encoded_size(&self) -> Result<usize> {
        let cells_size = match &self.kind {
            NodeKind::Leaf(entries) => entries.iter().try_fold(0usize, |size, entry| {
                size.checked_add(
                    LEAF_CELL_HEADER_SIZE + entry.key.len() + entry.value.encoded_len(),
                )
            }),
            NodeKind::Internal(internal) => {
                internal.entries.iter().try_fold(0usize, |size, entry| {
                    size.checked_add(INTERNAL_CELL_HEADER_SIZE + entry.key.len())
                })
            }
        }
        .ok_or_else(|| limit_error("B-tree node size overflowed"))?;
        let count = match &self.kind {
            NodeKind::Leaf(entries) => entries.len(),
            NodeKind::Internal(internal) => internal.entries.len(),
        };
        NODE_HEADER_SIZE
            .checked_add(count * SLOT_SIZE)
            .and_then(|size| size.checked_add(cells_size))
            .ok_or_else(|| limit_error("B-tree node size overflowed"))
    }

    fn fits(&self) -> Result<bool> {
        Ok(self.encoded_size()? <= MAX_PAGE_PAYLOAD_SIZE)
    }
}

struct InsertedPage {
    page_id: PageId,
    split: Option<PageSplit>,
}

struct PageSplit {
    separator: Vec<u8>,
    right_page_id: PageId,
    left_level: u8,
}

struct DeletedPage {
    page_id: Option<PageId>,
    removed: bool,
    first_key_changed: bool,
    first_key: Option<Vec<u8>>,
}

#[allow(clippy::too_many_arguments)]
fn delete_recursive<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    page_id: PageId,
    tree_id: TreeId,
    generation: u64,
    key: &[u8],
    visited: &mut HashSet<PageId>,
    depth: usize,
    expected_level: Option<u8>,
    parent_generation: Option<u64>,
) -> Result<DeletedPage> {
    if depth >= MAX_TREE_DEPTH {
        return Err(invalid_btree(format!(
            "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
        )));
    }
    if !visited.insert(page_id) {
        return Err(invalid_btree(format!(
            "Tree {tree_id} contains a cycle through page {page_id}"
        )));
    }
    let owned = transaction.owns_page(page_id);
    let mut node = Node::decode(transaction.read_page(page_id)?, tree_id, generation, owned)?;
    validate_expected_level(page_id, node.level, expected_level)?;
    validate_child_generation(page_id, node.generation, parent_generation)?;
    let node_level = node.level;
    let node_generation = node.generation;

    let (removed, first_key_changed, first_key, empty) = match &mut node.kind {
        NodeKind::Leaf(entries) => {
            let Ok(index) = entries.binary_search_by(|entry| entry.key.as_slice().cmp(key)) else {
                return Ok(DeletedPage {
                    page_id: Some(page_id),
                    removed: false,
                    first_key_changed: false,
                    first_key: None,
                });
            };
            let removed_first = index == 0;
            let entry = entries.remove(index);
            release_leaf_value(
                transaction,
                tree_id,
                generation,
                node_generation,
                &entry.value,
            )?;
            (
                true,
                removed_first,
                removed_first
                    .then(|| entries.first().map(|entry| entry.key.clone()))
                    .flatten(),
                entries.is_empty(),
            )
        }
        NodeKind::Internal(internal) => {
            let child_index = internal.child_index_for(key);
            let child_page_id = internal.child(child_index)?;
            let child = delete_recursive(
                transaction,
                child_page_id,
                tree_id,
                generation,
                key,
                visited,
                depth + 1,
                Some(node_level - 1),
                Some(node_generation),
            )?;
            if !child.removed {
                return Ok(DeletedPage {
                    page_id: Some(page_id),
                    removed: false,
                    first_key_changed: false,
                    first_key: None,
                });
            }
            match child.page_id {
                Some(child_page_id) => {
                    internal.replace_child(child_index, child_page_id)?;
                    if child_index > 0 && child.first_key_changed {
                        internal.entries[child_index - 1].key =
                            child.first_key.clone().ok_or_else(|| {
                                invalid_btree("A non-empty changed child has no first key")
                            })?;
                    }
                    (
                        true,
                        child_index == 0 && child.first_key_changed,
                        if child_index == 0 && child.first_key_changed {
                            child.first_key
                        } else {
                            None
                        },
                        false,
                    )
                }
                None if internal.entries.is_empty() => (true, true, None, true),
                None if child_index == 0 => {
                    let replacement = internal.entries.remove(0);
                    internal.leftmost_child = replacement.right_child;
                    (true, true, Some(replacement.key), false)
                }
                None => {
                    internal.entries.remove(child_index - 1);
                    (true, false, None, false)
                }
            }
        }
    };

    if empty {
        release_node_page(transaction, page_id, owned)?;
        return Ok(DeletedPage {
            page_id: None,
            removed,
            first_key_changed,
            first_key,
        });
    }

    node.generation = generation;
    let materialized = materialize_node(transaction, page_id, owned, node)?;
    debug_assert!(
        materialized.split.is_none(),
        "deletion cannot split a B-tree page"
    );
    Ok(DeletedPage {
        page_id: Some(materialized.page_id),
        removed,
        first_key_changed,
        first_key,
    })
}

fn release_node_page<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    page_id: PageId,
    owned: bool,
) -> Result<()> {
    if owned {
        transaction.release_new_page(page_id)
    } else {
        transaction.free_shared_page(page_id)
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_recursive<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    page_id: PageId,
    tree_id: TreeId,
    generation: u64,
    key: &[u8],
    value: &[u8],
    visited: &mut HashSet<PageId>,
    depth: usize,
    expected_level: Option<u8>,
    parent_generation: Option<u64>,
) -> Result<InsertedPage> {
    if depth >= MAX_TREE_DEPTH {
        return Err(invalid_btree(format!(
            "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
        )));
    }
    if !visited.insert(page_id) {
        return Err(invalid_btree(format!(
            "Tree {tree_id} contains a cycle through page {page_id}"
        )));
    }
    let owned = transaction.owns_page(page_id);
    let mut node = Node::decode(transaction.read_page(page_id)?, tree_id, generation, owned)?;
    validate_expected_level(page_id, node.level, expected_level)?;
    validate_child_generation(page_id, node.generation, parent_generation)?;
    let node_level = node.level;
    let node_generation = node.generation;

    match &mut node.kind {
        NodeKind::Leaf(entries) => {
            let location = entries.binary_search_by(|entry| entry.key.as_slice().cmp(key));
            if let Ok(index) = location {
                release_leaf_value(
                    transaction,
                    tree_id,
                    generation,
                    node_generation,
                    &entries[index].value,
                )?;
            }
            let value = store_leaf_value(transaction, tree_id, generation, key, value)?;
            match location {
                Ok(index) => entries[index].value = value,
                Err(index) => entries.insert(
                    index,
                    LeafEntry {
                        key: key.to_vec(),
                        value,
                    },
                ),
            }
        }
        NodeKind::Internal(internal) => {
            let child_index = internal.child_index_for(key);
            let child_page_id = internal.child(child_index)?;
            let child = insert_recursive(
                transaction,
                child_page_id,
                tree_id,
                generation,
                key,
                value,
                visited,
                depth + 1,
                Some(node_level - 1),
                Some(node_generation),
            )?;
            internal.replace_child(child_index, child.page_id)?;
            if let Some(split) = child.split {
                if split.left_level + 1 != node_level {
                    return Err(invalid_btree(format!(
                        "Child split level {} does not match parent level {}",
                        split.left_level, node_level
                    )));
                }
                internal.entries.insert(
                    child_index,
                    InternalEntry {
                        key: split.separator,
                        right_child: split.right_page_id,
                    },
                );
            }
        }
    }
    node.generation = generation;
    materialize_node(transaction, page_id, owned, node)
}

fn materialize_node<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    old_page_id: PageId,
    old_owned: bool,
    node: Node,
) -> Result<InsertedPage> {
    if node.fits()? {
        let page_id = if old_owned {
            old_page_id
        } else {
            transaction.allocate_page()?
        };
        write_node(transaction, page_id, &node)?;
        if !old_owned {
            transaction.free_shared_page(old_page_id)?;
        }
        return Ok(InsertedPage {
            page_id,
            split: None,
        });
    }

    let level = node.level;
    let (left, right, separator) = split_node(node)?;
    let left_page_id = if old_owned {
        old_page_id
    } else {
        transaction.allocate_page()?
    };
    let right_page_id = transaction.allocate_page()?;
    write_node(transaction, left_page_id, &left)?;
    write_node(transaction, right_page_id, &right)?;
    if !old_owned {
        transaction.free_shared_page(old_page_id)?;
    }
    Ok(InsertedPage {
        page_id: left_page_id,
        split: Some(PageSplit {
            separator,
            right_page_id,
            left_level: level,
        }),
    })
}

fn split_node(node: Node) -> Result<(Node, Node, Vec<u8>)> {
    match node.kind {
        NodeKind::Leaf(entries) => {
            let split = choose_leaf_split(&entries)?;
            let right_entries = entries[split..].to_vec();
            let separator = right_entries
                .first()
                .expect("a leaf split has a non-empty right side")
                .key
                .clone();
            let left_entries = entries[..split].to_vec();
            Ok((
                Node::leaf(node.tree_id, node.generation, left_entries),
                Node::leaf(node.tree_id, node.generation, right_entries),
                separator,
            ))
        }
        NodeKind::Internal(internal) => {
            let promote = choose_internal_split(&internal.entries)?;
            let promoted = internal.entries[promote].clone();
            let left_entries = internal.entries[..promote].to_vec();
            let right_entries = internal.entries[promote + 1..].to_vec();
            Ok((
                Node::internal(
                    node.tree_id,
                    node.generation,
                    node.level,
                    internal.leftmost_child,
                    left_entries,
                ),
                Node::internal(
                    node.tree_id,
                    node.generation,
                    node.level,
                    promoted.right_child,
                    right_entries,
                ),
                promoted.key,
            ))
        }
    }
}

fn choose_leaf_split(entries: &[LeafEntry]) -> Result<usize> {
    if entries.len() < 2 {
        return Err(limit_error("A single leaf entry cannot fit in a page"));
    }
    let mut best = None;
    for split in 1..entries.len() {
        let left = Node::leaf(1, 1, entries[..split].to_vec()).encoded_size()?;
        let right = Node::leaf(1, 1, entries[split..].to_vec()).encoded_size()?;
        if left <= MAX_PAGE_PAYLOAD_SIZE && right <= MAX_PAGE_PAYLOAD_SIZE {
            let imbalance = left.abs_diff(right);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((split, imbalance));
            }
        }
    }
    best.map(|(split, _)| split)
        .ok_or_else(|| limit_error("Leaf entries cannot be divided into bounded pages"))
}

fn choose_internal_split(entries: &[InternalEntry]) -> Result<usize> {
    if entries.len() < 3 {
        return Err(limit_error(
            "Internal separators cannot be divided into bounded pages",
        ));
    }
    let mut best = None;
    for promote in 1..entries.len() - 1 {
        let left_size = internal_entries_size(&entries[..promote])?;
        let right_size = internal_entries_size(&entries[promote + 1..])?;
        if left_size <= MAX_PAGE_PAYLOAD_SIZE && right_size <= MAX_PAGE_PAYLOAD_SIZE {
            let imbalance = left_size.abs_diff(right_size);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((promote, imbalance));
            }
        }
    }
    best.map(|(promote, _)| promote)
        .ok_or_else(|| limit_error("Internal separators cannot be divided into bounded pages"))
}

fn internal_entries_size(entries: &[InternalEntry]) -> Result<usize> {
    entries.iter().try_fold(
        NODE_HEADER_SIZE + entries.len() * SLOT_SIZE,
        |size, entry| {
            size.checked_add(INTERNAL_CELL_HEADER_SIZE + entry.key.len())
                .ok_or_else(|| limit_error("B-tree node size overflowed"))
        },
    )
}

fn write_node<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    page_id: PageId,
    node: &Node,
) -> Result<()> {
    transaction.write_new_page(&node.encode(page_id)?)
}

fn store_leaf_value<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    tree_id: TreeId,
    generation: u64,
    key: &[u8],
    value: &[u8],
) -> Result<LeafValue> {
    if value.len() <= MAX_BTREE_INLINE_VALUE_BYTES
        && key.len() + value.len() <= MAX_BTREE_INLINE_ENTRY_BYTES
    {
        return Ok(LeafValue::Inline(value.to_vec()));
    }

    let chunk_count = value.len().div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
    if chunk_count == 0 || chunk_count > MAX_OVERFLOW_PAGE_COUNT {
        return Err(limit_error(format!(
            "Overflow value requires {chunk_count} pages, exceeding {MAX_OVERFLOW_PAGE_COUNT}"
        )));
    }
    let mut page_ids = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        page_ids.push(transaction.allocate_page()?);
    }
    let checksum = crc32(value);
    for (index, page_id) in page_ids.iter().copied().enumerate() {
        let start = index * MAX_OVERFLOW_CHUNK_BYTES;
        let end = (start + MAX_OVERFLOW_CHUNK_BYTES).min(value.len());
        let overflow = OverflowPage {
            tree_id,
            generation,
            chunk_index: index as u32,
            chunk_count: chunk_count as u32,
            next_page_id: page_ids.get(index + 1).copied(),
            total_length: value.len() as u32,
            checksum,
            chunk: value[start..end].to_vec(),
        };
        transaction.write_new_page(&overflow.encode(page_id)?)?;
    }
    Ok(LeafValue::Overflow(OverflowDescriptor {
        first_page_id: page_ids[0],
        generation,
        total_length: value.len() as u32,
        checksum,
    }))
}

fn materialize_committed_value<D: PageDevice>(
    pager: &mut Pager<D>,
    tree_id: TreeId,
    view_generation: u64,
    leaf_generation: u64,
    value: &LeafValue,
) -> Result<Vec<u8>> {
    match value {
        LeafValue::Inline(value) => Ok(value.clone()),
        LeafValue::Overflow(descriptor) => read_overflow_chain(
            |page_id| pager.read_page(page_id),
            tree_id,
            view_generation,
            leaf_generation,
            descriptor,
        )
        .map(|(value, _)| value),
    }
}

fn release_leaf_value<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    tree_id: TreeId,
    view_generation: u64,
    leaf_generation: u64,
    value: &LeafValue,
) -> Result<()> {
    let LeafValue::Overflow(descriptor) = value else {
        return Ok(());
    };
    // Validate and materialize the complete chain before changing allocation state. A malformed
    // descriptor must never cause a partially-reclaimed chain.
    let (_, pages) = read_overflow_chain(
        |page_id| transaction.read_page(page_id),
        tree_id,
        view_generation,
        leaf_generation,
        descriptor,
    )?;
    for page_id in pages {
        if transaction.owns_page(page_id) {
            transaction.release_new_page(page_id)?;
        } else {
            transaction.free_shared_page(page_id)?;
        }
    }
    Ok(())
}

fn read_overflow_chain(
    mut read_page: impl FnMut(PageId) -> Result<Page>,
    tree_id: TreeId,
    view_generation: u64,
    leaf_generation: u64,
    descriptor: &OverflowDescriptor,
) -> Result<(Vec<u8>, Vec<PageId>)> {
    validate_overflow_descriptor(descriptor, leaf_generation)?;
    if descriptor.generation > view_generation {
        return Err(invalid_overflow(format!(
            "Overflow generation {} exceeds view generation {view_generation}",
            descriptor.generation
        )));
    }
    let chunk_count = (descriptor.total_length as usize).div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
    let mut value = Vec::with_capacity(descriptor.total_length as usize);
    let mut pages = Vec::with_capacity(chunk_count);
    let mut visited = HashSet::with_capacity(chunk_count);
    let mut page_id = descriptor.first_page_id;
    for index in 0..chunk_count {
        if !visited.insert(page_id) {
            return Err(invalid_overflow(format!(
                "Overflow chain contains a cycle through page {page_id}"
            )));
        }
        let overflow = OverflowPage::decode(
            read_page(page_id)?,
            tree_id,
            descriptor,
            index as u32,
            chunk_count as u32,
        )?;
        value.extend_from_slice(&overflow.chunk);
        pages.push(page_id);
        if index + 1 < chunk_count {
            page_id = overflow.next_page_id.ok_or_else(|| {
                invalid_overflow(format!("Overflow chain ended after page {page_id}"))
            })?;
        }
    }
    if value.len() != descriptor.total_length as usize {
        return Err(invalid_overflow(format!(
            "Overflow chain materialized {} bytes, not {}",
            value.len(),
            descriptor.total_length
        )));
    }
    if crc32(&value) != descriptor.checksum {
        return Err(invalid_overflow(
            "Overflow chain end-to-end checksum does not match",
        ));
    }
    Ok((value, pages))
}

fn encode_leaf_cell(entry: &LeafEntry) -> Result<Vec<u8>> {
    validate_key(&entry.key)?;
    let mut cell =
        Vec::with_capacity(LEAF_CELL_HEADER_SIZE + entry.key.len() + entry.value.encoded_len());
    cell.extend_from_slice(&(entry.key.len() as u16).to_le_bytes());
    cell.extend_from_slice(&entry.value.flags().to_le_bytes());
    cell.extend_from_slice(&(entry.value.encoded_len() as u32).to_le_bytes());
    cell.extend_from_slice(&entry.key);
    entry.value.encode_into(&mut cell);
    Ok(cell)
}

fn encode_internal_cell(entry: &InternalEntry) -> Result<Vec<u8>> {
    validate_key(&entry.key)?;
    let mut cell = Vec::with_capacity(INTERNAL_CELL_HEADER_SIZE + entry.key.len());
    cell.extend_from_slice(&(entry.key.len() as u16).to_le_bytes());
    cell.extend_from_slice(&INLINE_CELL_FLAGS.to_le_bytes());
    cell.extend_from_slice(&entry.right_child.to_le_bytes());
    cell.extend_from_slice(&entry.key);
    Ok(cell)
}

fn decode_leaf_cell(
    bytes: &[u8],
    offset: usize,
    leaf_generation: u64,
) -> Result<(LeafEntry, usize)> {
    let header_end = checked_end(offset, LEAF_CELL_HEADER_SIZE, bytes.len())?;
    let key_length = read_u16(bytes, offset) as usize;
    let flags = read_u16(bytes, offset + 2);
    let value_length = read_u32(bytes, offset + 4) as usize;
    let key_end = checked_end(header_end, key_length, bytes.len())?;
    let value_end = checked_end(key_end, value_length, bytes.len())?;
    let value = match flags {
        INLINE_CELL_FLAGS => {
            validate_decoded_inline_entry(key_length, value_length)?;
            LeafValue::Inline(bytes[key_end..value_end].to_vec())
        }
        OVERFLOW_CELL_FLAGS => {
            if value_length != OVERFLOW_DESCRIPTOR_SIZE {
                return Err(invalid_btree(format!(
                    "Overflow descriptor is {value_length} bytes, not {OVERFLOW_DESCRIPTOR_SIZE}"
                )));
            }
            let descriptor = OverflowDescriptor {
                first_page_id: read_u64(bytes, key_end),
                generation: read_u64(bytes, key_end + 8),
                total_length: read_u32(bytes, key_end + 16),
                checksum: read_u32(bytes, key_end + 20),
            };
            validate_overflow_descriptor(&descriptor, leaf_generation)?;
            LeafValue::Overflow(descriptor)
        }
        _ => {
            return Err(unsupported_btree(format!(
                "Leaf cell flags {flags:#06x} are not supported"
            )));
        }
    };
    Ok((
        LeafEntry {
            key: bytes[header_end..key_end].to_vec(),
            value,
        },
        value_end,
    ))
}

fn decode_internal_cell(bytes: &[u8], offset: usize) -> Result<(InternalEntry, usize)> {
    let header_end = checked_end(offset, INTERNAL_CELL_HEADER_SIZE, bytes.len())?;
    let key_length = read_u16(bytes, offset) as usize;
    let flags = read_u16(bytes, offset + 2);
    if flags != INLINE_CELL_FLAGS {
        return Err(unsupported_btree(format!(
            "Internal cell flags {flags:#06x} are not supported"
        )));
    }
    if key_length > MAX_BTREE_KEY_BYTES {
        return Err(invalid_btree(format!(
            "Internal key length {key_length} exceeds {MAX_BTREE_KEY_BYTES}"
        )));
    }
    let key_end = checked_end(header_end, key_length, bytes.len())?;
    Ok((
        InternalEntry {
            key: bytes[header_end..key_end].to_vec(),
            right_child: read_u64(bytes, offset + 4),
        },
        key_end,
    ))
}

fn read_slot(bytes: &[u8], index: usize, free_start: usize) -> Result<usize> {
    let offset = NODE_HEADER_SIZE + index * SLOT_SIZE;
    if offset + SLOT_SIZE > free_start || offset + SLOT_SIZE > bytes.len() {
        return Err(invalid_btree(format!(
            "B-tree slot {index} lies outside the slot array"
        )));
    }
    Ok(read_u16(bytes, offset) as usize)
}

fn validate_packed_cell(
    page_id: PageId,
    index: usize,
    offset: usize,
    cell_end: usize,
    expected_cell_end: usize,
    free_end: usize,
) -> Result<()> {
    if offset < free_end || cell_end != expected_cell_end {
        return Err(invalid_btree(format!(
            "B-tree page {page_id} cell {index} is overlapping, out of order, or not tightly packed"
        )));
    }
    Ok(())
}

fn validate_cell_floor(page_id: PageId, actual: usize, expected: usize) -> Result<()> {
    if actual != expected {
        return Err(invalid_btree(format!(
            "B-tree page {page_id} cell area starts at {actual}, not {expected}"
        )));
    }
    Ok(())
}

fn checked_end(offset: usize, length: usize, bound: usize) -> Result<usize> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| invalid_btree("B-tree cell offset overflowed"))?;
    if end > bound {
        return Err(invalid_btree(format!(
            "B-tree cell range {offset}..{end} exceeds page payload {bound}"
        )));
    }
    Ok(end)
}

fn validate_sorted_leaf_entries(entries: &[LeafEntry], leaf_generation: u64) -> Result<()> {
    for entry in entries {
        validate_stored_entry(&entry.key, &entry.value, leaf_generation)?;
    }
    validate_strictly_sorted(entries.iter().map(|entry| entry.key.as_slice()))
}

fn validate_sorted_internal_entries(
    page_id: PageId,
    leftmost_child: PageId,
    entries: &[InternalEntry],
) -> Result<()> {
    let mut children = HashSet::with_capacity(entries.len() + 1);
    children.insert(leftmost_child);
    for entry in entries {
        validate_key(&entry.key)?;
        validate_child_page(page_id, entry.right_child)?;
        if !children.insert(entry.right_child) {
            return Err(invalid_btree(format!(
                "Internal page {page_id} references child {} more than once",
                entry.right_child
            )));
        }
    }
    validate_strictly_sorted(entries.iter().map(|entry| entry.key.as_slice()))
}

fn validate_strictly_sorted<'a>(keys: impl Iterator<Item = &'a [u8]>) -> Result<()> {
    let mut previous: Option<&[u8]> = None;
    for key in keys {
        if previous.is_some_and(|previous| previous >= key) {
            return Err(invalid_btree(
                "B-tree keys must be in strictly increasing byte order",
            ));
        }
        previous = Some(key);
    }
    Ok(())
}

fn validate_tree_id(tree_id: TreeId) -> Result<()> {
    if tree_id == 0 {
        Err(EngineError::new(
            "INVALID_BTREE_ARGUMENT",
            "B-tree ID zero is reserved",
        ))
    } else {
        Ok(())
    }
}

fn validate_key(key: &[u8]) -> Result<()> {
    if key.len() > MAX_BTREE_KEY_BYTES {
        Err(limit_error(format!(
            "B-tree key is {} bytes, exceeding {MAX_BTREE_KEY_BYTES}",
            key.len()
        )))
    } else {
        Ok(())
    }
}

fn validate_value(value: &[u8]) -> Result<()> {
    if value.len() > MAX_BTREE_VALUE_BYTES {
        return Err(limit_error(format!(
            "B-tree value is {} bytes, exceeding {MAX_BTREE_VALUE_BYTES}",
            value.len()
        )));
    }
    Ok(())
}

fn validate_overflow_descriptor(
    descriptor: &OverflowDescriptor,
    leaf_generation: u64,
) -> Result<()> {
    validate_overflow_page_id(descriptor.first_page_id)?;
    if descriptor.generation == 0 || descriptor.generation > leaf_generation {
        return Err(invalid_overflow(format!(
            "Overflow generation {} is outside leaf generation {leaf_generation}",
            descriptor.generation
        )));
    }
    let total_length = descriptor.total_length as usize;
    if total_length == 0 || total_length > MAX_BTREE_VALUE_BYTES {
        return Err(invalid_overflow(format!(
            "Overflow total length {total_length} is outside 1..={MAX_BTREE_VALUE_BYTES}"
        )));
    }
    let chunk_count = total_length.div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
    if chunk_count == 0 || chunk_count > MAX_OVERFLOW_PAGE_COUNT {
        return Err(invalid_overflow(format!(
            "Overflow descriptor requires invalid chunk count {chunk_count}"
        )));
    }
    Ok(())
}

fn validate_overflow_page_id(page_id: PageId) -> Result<()> {
    if !(FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT).contains(&page_id) {
        return Err(invalid_overflow(format!(
            "Overflow page ID {page_id} is outside the data-page range"
        )));
    }
    Ok(())
}

fn overflow_chunk_length(
    total_length: usize,
    chunk_index: usize,
    chunk_count: usize,
) -> Result<usize> {
    if chunk_count == 0 || chunk_index >= chunk_count {
        return Err(invalid_overflow(format!(
            "Overflow chunk index {chunk_index} is outside count {chunk_count}"
        )));
    }
    let expected_count = total_length.div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
    if expected_count != chunk_count {
        return Err(invalid_overflow(format!(
            "Overflow total length {total_length} needs {expected_count} chunks, not {chunk_count}"
        )));
    }
    if chunk_index + 1 == chunk_count {
        Ok(total_length - chunk_index * MAX_OVERFLOW_CHUNK_BYTES)
    } else {
        Ok(MAX_OVERFLOW_CHUNK_BYTES)
    }
}

fn validate_stored_entry(key: &[u8], value: &LeafValue, leaf_generation: u64) -> Result<()> {
    validate_key(key)?;
    match value {
        LeafValue::Inline(value) => {
            if value.len() > MAX_BTREE_INLINE_VALUE_BYTES
                || key.len() + value.len() > MAX_BTREE_INLINE_ENTRY_BYTES
            {
                return Err(limit_error(format!(
                    "Inline B-tree key and value total {} bytes outside supported bounds",
                    key.len() + value.len()
                )));
            }
        }
        LeafValue::Overflow(descriptor) => {
            validate_overflow_descriptor(descriptor, leaf_generation)?
        }
    }
    Ok(())
}

fn validate_decoded_inline_entry(key_length: usize, value_length: usize) -> Result<()> {
    if key_length > MAX_BTREE_KEY_BYTES
        || value_length > MAX_BTREE_INLINE_VALUE_BYTES
        || key_length.saturating_add(value_length) > MAX_BTREE_INLINE_ENTRY_BYTES
    {
        return Err(invalid_btree(format!(
            "Decoded inline entry lengths {key_length}+{value_length} exceed the supported bounds"
        )));
    }
    Ok(())
}

fn validate_child_page(parent_page_id: PageId, child_page_id: PageId) -> Result<()> {
    if !(FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT).contains(&child_page_id) {
        return Err(invalid_btree(format!(
            "Internal page {parent_page_id} references invalid child page {child_page_id}"
        )));
    }
    if child_page_id == parent_page_id {
        return Err(invalid_btree(format!(
            "Internal page {parent_page_id} references itself"
        )));
    }
    Ok(())
}

fn validate_expected_level(
    page_id: PageId,
    actual_level: u8,
    expected_level: Option<u8>,
) -> Result<()> {
    if let Some(expected_level) = expected_level
        && actual_level != expected_level
    {
        return Err(invalid_btree(format!(
            "B-tree page {page_id} has level {actual_level}, not its expected level {expected_level}"
        )));
    }
    Ok(())
}

fn validate_child_generation(
    page_id: PageId,
    generation: u64,
    parent_generation: Option<u64>,
) -> Result<()> {
    if let Some(parent_generation) = parent_generation
        && generation > parent_generation
    {
        return Err(invalid_btree(format!(
            "B-tree page {page_id} generation {generation} is newer than its parent generation {parent_generation}"
        )));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("bounded u16"))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("bounded u32"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("bounded u64"))
}

fn as_corruption(error: EngineError) -> EngineError {
    invalid_btree(error.message)
}

fn node_full(required: usize) -> EngineError {
    EngineError::new(
        "BTREE_NODE_FULL",
        format!(
            "B-tree node requires {required} bytes, exceeding the {MAX_PAGE_PAYLOAD_SIZE}-byte payload"
        ),
    )
}

fn invalid_btree(message: impl Into<String>) -> EngineError {
    EngineError::new("INVALID_BTREE_PAGE", message)
}

fn unsupported_btree(message: impl Into<String>) -> EngineError {
    EngineError::new("UNSUPPORTED_BTREE_PAGE", message)
}

fn limit_error(message: impl Into<String>) -> EngineError {
    EngineError::new("BTREE_LIMIT", message)
}

fn invalid_overflow(message: impl Into<String>) -> EngineError {
    EngineError::new("INVALID_OVERFLOW_PAGE", message)
}

fn unsupported_overflow(message: impl Into<String>) -> EngineError {
    EngineError::new("UNSUPPORTED_OVERFLOW_PAGE", message)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{AllocationBitmap, MemoryPageDevice, PageDevice};

    const TREE: TreeId = 7;

    fn key(number: u32) -> Vec<u8> {
        let mut key = vec![b'k'; 900];
        key[..4].copy_from_slice(&number.to_be_bytes());
        key
    }

    fn value(number: u32) -> Vec<u8> {
        let mut value = vec![b'v'; 500];
        value[..4].copy_from_slice(&number.to_be_bytes());
        value
    }

    fn create_tree(pager: &mut Pager<MemoryPageDevice>) -> PageId {
        let mut transaction = pager.begin_write().unwrap();
        let root = Btree::create(&mut transaction, TREE).unwrap();
        transaction.commit(1, 1, Some(root)).unwrap();
        root
    }

    fn upsert_and_commit(
        pager: &mut Pager<MemoryPageDevice>,
        root: PageId,
        revision: u64,
        key: &[u8],
        value: &[u8],
    ) -> PageId {
        let mut transaction = pager.begin_write().unwrap();
        let root = Btree::upsert(&mut transaction, root, TREE, key, value).unwrap();
        transaction.commit(revision, revision, Some(root)).unwrap();
        root
    }

    fn overflow_descriptor_from_root(
        pager: &mut Pager<MemoryPageDevice>,
        root: PageId,
        key: &[u8],
    ) -> OverflowDescriptor {
        let node = Node::decode(
            pager.read_page(root).unwrap(),
            TREE,
            pager.generation(),
            false,
        )
        .unwrap();
        let NodeKind::Leaf(entries) = node.kind else {
            panic!("test expected a leaf root");
        };
        let entry = &entries[entries
            .binary_search_by(|entry| entry.key.as_slice().cmp(key))
            .unwrap()];
        let LeafValue::Overflow(descriptor) = &entry.value else {
            panic!("test expected overflow value");
        };
        descriptor.clone()
    }

    fn assert_exact_separators(
        pager: &mut Pager<MemoryPageDevice>,
        page_id: PageId,
    ) -> Option<Vec<u8>> {
        let node = Node::decode(
            pager.read_page(page_id).unwrap(),
            TREE,
            pager.generation(),
            false,
        )
        .unwrap();
        match node.kind {
            NodeKind::Leaf(entries) => entries.first().map(|entry| entry.key.clone()),
            NodeKind::Internal(internal) => {
                let mut first = assert_exact_separators(pager, internal.leftmost_child);
                for entry in internal.entries {
                    let right_first = assert_exact_separators(pager, entry.right_child);
                    if let Some(right_first) = right_first {
                        assert_eq!(entry.key, right_first);
                        if first.is_none() {
                            first = Some(entry.key);
                        }
                    }
                }
                first
            }
        }
    }

    #[derive(Debug)]
    struct SparseDevice {
        pages: HashMap<PageId, [u8; crate::PAGE_SIZE]>,
    }

    impl PageDevice for SparseDevice {
        fn page_count(&self) -> PageId {
            MAX_PAGE_COUNT
        }

        fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
            if id >= MAX_PAGE_COUNT || destination.len() != crate::PAGE_SIZE {
                return Err(EngineError::new("SPARSE_DEVICE", "invalid read"));
            }
            destination.copy_from_slice(self.pages.get(&id).unwrap_or(&[0; crate::PAGE_SIZE]));
            Ok(())
        }

        fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
            if id >= MAX_PAGE_COUNT || source.len() != crate::PAGE_SIZE {
                return Err(EngineError::new("SPARSE_DEVICE", "invalid write"));
            }
            self.pages.insert(id, source.try_into().unwrap());
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn into_sparse_pager_with_free_pages(
        pager: Pager<MemoryPageDevice>,
        free_pages: &[PageId],
    ) -> Pager<SparseDevice> {
        let active = pager.active_metadata().clone();
        let mut memory = pager.into_device();
        let mut bitmap = active.allocation_bitmap;
        for id in FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT {
            bitmap.set_allocated(id, true).unwrap();
        }
        for id in free_pages {
            bitmap.set_allocated(*id, false).unwrap();
        }
        let mut superblock = active.superblock;
        superblock.live_data_page_count =
            (MAX_PAGE_COUNT - FIRST_DATA_PAGE_ID - free_pages.len() as u64) as u32;
        for (chunk, page) in bitmap.encode_pages().unwrap().iter().enumerate() {
            memory
                .write_page(superblock.bitmap_slot.page_id(chunk), page)
                .unwrap();
        }
        memory
            .write_page(
                superblock.slot.page_id(),
                &superblock.encode_page().unwrap(),
            )
            .unwrap();

        let physical_count = memory.page_count();
        let mut pages = HashMap::new();
        for id in 0..physical_count {
            let mut bytes = [0; crate::PAGE_SIZE];
            memory.read_page(id, &mut bytes).unwrap();
            pages.insert(id, bytes);
        }
        Pager::open_or_create(SparseDevice { pages }).unwrap()
    }

    #[test]
    fn inserts_splits_reads_scans_seeks_updates_and_reopens() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);

        {
            let mut transaction = pager.begin_write().unwrap();
            for number in (0..80).rev() {
                root = Btree::upsert(&mut transaction, root, TREE, &key(number), &value(number))
                    .unwrap();
            }
            // Repeated right-edge changes must rewrite candidate-owned pages rather than leaking
            // a fresh COW path for every change in one transaction.
            for _ in 0..3 {
                root = Btree::upsert(&mut transaction, root, TREE, &key(79), &value(79)).unwrap();
            }
            transaction.commit(2, 2, Some(root)).unwrap();
        }

        let root_node = Node::decode(
            pager.read_page(root).unwrap(),
            TREE,
            pager.generation(),
            false,
        )
        .unwrap();
        assert!(
            root_node.level >= 2,
            "test data must exercise multiple internal levels"
        );

        for number in [0, 1, 37, 79] {
            assert_eq!(
                Btree::get(&mut pager, root, TREE, &key(number)).unwrap(),
                Some(value(number))
            );
        }
        assert_eq!(Btree::get(&mut pager, root, TREE, &key(80)).unwrap(), None);

        let mut cursor = Btree::cursor(&mut pager, root, TREE).unwrap();
        let mut scanned = Vec::new();
        while let Some((key, value)) = cursor.next(&mut pager).unwrap() {
            scanned.push((u32::from_be_bytes(key[..4].try_into().unwrap()), value));
        }
        assert_eq!(
            scanned
                .iter()
                .map(|(number, _)| *number)
                .collect::<Vec<_>>(),
            (0..80).collect::<Vec<_>>()
        );
        assert!(
            scanned
                .iter()
                .all(|(number, actual)| actual == &value(*number))
        );

        let mut cursor = Btree::cursor_from(&mut pager, root, TREE, &key(37)).unwrap();
        assert_eq!(
            cursor.next(&mut pager).unwrap().unwrap().0[..4],
            37_u32.to_be_bytes()
        );

        {
            let mut transaction = pager.begin_write().unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, &key(37), b"replacement").unwrap();
            transaction.commit(3, 3, Some(root)).unwrap();
        }
        assert_eq!(
            Btree::get(&mut pager, root, TREE, &key(37)).unwrap(),
            Some(b"replacement".to_vec())
        );

        let device = pager.into_device();
        let mut reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(reopened.catalog_root_page_id(), Some(root));
        assert_eq!(
            Btree::get(&mut reopened, root, TREE, &key(79)).unwrap(),
            Some(value(79))
        );
    }

    #[test]
    fn deletes_copy_on_write_across_levels_and_advances_separators() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in 0..80 {
                root = Btree::upsert(&mut transaction, root, TREE, &key(number), &value(number))
                    .unwrap();
            }
            transaction.commit(2, 2, Some(root)).unwrap();
        }
        assert!(
            Node::decode(
                pager.read_page(root).unwrap(),
                TREE,
                pager.generation(),
                false,
            )
            .unwrap()
            .level
                >= 2
        );
        assert!(assert_exact_separators(&mut pager, root).is_some());
        let boundary_key = {
            let node = Node::decode(
                pager.read_page(root).unwrap(),
                TREE,
                pager.generation(),
                false,
            )
            .unwrap();
            let NodeKind::Internal(internal) = node.kind else {
                panic!("test expected an internal root");
            };
            internal.entries[0].key.clone()
        };
        let boundary_number = u32::from_be_bytes(boundary_key[..4].try_into().unwrap());
        {
            let mut transaction = pager.begin_write().unwrap();
            let (next, removed) =
                Btree::delete(&mut transaction, root, TREE, &boundary_key).unwrap();
            assert!(removed);
            root = next.unwrap();
            transaction.commit(3, 3, Some(root)).unwrap();
        }
        assert!(assert_exact_separators(&mut pager, root).is_some());

        let committed_root = root;
        {
            let mut transaction = pager.begin_write().unwrap();
            let (unchanged, removed) =
                Btree::delete(&mut transaction, root, TREE, &key(100)).unwrap();
            assert!(!removed);
            assert_eq!(unchanged, Some(root));
            for number in 0..70 {
                if number == boundary_number {
                    continue;
                }
                let (next, removed) =
                    Btree::delete(&mut transaction, root, TREE, &key(number)).unwrap();
                assert!(removed);
                root = next.unwrap();
            }
            transaction.commit(4, 4, Some(root)).unwrap();
        }
        assert_ne!(root, committed_root);
        assert_eq!(Btree::get(&mut pager, root, TREE, &key(69)).unwrap(), None);
        assert_eq!(
            Btree::get(&mut pager, root, TREE, &key(70)).unwrap(),
            Some(value(70))
        );
        let mut cursor = Btree::cursor(&mut pager, root, TREE).unwrap();
        let mut remaining = Vec::new();
        while let Some((entry_key, _)) = cursor.next(&mut pager).unwrap() {
            remaining.push(u32::from_be_bytes(entry_key[..4].try_into().unwrap()));
        }
        assert_eq!(remaining, (70..80).collect::<Vec<_>>());

        {
            let mut transaction = pager.begin_write().unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, &key(65), &value(65)).unwrap();
            transaction.commit(5, 5, Some(root)).unwrap();
        }
        let mut cursor = Btree::cursor(&mut pager, root, TREE).unwrap();
        let mut reinserted = Vec::new();
        while let Some((entry_key, _)) = cursor.next(&mut pager).unwrap() {
            reinserted.push(u32::from_be_bytes(entry_key[..4].try_into().unwrap()));
        }
        assert_eq!(
            reinserted,
            std::iter::once(65).chain(70..80).collect::<Vec<_>>()
        );

        let reopened_root = root;
        let mut reopened = Pager::open_or_create(pager.into_device()).unwrap();
        assert_eq!(
            Btree::get(&mut reopened, reopened_root, TREE, &key(70)).unwrap(),
            Some(value(70))
        );
    }

    #[test]
    fn deleting_single_leaf_root_reclaims_overflow_and_absent_delete_is_unchanged() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        let large = vec![9; MAX_OVERFLOW_CHUNK_BYTES * 2 + 17];
        root = upsert_and_commit(&mut pager, root, 2, b"large", &large);
        let overflow = overflow_descriptor_from_root(&mut pager, root, b"large");
        let live_before = pager.active_metadata().superblock.live_data_page_count;

        {
            let mut transaction = pager.begin_write().unwrap();
            let (same_root, removed) =
                Btree::delete(&mut transaction, root, TREE, b"absent").unwrap();
            assert!(!removed);
            assert_eq!(same_root, Some(root));
            let (empty_root, removed) =
                Btree::delete(&mut transaction, root, TREE, b"large").unwrap();
            assert!(removed);
            assert_eq!(empty_root, None);
            transaction.commit(3, 3, None).unwrap();
        }
        assert!(pager.active_metadata().superblock.live_data_page_count < live_before);
        assert_eq!(
            pager.read_page(overflow.first_page_id).unwrap_err().code,
            "PAGE_NOT_ALLOCATED"
        );
    }

    #[test]
    fn deleting_every_entry_prunes_empty_descendants_and_root_then_reuses_pages() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in 0..80 {
                root = Btree::upsert(&mut transaction, root, TREE, &key(number), &value(number))
                    .unwrap();
            }
            transaction.commit(2, 2, Some(root)).unwrap();
        }
        let live_before = pager.active_metadata().superblock.live_data_page_count;
        let mut next_root = Some(root);
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in (0..80).rev() {
                let (next, removed) =
                    Btree::delete(&mut transaction, next_root.unwrap(), TREE, &key(number))
                        .unwrap();
                assert!(removed);
                next_root = next;
            }
            assert_eq!(next_root, None);
            transaction.commit(3, 3, None).unwrap();
        }
        assert!(pager.active_metadata().superblock.live_data_page_count < live_before);

        let mut transaction = pager.begin_write().unwrap();
        let reused = Btree::create(&mut transaction, TREE).unwrap();
        let reused = Btree::upsert(&mut transaction, reused, TREE, b"again", b"works").unwrap();
        transaction.commit(4, 4, Some(reused)).unwrap();
        assert_eq!(
            Btree::get(&mut pager, reused, TREE, b"again").unwrap(),
            Some(b"works".to_vec())
        );
    }

    #[test]
    fn inline_boundaries_and_one_mib_overflow_round_trip_get_cursor_and_reopen() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        let inline = vec![11; MAX_BTREE_INLINE_VALUE_BYTES];
        let forced_overflow = vec![12; MAX_BTREE_INLINE_VALUE_BYTES + 1];
        let maximum = (0..MAX_BTREE_VALUE_BYTES)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();

        {
            let mut transaction = pager.begin_write().unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, b"inline", &inline).unwrap();
            root =
                Btree::upsert(&mut transaction, root, TREE, b"overflow", &forced_overflow).unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, b"maximum", &maximum).unwrap();
            transaction.commit(2, 2, Some(root)).unwrap();
        }

        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"inline").unwrap(),
            Some(inline.clone())
        );
        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"overflow").unwrap(),
            Some(forced_overflow.clone())
        );
        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"maximum").unwrap(),
            Some(maximum.clone())
        );
        let mut cursor = Btree::cursor(&mut pager, root, TREE).unwrap();
        let mut seen = std::collections::HashMap::new();
        while let Some((key, value)) = cursor.next(&mut pager).unwrap() {
            seen.insert(key, value);
        }
        assert_eq!(seen.get(b"inline".as_slice()), Some(&inline));
        assert_eq!(seen.get(b"overflow".as_slice()), Some(&forced_overflow));
        assert_eq!(seen.get(b"maximum".as_slice()), Some(&maximum));

        let device = pager.into_device();
        let mut reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(
            Btree::get(&mut reopened, root, TREE, b"maximum").unwrap(),
            Some(maximum)
        );
    }

    #[test]
    fn combined_inline_entry_boundary_spills_without_rejecting_the_value() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        let key = vec![1; MAX_BTREE_KEY_BYTES];
        let inline = vec![2; MAX_BTREE_INLINE_ENTRY_BYTES - MAX_BTREE_KEY_BYTES];
        let spilled = vec![3; inline.len() + 1];
        {
            let mut transaction = pager.begin_write().unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, &key, &inline).unwrap();
            let node = Node::decode(
                transaction.read_page(root).unwrap(),
                TREE,
                transaction.generation().unwrap(),
                true,
            )
            .unwrap();
            let NodeKind::Leaf(entries) = node.kind else {
                panic!("test expected leaf");
            };
            assert!(matches!(entries[0].value, LeafValue::Inline(_)));
            root = Btree::upsert(&mut transaction, root, TREE, &key, &spilled).unwrap();
            let node = Node::decode(
                transaction.read_page(root).unwrap(),
                TREE,
                transaction.generation().unwrap(),
                true,
            )
            .unwrap();
            let NodeKind::Leaf(entries) = node.kind else {
                panic!("test expected leaf");
            };
            assert!(matches!(entries[0].value, LeafValue::Overflow(_)));
            transaction.commit(2, 2, Some(root)).unwrap();
        }
        assert_eq!(
            Btree::get(&mut pager, root, TREE, &key).unwrap(),
            Some(spilled)
        );
    }

    #[test]
    fn overflow_replacements_reclaim_shared_and_candidate_chains() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        let large_a = vec![1; MAX_OVERFLOW_CHUNK_BYTES * 3 + 17];
        let large_b = vec![2; MAX_OVERFLOW_CHUNK_BYTES * 2 + 29];
        let large_c = vec![3; MAX_OVERFLOW_CHUNK_BYTES + 7];

        root = upsert_and_commit(&mut pager, root, 2, b"value", &large_a);
        let first_a = overflow_descriptor_from_root(&mut pager, root, b"value").first_page_id;
        let before_replace = pager.active_metadata().superblock.live_data_page_count;
        root = upsert_and_commit(&mut pager, root, 3, b"value", b"small");
        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"value").unwrap(),
            Some(b"small".to_vec())
        );
        assert_eq!(
            pager.read_page(first_a).unwrap_err().code,
            "PAGE_NOT_ALLOCATED"
        );
        assert!(pager.active_metadata().superblock.live_data_page_count < before_replace);

        root = upsert_and_commit(&mut pager, root, 4, b"value", &large_b);
        let first_b = overflow_descriptor_from_root(&mut pager, root, b"value").first_page_id;
        root = upsert_and_commit(&mut pager, root, 5, b"value", &large_c);
        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"value").unwrap(),
            Some(large_c.clone())
        );
        assert_eq!(
            pager.read_page(first_b).unwrap_err().code,
            "PAGE_NOT_ALLOCATED"
        );

        let before_same_transaction = pager.active_metadata().superblock.live_data_page_count;
        {
            let mut transaction = pager.begin_write().unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", &large_a).unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", b"tiny").unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", &large_b).unwrap();
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", &large_c).unwrap();
            transaction.commit(6, 6, Some(root)).unwrap();
        }
        assert_eq!(
            pager.active_metadata().superblock.live_data_page_count,
            before_same_transaction + large_c.len().div_ceil(MAX_OVERFLOW_CHUNK_BYTES) as u32
        );
        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"same-tx").unwrap(),
            Some(large_c)
        );
    }

    #[test]
    fn overflow_allocation_failure_poisons_and_can_be_aborted() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let root = create_tree(&mut pager);
        let only_free_page = MAX_PAGE_COUNT - 1;
        let mut pager = into_sparse_pager_with_free_pages(pager, &[only_free_page]);
        let mut transaction = pager.begin_write().unwrap();
        assert_eq!(
            Btree::upsert(
                &mut transaction,
                root,
                TREE,
                b"large",
                &vec![7; MAX_OVERFLOW_CHUNK_BYTES + 1]
            )
            .unwrap_err()
            .code,
            "DATABASE_FULL"
        );
        assert_eq!(
            transaction.generation().unwrap_err().code,
            "TRANSACTION_FAILED"
        );
        transaction.abort();
        assert_eq!(Btree::get(&mut pager, root, TREE, b"large").unwrap(), None);
        let mut retry = pager.begin_write().unwrap();
        assert_eq!(retry.allocate_page().unwrap(), only_free_page);
        retry.abort();
    }

    #[test]
    fn cursor_is_detached_and_detects_publication() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let root = create_tree(&mut pager);
        let mut cursor = Btree::cursor(&mut pager, root, TREE).unwrap();

        let mut transaction = pager.begin_write().unwrap();
        let next_root = Btree::upsert(&mut transaction, root, TREE, b"a", b"b").unwrap();
        transaction.commit(2, 2, Some(next_root)).unwrap();
        assert_eq!(
            cursor.next(&mut pager).unwrap_err().code,
            "CURSOR_INVALIDATED"
        );
    }

    #[test]
    fn failed_cow_path_cannot_publish_a_bitmap_with_a_live_child_freed() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in 0..10 {
                root = Btree::upsert(&mut transaction, root, TREE, &key(number), &value(number))
                    .unwrap();
            }
            transaction.commit(2, 2, Some(root)).unwrap();
        }
        assert!(
            Node::decode(
                pager.read_page(root).unwrap(),
                TREE,
                pager.generation(),
                false
            )
            .unwrap()
            .level
                > 0
        );

        let active = pager.active_metadata().clone();
        let mut memory = pager.into_device();
        let mut bitmap: AllocationBitmap = active.allocation_bitmap;
        for id in FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT {
            bitmap.set_allocated(id, true).unwrap();
        }
        bitmap.set_allocated(MAX_PAGE_COUNT - 1, false).unwrap();
        let mut superblock = active.superblock;
        superblock.live_data_page_count = (MAX_PAGE_COUNT - FIRST_DATA_PAGE_ID - 1) as u32;
        for (chunk, page) in bitmap.encode_pages().unwrap().iter().enumerate() {
            memory
                .write_page(superblock.bitmap_slot.page_id(chunk), page)
                .unwrap();
        }
        memory
            .write_page(
                superblock.slot.page_id(),
                &superblock.encode_page().unwrap(),
            )
            .unwrap();

        let physical_count = memory.page_count();
        let mut pages = HashMap::new();
        for id in 0..physical_count {
            let mut bytes = [0; crate::PAGE_SIZE];
            memory.read_page(id, &mut bytes).unwrap();
            pages.insert(id, bytes);
        }
        let mut pager = Pager::open_or_create(SparseDevice { pages }).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        assert_eq!(
            Btree::upsert(&mut transaction, root, TREE, &key(9), b"new")
                .unwrap_err()
                .code,
            "DATABASE_FULL"
        );
        assert_eq!(
            transaction.commit(3, 3, Some(root)).unwrap_err().code,
            "TRANSACTION_FAILED"
        );
        assert_eq!(
            Btree::get(&mut pager, root, TREE, &key(9)).unwrap(),
            Some(value(9))
        );
    }

    #[test]
    fn rejects_argument_bounds_before_allocating() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let root = create_tree(&mut pager);
        let page_count = pager.physical_page_count();
        let mut transaction = pager.begin_write().unwrap();
        assert_eq!(
            Btree::upsert(
                &mut transaction,
                root,
                TREE,
                &vec![0; MAX_BTREE_KEY_BYTES + 1],
                b"v"
            )
            .unwrap_err()
            .code,
            "BTREE_LIMIT"
        );
        assert_eq!(
            Btree::upsert(
                &mut transaction,
                root,
                TREE,
                b"k",
                &vec![0; MAX_BTREE_VALUE_BYTES + 1]
            )
            .unwrap_err()
            .code,
            "BTREE_LIMIT"
        );
        transaction.abort();
        assert_eq!(pager.physical_page_count(), page_count);
    }

    #[test]
    fn codec_rejects_corrupt_bounds_order_tree_generation_and_type() {
        let entries = vec![
            LeafEntry {
                key: b"a".to_vec(),
                value: LeafValue::Inline(b"1".to_vec()),
            },
            LeafEntry {
                key: b"b".to_vec(),
                value: LeafValue::Inline(b"2".to_vec()),
            },
        ];
        let page = Node::leaf(TREE, 2, entries.clone())
            .encode(FIRST_DATA_PAGE_ID)
            .unwrap();
        assert_eq!(
            Node::decode(page.clone(), TREE + 1, 2, false)
                .unwrap_err()
                .code,
            "INVALID_BTREE_PAGE"
        );
        assert_eq!(
            Node::decode(page.clone(), TREE, 1, false).unwrap_err().code,
            "INVALID_BTREE_PAGE"
        );

        let mut bad_bounds = page.clone();
        bad_bounds.payload[26..28].copy_from_slice(&41_u16.to_le_bytes());
        assert_eq!(
            Node::decode(bad_bounds, TREE, 2, false).unwrap_err().code,
            "INVALID_BTREE_PAGE"
        );

        let mut overlapping = page.clone();
        let first = read_u16(&overlapping.payload, NODE_HEADER_SIZE);
        overlapping.payload[NODE_HEADER_SIZE + 2..NODE_HEADER_SIZE + 4]
            .copy_from_slice(&first.to_le_bytes());
        assert_eq!(
            Node::decode(overlapping, TREE, 2, false).unwrap_err().code,
            "INVALID_BTREE_PAGE"
        );

        let duplicate = Node::leaf(TREE, 2, vec![entries[0].clone(), entries[0].clone()]);
        assert_eq!(
            duplicate.encode(FIRST_DATA_PAGE_ID).unwrap_err().code,
            "INVALID_BTREE_PAGE"
        );

        let mut wrong_type = page;
        wrong_type.page_type = PageType::BtreeInternal;
        assert_eq!(
            Node::decode(wrong_type, TREE, 2, false).unwrap_err().code,
            "INVALID_BTREE_PAGE"
        );
    }

    #[test]
    fn page_envelope_crc_catches_payload_corruption() {
        let page = Node::leaf(
            TREE,
            2,
            vec![LeafEntry {
                key: b"a".to_vec(),
                value: LeafValue::Inline(b"b".to_vec()),
            }],
        )
        .encode(FIRST_DATA_PAGE_ID)
        .unwrap();
        let mut bytes = page.encode().unwrap();
        bytes[100] ^= 1;
        assert_eq!(Page::decode(&bytes).unwrap_err().code, "INVALID_PAGE");
    }

    #[test]
    fn overflow_codec_rejects_cycle_truncation_position_tree_generation_and_checksum() {
        let value = vec![9; MAX_OVERFLOW_CHUNK_BYTES + 13];
        let checksum = crc32(&value);
        let descriptor = OverflowDescriptor {
            first_page_id: FIRST_DATA_PAGE_ID,
            generation: 2,
            total_length: value.len() as u32,
            checksum,
        };
        let first = OverflowPage {
            tree_id: TREE,
            generation: 2,
            chunk_index: 0,
            chunk_count: 2,
            next_page_id: Some(FIRST_DATA_PAGE_ID + 1),
            total_length: value.len() as u32,
            checksum,
            chunk: value[..MAX_OVERFLOW_CHUNK_BYTES].to_vec(),
        };
        let second = OverflowPage {
            tree_id: TREE,
            generation: 2,
            chunk_index: 1,
            chunk_count: 2,
            next_page_id: None,
            total_length: value.len() as u32,
            checksum,
            chunk: value[MAX_OVERFLOW_CHUNK_BYTES..].to_vec(),
        };
        let pages = HashMap::from([
            (
                FIRST_DATA_PAGE_ID,
                first.encode(FIRST_DATA_PAGE_ID).unwrap(),
            ),
            (
                FIRST_DATA_PAGE_ID + 1,
                second.encode(FIRST_DATA_PAGE_ID + 1).unwrap(),
            ),
        ]);
        assert_eq!(
            read_overflow_chain(
                |id| Ok(pages.get(&id).unwrap().clone()),
                TREE,
                2,
                2,
                &descriptor
            )
            .unwrap()
            .0,
            value
        );

        let mut bad_checksum = descriptor.clone();
        bad_checksum.checksum ^= 1;
        assert_eq!(
            read_overflow_chain(
                |id| Ok(pages.get(&id).unwrap().clone()),
                TREE,
                2,
                2,
                &bad_checksum
            )
            .unwrap_err()
            .code,
            "INVALID_OVERFLOW_PAGE"
        );

        let mut cycle_first = first.clone();
        cycle_first.next_page_id = Some(FIRST_DATA_PAGE_ID);
        let cycle_page = cycle_first.encode(FIRST_DATA_PAGE_ID).unwrap_err();
        assert_eq!(cycle_page.code, "INVALID_OVERFLOW_PAGE");

        let mut wrong_tree = first.clone();
        wrong_tree.tree_id += 1;
        assert_eq!(
            OverflowPage::decode(
                wrong_tree.encode(FIRST_DATA_PAGE_ID).unwrap(),
                TREE,
                &descriptor,
                0,
                2
            )
            .unwrap_err()
            .code,
            "INVALID_OVERFLOW_PAGE"
        );
        let mut wrong_generation = first.clone();
        wrong_generation.generation += 1;
        assert_eq!(
            OverflowPage::decode(
                wrong_generation.encode(FIRST_DATA_PAGE_ID).unwrap(),
                TREE,
                &descriptor,
                0,
                2
            )
            .unwrap_err()
            .code,
            "INVALID_OVERFLOW_PAGE"
        );
        assert_eq!(
            OverflowPage::decode(
                second.encode(FIRST_DATA_PAGE_ID + 1).unwrap(),
                TREE,
                &descriptor,
                0,
                2
            )
            .unwrap_err()
            .code,
            "INVALID_OVERFLOW_PAGE"
        );
        let truncated = Page::new(
            FIRST_DATA_PAGE_ID,
            PageType::Overflow,
            vec![0; OVERFLOW_HEADER_SIZE - 1],
        )
        .unwrap();
        assert_eq!(
            OverflowPage::decode(truncated, TREE, &descriptor, 0, 2)
                .unwrap_err()
                .code,
            "INVALID_OVERFLOW_PAGE"
        );
    }

    #[test]
    fn get_and_cursor_reject_corrupt_overflow_data_and_cycles_after_reopen() {
        fn committed_overflow() -> (MemoryPageDevice, PageId, OverflowDescriptor) {
            let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
            let root = create_tree(&mut pager);
            let root = upsert_and_commit(
                &mut pager,
                root,
                2,
                b"large",
                &vec![5; MAX_OVERFLOW_CHUNK_BYTES * 2 + 1],
            );
            let descriptor = overflow_descriptor_from_root(&mut pager, root, b"large");
            (pager.into_device(), root, descriptor)
        }

        let (mut device, root, descriptor) = committed_overflow();
        let mut overflow = Page::decode(device.page(descriptor.first_page_id).unwrap()).unwrap();
        overflow.payload[OVERFLOW_HEADER_SIZE] ^= 1;
        device
            .write_page(descriptor.first_page_id, &overflow.encode().unwrap())
            .unwrap();
        let mut pager = Pager::open_or_create(device).unwrap();
        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"large")
                .unwrap_err()
                .code,
            "INVALID_OVERFLOW_PAGE"
        );
        let mut cursor = Btree::cursor(&mut pager, root, TREE).unwrap();
        assert_eq!(
            cursor.next(&mut pager).unwrap_err().code,
            "INVALID_OVERFLOW_PAGE"
        );
        assert_eq!(
            cursor.next(&mut pager).unwrap_err().code,
            "INVALID_OVERFLOW_PAGE"
        );

        let (mut device, root, descriptor) = committed_overflow();
        let mut overflow = Page::decode(device.page(descriptor.first_page_id).unwrap()).unwrap();
        overflow.payload[32..40].copy_from_slice(&descriptor.first_page_id.to_le_bytes());
        device
            .write_page(descriptor.first_page_id, &overflow.encode().unwrap())
            .unwrap();
        let mut pager = Pager::open_or_create(device).unwrap();
        assert_eq!(
            Btree::get(&mut pager, root, TREE, b"large")
                .unwrap_err()
                .code,
            "INVALID_OVERFLOW_PAGE"
        );
    }

    #[test]
    fn self_referential_internal_page_fails_closed() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let root = create_tree(&mut pager);
        let generation = pager.generation();
        let mut device = pager.into_device();
        let corrupt = Node::internal(
            TREE,
            generation,
            1,
            root,
            vec![InternalEntry {
                key: b"m".to_vec(),
                right_child: root + 1,
            }],
        );
        // The encoder itself refuses a direct self-reference, before corrupt bytes can reach disk.
        assert_eq!(corrupt.encode(root).unwrap_err().code, "INVALID_BTREE_PAGE");

        // Preserve use of the PageDevice import and prove an arbitrary invalid envelope cannot be
        // mistaken for a tree page after reopening.
        device.write_page(root, &[0; crate::PAGE_SIZE]).unwrap();
        let mut reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(
            Btree::get(&mut reopened, root, TREE, b"m")
                .unwrap_err()
                .code,
            "INVALID_PAGE"
        );
    }

    #[test]
    fn rejects_duplicate_children_and_wrong_child_levels() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let duplicate_root;
        {
            let mut transaction = pager.begin_write().unwrap();
            let generation = transaction.generation().unwrap();
            let left = transaction.allocate_page().unwrap();
            let right = transaction.allocate_page().unwrap();
            duplicate_root = transaction.allocate_page().unwrap();
            write_node(
                &mut transaction,
                left,
                &Node::leaf(TREE, generation, Vec::new()),
            )
            .unwrap();
            write_node(
                &mut transaction,
                right,
                &Node::leaf(TREE, generation, Vec::new()),
            )
            .unwrap();
            let mut page = Node::internal(
                TREE,
                generation,
                1,
                left,
                vec![InternalEntry {
                    key: b"m".to_vec(),
                    right_child: right,
                }],
            )
            .encode(duplicate_root)
            .unwrap();
            let cell_offset = read_u16(&page.payload, NODE_HEADER_SIZE) as usize;
            page.payload[cell_offset + 4..cell_offset + 12].copy_from_slice(&left.to_le_bytes());
            transaction.write_new_page(&page).unwrap();
            transaction.commit(1, 1, Some(duplicate_root)).unwrap();
        }
        assert_eq!(
            Btree::cursor(&mut pager, duplicate_root, TREE)
                .unwrap_err()
                .code,
            "INVALID_BTREE_PAGE"
        );

        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let wrong_level_root;
        {
            let mut transaction = pager.begin_write().unwrap();
            let generation = transaction.generation().unwrap();
            let left = transaction.allocate_page().unwrap();
            let right = transaction.allocate_page().unwrap();
            wrong_level_root = transaction.allocate_page().unwrap();
            write_node(
                &mut transaction,
                left,
                &Node::leaf(TREE, generation, Vec::new()),
            )
            .unwrap();
            write_node(
                &mut transaction,
                right,
                &Node::leaf(TREE, generation, Vec::new()),
            )
            .unwrap();
            write_node(
                &mut transaction,
                wrong_level_root,
                &Node::internal(
                    TREE,
                    generation,
                    2,
                    left,
                    vec![InternalEntry {
                        key: b"m".to_vec(),
                        right_child: right,
                    }],
                ),
            )
            .unwrap();
            transaction.commit(1, 1, Some(wrong_level_root)).unwrap();
        }
        assert_eq!(
            Btree::get(&mut pager, wrong_level_root, TREE, b"a")
                .unwrap_err()
                .code,
            "INVALID_BTREE_PAGE"
        );
    }
}
