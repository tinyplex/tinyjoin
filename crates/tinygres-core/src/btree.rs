use std::collections::HashSet;

use crate::{
    EngineError, FIRST_DATA_PAGE_ID, MAX_PAGE_COUNT, MAX_PAGE_PAYLOAD_SIZE, Page, PageDevice,
    PageId, PageType, Pager, PagerWriteTransaction, Result,
};

pub type TreeId = u64;

pub const MAX_BTREE_KEY_BYTES: usize = 1_024;
pub const MAX_BTREE_INLINE_VALUE_BYTES: usize = 1_024;
pub const MAX_BTREE_INLINE_ENTRY_BYTES: usize = 1_536;

const NODE_MAGIC: &[u8; 4] = b"TGBT";
const NODE_FORMAT_VERSION: u16 = 1;
const NODE_FLAGS: u8 = 0;
const NODE_HEADER_SIZE: usize = 40;
const SLOT_SIZE: usize = 2;
const LEAF_CELL_HEADER_SIZE: usize = 8;
const INTERNAL_CELL_HEADER_SIZE: usize = 12;
const CELL_FLAGS: u16 = 0;
const NO_PAGE_ID: PageId = u64::MAX;
const MAX_TREE_DEPTH: usize = 64;

/// A versioned, lexicographically ordered B-tree stored in pager data pages.
///
/// Keys and values are opaque bytes. SQL-aware sortable encodings and overflow values belong to
/// the storage layer above this primitive. This first bounded slice stores values inline only.
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
            match node.kind {
                NodeKind::Leaf(entries) => {
                    return Ok(entries
                        .binary_search_by(|entry| entry.key.as_slice().cmp(key))
                        .ok()
                        .map(|index| entries[index].value.clone()));
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
        validate_entry(key, value)?;
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
                self.leaf_index += 1;
                return Ok(Some((entry.key.clone(), entry.value.clone())));
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
    value: Vec<u8>,
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
                validate_sorted_leaf_entries(entries)?;
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
                if internal.entries.is_empty() {
                    return Err(invalid_btree("An internal node must contain a separator"));
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
                    let (entry, cell_end) = decode_leaf_cell(bytes, offset)?;
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
                validate_sorted_leaf_entries(&entries).map_err(as_corruption)?;
                NodeKind::Leaf(entries)
            }
            PageType::BtreeInternal => {
                if level == 0 || item_count == 0 {
                    return Err(invalid_btree(format!(
                        "B-tree internal page {} has no level or separators",
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
                size.checked_add(LEAF_CELL_HEADER_SIZE + entry.key.len() + entry.value.len())
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
            match entries.binary_search_by(|entry| entry.key.as_slice().cmp(key)) {
                Ok(index) => entries[index].value = value.to_vec(),
                Err(index) => entries.insert(
                    index,
                    LeafEntry {
                        key: key.to_vec(),
                        value: value.to_vec(),
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

fn encode_leaf_cell(entry: &LeafEntry) -> Result<Vec<u8>> {
    validate_entry(&entry.key, &entry.value)?;
    let mut cell = Vec::with_capacity(LEAF_CELL_HEADER_SIZE + entry.key.len() + entry.value.len());
    cell.extend_from_slice(&(entry.key.len() as u16).to_le_bytes());
    cell.extend_from_slice(&CELL_FLAGS.to_le_bytes());
    cell.extend_from_slice(&(entry.value.len() as u32).to_le_bytes());
    cell.extend_from_slice(&entry.key);
    cell.extend_from_slice(&entry.value);
    Ok(cell)
}

fn encode_internal_cell(entry: &InternalEntry) -> Result<Vec<u8>> {
    validate_key(&entry.key)?;
    let mut cell = Vec::with_capacity(INTERNAL_CELL_HEADER_SIZE + entry.key.len());
    cell.extend_from_slice(&(entry.key.len() as u16).to_le_bytes());
    cell.extend_from_slice(&CELL_FLAGS.to_le_bytes());
    cell.extend_from_slice(&entry.right_child.to_le_bytes());
    cell.extend_from_slice(&entry.key);
    Ok(cell)
}

fn decode_leaf_cell(bytes: &[u8], offset: usize) -> Result<(LeafEntry, usize)> {
    let header_end = checked_end(offset, LEAF_CELL_HEADER_SIZE, bytes.len())?;
    let key_length = read_u16(bytes, offset) as usize;
    let flags = read_u16(bytes, offset + 2);
    if flags != CELL_FLAGS {
        return Err(unsupported_btree(format!(
            "Leaf cell flags {flags:#06x} are not supported"
        )));
    }
    let value_length = read_u32(bytes, offset + 4) as usize;
    let key_end = checked_end(header_end, key_length, bytes.len())?;
    let value_end = checked_end(key_end, value_length, bytes.len())?;
    validate_decoded_entry(key_length, value_length)?;
    Ok((
        LeafEntry {
            key: bytes[header_end..key_end].to_vec(),
            value: bytes[key_end..value_end].to_vec(),
        },
        value_end,
    ))
}

fn decode_internal_cell(bytes: &[u8], offset: usize) -> Result<(InternalEntry, usize)> {
    let header_end = checked_end(offset, INTERNAL_CELL_HEADER_SIZE, bytes.len())?;
    let key_length = read_u16(bytes, offset) as usize;
    let flags = read_u16(bytes, offset + 2);
    if flags != CELL_FLAGS {
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

fn validate_sorted_leaf_entries(entries: &[LeafEntry]) -> Result<()> {
    for entry in entries {
        validate_entry(&entry.key, &entry.value)?;
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

fn validate_entry(key: &[u8], value: &[u8]) -> Result<()> {
    validate_key(key)?;
    if value.len() > MAX_BTREE_INLINE_VALUE_BYTES {
        return Err(limit_error(format!(
            "Inline B-tree value is {} bytes, exceeding {MAX_BTREE_INLINE_VALUE_BYTES}; overflow pages are not implemented yet",
            value.len()
        )));
    }
    if key.len() + value.len() > MAX_BTREE_INLINE_ENTRY_BYTES {
        return Err(limit_error(format!(
            "Inline B-tree key and value total {} bytes, exceeding {MAX_BTREE_INLINE_ENTRY_BYTES}",
            key.len() + value.len()
        )));
    }
    Ok(())
}

fn validate_decoded_entry(key_length: usize, value_length: usize) -> Result<()> {
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
                &vec![0; MAX_BTREE_INLINE_VALUE_BYTES + 1]
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
                &vec![0; MAX_BTREE_KEY_BYTES],
                &vec![0; MAX_BTREE_INLINE_ENTRY_BYTES - MAX_BTREE_KEY_BYTES + 1]
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
                value: b"1".to_vec(),
            },
            LeafEntry {
                key: b"b".to_vec(),
                value: b"2".to_vec(),
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
                value: b"b".to_vec(),
            }],
        )
        .encode(FIRST_DATA_PAGE_ID)
        .unwrap();
        let mut bytes = page.encode().unwrap();
        bytes[100] ^= 1;
        assert_eq!(Page::decode(&bytes).unwrap_err().code, "INVALID_PAGE");
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
