use std::{borrow::Cow, cmp::Ordering, ops::Range};

use crate::{
    CandidateId, EngineError, FIRST_DATA_PAGE_ID, MAX_PAGE_COUNT, MAX_PAGE_PAYLOAD_SIZE, Page,
    PageDevice, PageId, PageRef, PageType, Pager, PagerWriteTransaction, Result,
    cache::PageSet,
    checksum::{checksum32, xxh64},
    hash::{EMPTY_HASH, combine},
};

// Page diagnostics are intentionally compact in the browser build. The stable
// error code still distinguishes invalid, unsupported, overflow, and limit
// failures; native builds retain the detailed page IDs and offsets used while
// debugging storage internals.
#[cfg(all(target_arch = "wasm32", feature = "compact-storage-diagnostics"))]
macro_rules! storage_diagnostic {
    ($($argument:tt)*) => {{
        if false {
            let _ = ::std::format!($($argument)*);
        }
        String::from("B-tree validation failed")
    }};
}

#[cfg(not(all(target_arch = "wasm32", feature = "compact-storage-diagnostics")))]
macro_rules! storage_diagnostic {
    ($($argument:tt)*) => {
        ::std::format!($($argument)*)
    };
}

pub(crate) type TreeId = u64;

pub(crate) const MAX_BTREE_KEY_BYTES: usize = 1_024;
pub(crate) const MAX_BTREE_INLINE_VALUE_BYTES: usize = 1_024;
pub(crate) const MAX_BTREE_INLINE_ENTRY_BYTES: usize = 1_536;
pub(crate) const MAX_BTREE_VALUE_BYTES: usize = 1024 * 1024;

const NODE_MAGIC: &[u8; 4] = b"TGBT";
const NODE_FORMAT_VERSION: u16 = 2;
const NODE_FLAGS: u8 = 0;
const NODE_HEADER_SIZE: usize = 48;
const SLOT_SIZE: usize = 2;
/// The bytes of a node's payload that its slots and cells share.
const NODE_CAPACITY: usize = MAX_PAGE_PAYLOAD_SIZE - NODE_HEADER_SIZE;
const LEAF_CELL_HEADER_SIZE: usize = 8;
const OVERFLOW_DESCRIPTOR_SIZE: usize = 24;
const INTERNAL_CELL_HEADER_SIZE: usize = 20;
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
pub(crate) struct Btree;

/// The outcome of one [`Btree::upsert`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BtreeUpsert {
    pub(crate) root_page_id: PageId,
    /// The fingerprint of every entry in the tree after the insertion.
    pub(crate) hash: u64,
}

/// The outcome of one [`Btree::delete`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BtreeDelete {
    /// The candidate root, or `None` once the last entry has been removed.
    pub(crate) root_page_id: Option<PageId>,
    /// The fingerprint of every remaining entry, or `None` when no entry matched. A caller which
    /// records tree fingerprints must leave the one it already holds in place in that case.
    pub(crate) hash: Option<u64>,
    pub(crate) removed: bool,
}

/// One change in a [`Btree::apply`] batch: `value` upserts `key`, and `None` deletes it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BatchChange<'a> {
    pub(crate) key: &'a [u8],
    pub(crate) value: Option<&'a [u8]>,
}

/// The outcome of one [`Btree::apply`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BtreeBatch {
    /// The candidate root, or `None` once the tree holds no entry.
    pub(crate) root_page_id: Option<PageId>,
    /// The fingerprint of every entry in the tree, or `None` when the batch changed nothing. A
    /// caller which records tree fingerprints must leave the one it already holds in place then.
    pub(crate) hash: Option<u64>,
    /// How many upserts added a key the tree did not hold.
    pub(crate) inserted: usize,
    /// How many deletes removed a key the tree held.
    pub(crate) removed: usize,
}

impl Btree {
    /// Creates an empty leaf and returns its candidate root page ID.
    ///
    /// No fingerprint is reported: an empty tree always has [`EMPTY_HASH`].
    #[cfg(test)]
    pub(crate) fn create<D: PageDevice>(
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
            transaction.mark_btree_mutated(tree_id)?;
            Ok(root)
        })();
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }

    /// Looks up one exact key in a committed root.
    pub(crate) fn get<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        get_from(pager, root_page_id, tree_id, key)
    }

    /// Looks up one exact key in a committed root, as [`Self::get`] does, and returns the key
    /// followed by its value: the entry whole, as a reader that keeps a row holds it.
    pub(crate) fn get_entry<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        get_prefixed_from(pager, root_page_id, tree_id, key, key)
    }

    /// Inserts or replaces an inline value and returns the candidate root and its fingerprint.
    /// Commits write through [`Self::apply`]; tests build trees one change at a time with this and
    /// [`Self::delete`], and check batches against them.
    ///
    /// On error, the pager transaction is marked failed and must be aborted. Allocation or device
    /// failures may have left unreachable candidate pages which must not be published as part of
    /// another operation.
    #[cfg(test)]
    pub(crate) fn upsert<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
        key: &[u8],
        value: &[u8],
    ) -> Result<BtreeUpsert> {
        validate_tree_id(tree_id)?;
        validate_key(key)?;
        validate_value(value)?;
        let result = (|| {
            let generation = transaction.generation()?;
            let mut visited = PageSet::default();
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
            let (root_page_id, hash) = if let Some(split) = inserted.split {
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
                    inserted.hash,
                    vec![InternalEntry {
                        key: split.separator,
                        right_child: split.right_page_id,
                        child_hash: split.right_hash,
                    }],
                );
                let hash = write_node(transaction, new_root, &root)?;
                (new_root, hash)
            } else {
                (inserted.page_id, inserted.hash)
            };
            transaction.mark_btree_mutated(tree_id)?;
            Ok(BtreeUpsert { root_page_id, hash })
        })();
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }

    /// Removes one exact key and reports the candidate root, its fingerprint, and whether an
    /// entry existed.
    ///
    /// Deletion is copy-on-write and deliberately does not rebalance under-full pages. Empty
    /// descendants are pruned; an internal page may retain one child and no separator, and an
    /// empty tree is represented by `None`. Separator keys are advanced when the first key in a
    /// right subtree changes. On error, the pager transaction is marked failed and must be aborted.
    #[cfg(test)]
    pub(crate) fn delete<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
        key: &[u8],
    ) -> Result<BtreeDelete> {
        validate_tree_id(tree_id)?;
        validate_key(key)?;
        let result = (|| {
            let generation = transaction.generation()?;
            let mut visited = PageSet::default();
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
            if deleted.removed {
                transaction.mark_btree_mutated(tree_id)?;
            }
            Ok(BtreeDelete {
                root_page_id: deleted.page_id,
                hash: deleted.hash,
                removed: deleted.removed,
            })
        })();
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }

    /// Applies a batch of changes, sorted by strictly increasing key, in one pass over the tree.
    ///
    /// Each page the batch reaches is read and written once, however many of its changes land on
    /// it, and a page that overflows is divided into as many pages as it needs at once. A page that
    /// takes only appends past its last key is filled before the next is started, so tables
    /// loaded in key order keep full pages; otherwise its entries are spread evenly. Emptied pages
    /// are pruned, and pages are not rebalanced. An upsert of the value an entry already holds
    /// inline leaves its page as it was. A missing root starts an empty tree. On error, the pager
    /// transaction is marked failed and must be aborted.
    pub(crate) fn apply<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: Option<PageId>,
        tree_id: TreeId,
        changes: &[BatchChange<'_>],
    ) -> Result<BtreeBatch> {
        Self::apply_as(
            transaction,
            root_page_id,
            tree_id,
            changes,
            #[cfg(test)]
            false,
        )
    }

    /// Applies a batch as [`Self::apply`] does, but rewrites each leaf cell by cell, as
    /// [`Self::apply`] did before it kept cells in runs. A test applies one batch both ways and
    /// compares what they report and every page they leave.
    #[cfg(test)]
    pub(crate) fn apply_by_cell<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: Option<PageId>,
        tree_id: TreeId,
        changes: &[BatchChange<'_>],
    ) -> Result<BtreeBatch> {
        Self::apply_as(transaction, root_page_id, tree_id, changes, true)
    }

    /// [`Self::apply`], which a test can make rewrite leaves cell by cell.
    fn apply_as<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: Option<PageId>,
        tree_id: TreeId,
        changes: &[BatchChange<'_>],
        #[cfg(test)] by_cell: bool,
    ) -> Result<BtreeBatch> {
        validate_tree_id(tree_id)?;
        if changes.is_empty() {
            return Ok(BtreeBatch {
                root_page_id,
                hash: None,
                inserted: 0,
                removed: 0,
            });
        }
        for (index, change) in changes.iter().enumerate() {
            validate_key(change.key)?;
            if let Some(value) = change.value {
                validate_value(value)?;
            }
            if index > 0 && changes[index - 1].key >= change.key {
                return Err(EngineError::new(
                    "INVALID_PAGED_ARGUMENT",
                    "A B-tree batch must be sorted by strictly increasing key",
                ));
            }
        }
        let result = (|| {
            let mut batch = BatchWriter {
                transaction: &mut *transaction,
                tree_id,
                generation: 0,
                visited: PageSet::default(),
                inserted: 0,
                removed: 0,
                #[cfg(test)]
                by_cell,
            };
            batch.generation = batch.transaction.generation()?;
            let (pieces, level) = match root_page_id {
                Some(root) => match batch.apply(root, changes, 0, None, None, None)? {
                    None => {
                        return Ok(BtreeBatch {
                            root_page_id,
                            hash: None,
                            inserted: 0,
                            removed: 0,
                        });
                    }
                    Some(applied) => applied,
                },
                None => {
                    let mut cells = Vec::new();
                    for change in changes {
                        if let Some(value) = change.value {
                            let value = batch.new_value(change.key, value)?;
                            cells.push(LeafCell::New {
                                key: change.key,
                                value,
                            });
                        }
                    }
                    batch.inserted = cells.len();
                    if cells.is_empty() {
                        return Ok(BtreeBatch {
                            root_page_id: None,
                            hash: None,
                            inserted: 0,
                            removed: 0,
                        });
                    }
                    (batch.write_leaf_cells(None, None, &cells, true, None)?, 0)
                }
            };
            let (root_page_id, hash) = batch.root(pieces, level)?;
            let (inserted, removed) = (batch.inserted, batch.removed);
            transaction.mark_btree_mutated(tree_id)?;
            Ok(BtreeBatch {
                root_page_id,
                hash: Some(hash),
                inserted,
                removed,
            })
        })();
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }

    /// Opens a detached cursor at the first key in a committed tree.
    #[cfg(test)]
    pub(crate) fn cursor<D: PageDevice>(
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
    pub(crate) fn cursor_from<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
        lower_bound: &[u8],
    ) -> Result<BtreeCursor> {
        open_cursor(
            pager,
            root_page_id,
            tree_id,
            Some(lower_bound),
            false,
            false,
        )
    }

    #[cfg(test)]
    /// Opens a detached cursor which moves backward from the last key before `bound`, or from the
    /// last key of all.
    pub(crate) fn cursor_before<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
        bound: Option<&[u8]>,
    ) -> Result<BtreeCursor> {
        open_cursor(pager, root_page_id, tree_id, bound, true, false)
    }

    /// Opens a committed cursor which fully validates every node it reads, as opening a database
    /// requires. Ordinary cursors check only what reading safely needs.
    pub(crate) fn validating_cursor<D: PageDevice>(
        pager: &mut Pager<D>,
        root_page_id: PageId,
        tree_id: TreeId,
    ) -> Result<BtreeCursor> {
        open_cursor(pager, root_page_id, tree_id, None, false, true)
    }

    /// Opens a detached cursor over a tree visible to an open pager transaction.
    ///
    /// Unlike [`Self::cursor`], this can read both pages shared from the committed generation and
    /// pages owned by the candidate. This lets a schema operation stream one committed tree while
    /// it builds another without collecting the source rows. The cursor is tied to the exact
    /// transaction which opened it and must be advanced with
    /// [`BtreeCursor::next_in_transaction`].
    pub(crate) fn cursor_in_transaction<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
    ) -> Result<BtreeCursor> {
        Self::cursor_from_in_transaction(transaction, root_page_id, tree_id, &[])
    }

    /// Opens a transaction cursor at the first key greater than or equal to `lower_bound`.
    pub(crate) fn cursor_from_in_transaction<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
        lower_bound: &[u8],
    ) -> Result<BtreeCursor> {
        open_cursor(
            transaction,
            root_page_id,
            tree_id,
            Some(lower_bound),
            false,
            false,
        )
    }

    #[cfg(test)]
    /// [`Self::cursor_before`] for a tree visible to an open pager transaction.
    pub(crate) fn cursor_before_in_transaction<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
        bound: Option<&[u8]>,
    ) -> Result<BtreeCursor> {
        open_cursor(transaction, root_page_id, tree_id, bound, true, false)
    }

    /// Validates and atomically removes every page reachable from `root_page_id`.
    ///
    /// Node and overflow pages are all traversed before the candidate allocation bitmap is
    /// changed, so corruption cannot leave a partially reclaimed tree. The root may belong to the
    /// committed generation or to the current candidate. On error the transaction is failed and
    /// must be aborted, matching [`Self::upsert`] and [`Self::delete`].
    pub(crate) fn reclaim<D: PageDevice>(
        transaction: &mut PagerWriteTransaction<'_, D>,
        root_page_id: PageId,
        tree_id: TreeId,
    ) -> Result<()> {
        validate_tree_id(tree_id)?;
        let result = reclaim_tree(transaction, root_page_id, tree_id)
            .and_then(|()| transaction.mark_btree_mutated(tree_id));
        if result.is_err() {
            transaction.mark_failed();
        }
        result
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CursorView {
    Committed {
        generation: u64,
    },
    Candidate {
        generation: u64,
        candidate_id: CandidateId,
        tree_version: u64,
    },
}

impl CursorView {
    fn generation(self) -> u64 {
        match self {
            Self::Committed { generation } | Self::Candidate { generation, .. } => generation,
        }
    }
}

/// The pages a B-tree is read from: those committed, or those a transaction sees.
pub(crate) trait BtreeReadView {
    fn read_btree_page(&mut self, id: PageId) -> Result<Page>;
    fn read_btree_page_in_place(&mut self, id: PageId) -> Result<PageRef<'_>>;
    fn cursor_view(&self, tree_id: TreeId) -> Result<CursorView>;
    fn requires_view_generation(&self, _id: PageId) -> bool {
        false
    }
}

impl<D: PageDevice> BtreeReadView for Pager<D> {
    fn read_btree_page(&mut self, id: PageId) -> Result<Page> {
        self.read_page(id)
    }

    fn read_btree_page_in_place(&mut self, id: PageId) -> Result<PageRef<'_>> {
        self.read_page_in_place(id)
    }

    fn cursor_view(&self, _tree_id: TreeId) -> Result<CursorView> {
        Ok(CursorView::Committed {
            generation: self.generation(),
        })
    }
}

impl<D: PageDevice> BtreeReadView for PagerWriteTransaction<'_, D> {
    fn read_btree_page(&mut self, id: PageId) -> Result<Page> {
        self.read_page(id)
    }

    fn read_btree_page_in_place(&mut self, id: PageId) -> Result<PageRef<'_>> {
        self.read_page_in_place(id)
    }

    fn cursor_view(&self, tree_id: TreeId) -> Result<CursorView> {
        Ok(CursorView::Candidate {
            generation: self.generation()?,
            candidate_id: self.candidate_id(),
            tree_version: self.btree_version(tree_id)?,
        })
    }

    fn requires_view_generation(&self, id: PageId) -> bool {
        self.owns_page(id)
    }
}

/// The value `key` holds in the tree at `root_page_id`, if it holds one: what
/// [`get_prefixed_from`] returns with nothing to put in front of it. It is compiled into its
/// callers, since it only passes their arguments on.
#[inline(always)]
pub(crate) fn get_from(
    reader: &mut dyn BtreeReadView,
    root_page_id: PageId,
    tree_id: TreeId,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    get_prefixed_from(reader, root_page_id, tree_id, key, &[])
}

/// Descends from `root_page_id` to the one leaf that can hold `key`, reading each node in place in
/// the page cache, and returns `prefix` followed by the value the key holds, in one allocation of
/// exactly their length. A caller that keeps an entry as its key and then its value passes the
/// key as the prefix, so that the value is copied out of its page once, into the entry itself.
///
/// Each step down must reach exactly the next lower level and a page's level is fixed, so a cycle
/// is rejected without tracking visited pages.
pub(crate) fn get_prefixed_from(
    reader: &mut dyn BtreeReadView,
    root_page_id: PageId,
    tree_id: TreeId,
    key: &[u8],
    prefix: &[u8],
) -> Result<Option<Vec<u8>>> {
    validate_tree_id(tree_id)?;
    validate_key(key)?;
    let generation = reader.cursor_view(tree_id)?.generation();
    let mut page_id = root_page_id;
    let mut expected_level = None;
    let mut parent_generation = None;

    for _ in 0..MAX_TREE_DEPTH {
        let require_view_generation = reader.requires_view_generation(page_id);
        let node = NodeView::open_in_place(
            reader.read_btree_page_in_place(page_id)?,
            tree_id,
            generation,
            require_view_generation,
        )?;
        validate_expected_level(page_id, node.level, expected_level)?;
        validate_child_generation(page_id, node.generation, parent_generation)?;
        let Some((index, found)) = node.search(key) else {
            return Err(node.search_error(key));
        };
        if node.leaf {
            if !found {
                return Ok(None);
            }
            let value = match node.inline_leaf_cell(index) {
                Some((_, value)) => value,
                None => match node.leaf_cell(index)?.1 {
                    CellValue::Inline(value) => value,
                    CellValue::Overflow(descriptor) => {
                        let leaf_generation = node.generation;
                        let (value, _) = read_overflow_chain(
                            &mut |page_id| reader.read_btree_page(page_id),
                            tree_id,
                            generation,
                            leaf_generation,
                            &descriptor,
                        )?;
                        // A value read from its overflow pages is a vector already, which only a
                        // prefix has to be put in front of.
                        return Ok(Some(if prefix.is_empty() {
                            value
                        } else {
                            prefixed(prefix, &value)
                        }));
                    }
                },
            };
            return Ok(Some(prefixed(prefix, value)));
        }
        page_id = node.child(index)?;
        expected_level = Some(node.level - 1);
        parent_generation = Some(node.generation);
    }
    Err(invalid_btree(storage_diagnostic!(
        "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
    )))
}

/// `prefix` followed by `value`, in one allocation of exactly their length. It is compiled into
/// the lookup, which copies each value it finds in a leaf through it, once.
#[inline(always)]
fn prefixed(prefix: &[u8], value: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return value.to_vec();
    }
    let mut bytes = Vec::with_capacity(prefix.len() + value.len());
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(value);
    bytes
}

/// Opens a cursor moving forward from the first key at or after `bound`, or backward from the last
/// key before it. With no bound, it starts at the first or last key of all.
pub(crate) fn open_cursor(
    reader: &mut dyn BtreeReadView,
    root_page_id: PageId,
    tree_id: TreeId,
    bound: Option<&[u8]>,
    backward: bool,
    strict: bool,
) -> Result<BtreeCursor> {
    validate_tree_id(tree_id)?;
    if let Some(bound) = bound {
        validate_key(bound)?;
    }
    let view = reader.cursor_view(tree_id)?;
    let mut cursor = BtreeCursor {
        tree_id,
        root_page_id,
        view,
        path: Vec::new(),
        leaf: None,
        leaf_index: 0,
        finished: false,
        visited_pages: PageSet::default(),
        backward,
        strict,
    };
    cursor.seek(reader, bound)?;
    Ok(cursor)
}

/// A key and value read in place: slices of the cursor's current leaf, except for a value read
/// from an overflow chain.
pub(crate) type CursorEntry<'a> = (&'a [u8], Cow<'a, [u8]>);

/// Resumable cursor state which does not hold a pager borrow.
///
/// The cursor keeps a view of its current leaf and of every internal node above it, so moving to
/// the next leaf reads only that leaf, and entries are returned as slices of the leaf's page.
#[derive(Debug)]
pub(crate) struct BtreeCursor {
    tree_id: TreeId,
    root_page_id: PageId,
    view: CursorView,
    /// Internal nodes from the root down, each with the child index the cursor followed.
    path: Vec<(NodeView<'static>, usize)>,
    leaf: Option<NodeView<'static>>,
    /// How many of the leaf's entries lie before the cursor: the next entry moving forward, or one
    /// past it moving backward.
    leaf_index: usize,
    finished: bool,
    visited_pages: PageSet,
    /// Whether the cursor moves toward smaller keys.
    backward: bool,
    strict: bool,
}

impl BtreeCursor {
    /// Returns the next owned key/value pair, or `None` after the final leaf.
    pub(crate) fn next<D: PageDevice>(
        &mut self,
        pager: &mut Pager<D>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self
            .next_entry(pager)?
            .map(|(key, value)| (key.to_vec(), value.into_owned())))
    }

    /// Returns the next pair from a cursor opened by
    /// [`Btree::cursor_in_transaction`] or [`Btree::cursor_from_in_transaction`].
    pub(crate) fn next_in_transaction<D: PageDevice>(
        &mut self,
        transaction: &mut PagerWriteTransaction<'_, D>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(self
            .next_entry_in_transaction(transaction)?
            .map(|(key, value)| (key.to_vec(), value.into_owned())))
    }

    /// Returns the next pair without copying it: the key and an inline value are slices of the
    /// cursor's current leaf, valid until the cursor moves.
    pub(crate) fn next_entry<D: PageDevice>(
        &mut self,
        pager: &mut Pager<D>,
    ) -> Result<Option<CursorEntry<'_>>> {
        self.next_from(pager)
    }

    /// [`Self::next_entry`] for a cursor opened in a transaction.
    pub(crate) fn next_entry_in_transaction<D: PageDevice>(
        &mut self,
        transaction: &mut PagerWriteTransaction<'_, D>,
    ) -> Result<Option<CursorEntry<'_>>> {
        self.next_from(transaction)
    }

    pub(crate) fn next_from(
        &mut self,
        reader: &mut dyn BtreeReadView,
    ) -> Result<Option<CursorEntry<'_>>> {
        if !self.next_leaf_from(reader)? {
            return Ok(None);
        }
        self.next_in_leaf(&mut |page_id| reader.read_btree_page(page_id))
    }

    /// Moves on to the next leaf holding entries the cursor has not returned, unless the current
    /// one still holds some, and reports whether there is one. Its entries are then taken with
    /// [`Self::next_in_leaf`], or read in place from [`Self::leaf`]. The cursor's view is checked
    /// here, once for a leaf's entries, so whoever takes them must not change the tree in between,
    /// as a visitor of rows cannot.
    pub(crate) fn next_leaf_from(&mut self, reader: &mut dyn BtreeReadView) -> Result<bool> {
        if self.finished {
            return Ok(false);
        }
        self.ensure_view(reader)?;
        loop {
            let remaining = if self.backward {
                self.leaf_index > 0
            } else {
                self.leaf_index < self.leaf.as_ref().map_or(0, NodeView::len)
            };
            if remaining {
                return Ok(true);
            }
            if !self.advance_leaf(reader)? {
                self.finished = true;
                return Ok(false);
            }
        }
    }

    /// The next entry of the leaf [`Self::next_leaf_from`] moved to, or `None` once it holds no more.
    /// The key and an inline value are slices of the cursor's copy of the leaf, so only a value
    /// held in an overflow chain needs pages read, which `read` reads.
    pub(crate) fn next_in_leaf(
        &mut self,
        read: &mut dyn FnMut(PageId) -> Result<Page>,
    ) -> Result<Option<CursorEntry<'_>>> {
        let Some(leaf) = self.leaf.as_ref() else {
            return Ok(None);
        };
        let index = match self.backward {
            true if self.leaf_index > 0 => self.leaf_index - 1,
            false if self.leaf_index < leaf.len() => self.leaf_index,
            _ => return Ok(None),
        };
        // An entry that fails to read is not passed over: reading on reports the same error.
        let entry = read_cell(leaf, self.tree_id, self.view.generation(), index, read)?;
        self.leaf_index = if self.backward { index } else { index + 1 };
        Ok(Some(entry))
    }

    /// The leaf [`Self::next_leaf_from`] moved to, and how many of its entries lie before the
    /// cursor: the first the cursor has not returned moving forward, or one past it moving
    /// backward; for a loop that reads the leaf's entries in place and then
    /// [passes them](Self::skip_to). `None` before the first leaf or after the last.
    pub(crate) fn leaf(&self) -> Option<(&NodeView<'static>, usize)> {
        self.leaf.as_ref().map(|leaf| (leaf, self.leaf_index))
    }

    /// Sets how many of the current leaf's entries lie before the cursor, so that
    /// [`Self::next_leaf_from`] moves on once none remain: at the leaf's length moving forward,
    /// or at zero moving backward.
    pub(crate) fn skip_to(&mut self, index: usize) {
        self.leaf_index = index;
    }

    /// The entry at `index` of the current leaf, whose value `read` reads from its overflow pages
    /// when it has them.
    pub(crate) fn cell_at(
        &self,
        index: usize,
        read: &mut dyn FnMut(PageId) -> Result<Page>,
    ) -> Result<CursorEntry<'_>> {
        let leaf = self.leaf.as_ref().ok_or_else(|| {
            invalid_btree(storage_diagnostic!(
                "Tree {} has no leaf to read cell {index} from",
                self.tree_id
            ))
        })?;
        read_cell(leaf, self.tree_id, self.view.generation(), index, read)
    }

    /// Finds which of `keys`, in ascending order, the leaf [`Self::next_leaf_from`] moved to holds at or
    /// after the cursor, moving forward, and puts their indexes in `positions`. Each key at or
    /// below the leaf's last is taken from `keys`, since no later leaf can hold it.
    pub(crate) fn leaf_positions(
        &self,
        keys: &mut &[&[u8]],
        positions: &mut Vec<usize>,
    ) -> Result<()> {
        positions.clear();
        let Some(leaf) = self.leaf.as_ref().filter(|_| !keys.is_empty()) else {
            return Ok(());
        };
        let Some(last) = leaf.len().checked_sub(1) else {
            return Ok(());
        };
        let last = leaf.leaf_key(last)?;
        // Each key lies after the one before it, so its search starts there.
        let mut low = self.leaf_index;
        while let Some((key, rest)) = keys.split_first()
            && *key <= last
        {
            low = leaf.lower_bound_from(low, key)?;
            if leaf.leaf_key(low)? == *key {
                positions.push(low);
                low += 1;
            }
            *keys = rest;
        }
        Ok(())
    }

    fn load(
        &mut self,
        reader: &mut dyn BtreeReadView,
        page_id: PageId,
        expected_level: Option<u8>,
        parent_generation: Option<u64>,
    ) -> Result<NodeView<'static>> {
        if !self.visited_pages.insert(page_id) {
            return Err(invalid_btree(storage_diagnostic!(
                "Tree {} contains a cycle through page {page_id}",
                self.tree_id
            )));
        }
        let node = NodeView::open(
            reader.read_btree_page(page_id)?,
            self.tree_id,
            self.view.generation(),
            reader.requires_view_generation(page_id),
            self.strict,
        )?;
        validate_expected_level(page_id, node.level, expected_level)?;
        validate_child_generation(page_id, node.generation, parent_generation)?;
        Ok(node)
    }

    fn seek(&mut self, reader: &mut dyn BtreeReadView, bound: Option<&[u8]>) -> Result<()> {
        self.ensure_view(reader)?;
        self.path.clear();
        self.visited_pages.clear();
        let mut page_id = self.root_page_id;
        let mut expected_level = None;
        let mut parent_generation = None;
        for _ in 0..MAX_TREE_DEPTH {
            let node = self.load(reader, page_id, expected_level, parent_generation)?;
            // Either way, the entries before the cursor are those with keys below the bound.
            if node.leaf {
                self.leaf_index = match (bound, self.backward) {
                    (Some(bound), _) => node.lower_bound(bound)?,
                    (None, false) => 0,
                    (None, true) => node.len(),
                };
                self.leaf = Some(node);
                return Ok(());
            }
            let child_index = match (bound, self.backward) {
                (Some(bound), false) => node.child_index_for(bound)?,
                (Some(bound), true) => {
                    node.partition(0, |index| Ok(node.internal_key(index)? < bound))?
                }
                (None, false) => 0,
                (None, true) => node.len(),
            };
            page_id = node.child(child_index)?;
            expected_level = Some(node.level - 1);
            parent_generation = Some(node.generation);
            self.path.push((node, child_index));
        }
        Err(invalid_btree(storage_diagnostic!(
            "Tree {} exceeds the maximum depth of {MAX_TREE_DEPTH}",
            self.tree_id
        )))
    }

    fn advance_leaf(&mut self, reader: &mut dyn BtreeReadView) -> Result<bool> {
        while let Some((node, child_index)) = self.path.pop() {
            let sibling = if self.backward {
                child_index.checked_sub(1)
            } else {
                (child_index < node.len()).then_some(child_index + 1)
            };
            if let Some(sibling) = sibling {
                let next_child = node.child(sibling)?;
                let expected_level = node.level - 1;
                let parent_generation = node.generation;
                self.path.push((node, sibling));
                return self.descend_edge(reader, next_child, expected_level, parent_generation);
            }
        }
        Ok(false)
    }

    /// Descends to the first leaf under a node moving forward, or its last moving backward.
    fn descend_edge(
        &mut self,
        reader: &mut dyn BtreeReadView,
        mut page_id: PageId,
        mut expected_level: u8,
        mut parent_generation: u64,
    ) -> Result<bool> {
        for _ in self.path.len()..MAX_TREE_DEPTH {
            let node = self.load(
                reader,
                page_id,
                Some(expected_level),
                Some(parent_generation),
            )?;
            if node.leaf {
                self.leaf_index = if self.backward { node.len() } else { 0 };
                self.leaf = Some(node);
                return Ok(true);
            }
            let edge = if self.backward { node.len() } else { 0 };
            page_id = node.child(edge)?;
            expected_level = node.level - 1;
            parent_generation = node.generation;
            self.path.push((node, edge));
        }
        Err(invalid_btree(storage_diagnostic!(
            "Tree {} exceeds the maximum depth of {MAX_TREE_DEPTH}",
            self.tree_id
        )))
    }

    fn ensure_view(&self, reader: &dyn BtreeReadView) -> Result<()> {
        let current = reader.cursor_view(self.tree_id)?;
        if current != self.view {
            return Err(EngineError::new(
                "CURSOR_INVALIDATED",
                "The cursor was opened for another view of its tree than the current reader's",
            ));
        }
        Ok(())
    }
}

/// A value as a leaf cell holds it: inline bytes, or a descriptor of its overflow chain.
/// The entry at `index` of `leaf`, in a tree read at `view_generation`, whose value `read` reads
/// from its overflow pages when it has them.
fn read_cell<'a>(
    leaf: &'a NodeView<'static>,
    tree_id: TreeId,
    view_generation: u64,
    index: usize,
    read: &mut dyn FnMut(PageId) -> Result<Page>,
) -> Result<CursorEntry<'a>> {
    let (key, value) = match leaf.inline_leaf_cell(index) {
        Some((key, value)) => (key, CellValue::Inline(value)),
        None => leaf.leaf_cell(index)?,
    };
    let value = match value {
        CellValue::Inline(value) => Cow::Borrowed(value),
        CellValue::Overflow(descriptor) => Cow::Owned(
            read_overflow_chain(read, tree_id, view_generation, leaf.generation, &descriptor)?.0,
        ),
    };
    Ok((key, value))
}

enum CellValue<'a> {
    Inline(&'a [u8]),
    Overflow(OverflowDescriptor),
}

impl CellValue<'_> {
    fn encoded_len(&self) -> usize {
        match self {
            Self::Inline(value) => value.len(),
            Self::Overflow(_) => OVERFLOW_DESCRIPTOR_SIZE,
        }
    }
}

/// A reading of one B-tree page that decodes only the cells a reader visits.
///
/// Pages are verified as they enter the page cache, so a view checks the node header and the
/// cells it reads. A batch checks every cell of each leaf it reaches, for its bounds, its flags
/// or overflow descriptor, its packing and its order, before it writes the leaf again from the
/// bytes it keeps. `check()` and the catalog load decode with every check in [`Node::decode`],
/// through [`Btree::validating_cursor`]. A view owns a copy of its page's payload, which a cursor
/// keeps between steps, or borrows it from the page cache for one lookup.
#[derive(Debug)]
pub(crate) struct NodeView<'a> {
    page_id: PageId,
    level: u8,
    generation: u64,
    leaf: bool,
    leftmost_child: PageId,
    item_count: usize,
    free_end: usize,
    bytes: Cow<'a, [u8]>,
}

impl NodeView<'static> {
    fn open(
        page: Page,
        expected_tree_id: TreeId,
        view_generation: u64,
        require_view_generation: bool,
        strict: bool,
    ) -> Result<Self> {
        if strict {
            Node::decode(
                page.clone(),
                expected_tree_id,
                view_generation,
                require_view_generation,
            )?;
        }
        NodeView::from_payload(
            page.id,
            page.page_type,
            Cow::Owned(page.payload),
            expected_tree_id,
            view_generation,
            require_view_generation,
        )
    }
}

impl<'a> NodeView<'a> {
    /// A view of a page read in place.
    fn open_in_place(
        page: PageRef<'a>,
        expected_tree_id: TreeId,
        view_generation: u64,
        require_view_generation: bool,
    ) -> Result<Self> {
        NodeView::from_payload(
            page.id,
            page.page_type,
            Cow::Borrowed(page.payload),
            expected_tree_id,
            view_generation,
            require_view_generation,
        )
    }

    fn from_payload(
        page_id: PageId,
        page_type: PageType,
        payload: Cow<'a, [u8]>,
        expected_tree_id: TreeId,
        view_generation: u64,
        require_view_generation: bool,
    ) -> Result<Self> {
        let bytes = &*payload;
        if bytes.len() != MAX_PAGE_PAYLOAD_SIZE || &bytes[..4] != NODE_MAGIC {
            return Err(invalid_btree(storage_diagnostic!(
                "Page {page_id} is not a B-tree node"
            )));
        }
        let version = read_u16(bytes, 4);
        if version != NODE_FORMAT_VERSION || bytes[7] != NODE_FLAGS {
            return Err(unsupported_btree(format!(
                "B-tree page {page_id} format version {version} or flags are not supported"
            )));
        }
        let tree_id = read_u64(bytes, 8);
        if tree_id != expected_tree_id {
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {page_id} belongs to tree {tree_id}, not tree {expected_tree_id}"
            )));
        }
        let generation = read_u64(bytes, 16);
        if generation == 0
            || generation > view_generation
            || require_view_generation && generation != view_generation
        {
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {page_id} generation {generation} does not fit view generation {view_generation}"
            )));
        }
        let level = bytes[6];
        let leaf = match page_type {
            PageType::BtreeLeaf if level == 0 => true,
            PageType::BtreeInternal if level > 0 => false,
            other => {
                return Err(invalid_btree(storage_diagnostic!(
                    "Page {page_id} of type {other:?} at level {level} is not a valid B-tree node"
                )));
            }
        };
        let item_count = read_u16(bytes, 24) as usize;
        let free_start = read_u16(bytes, 26) as usize;
        let free_end = read_u16(bytes, 28) as usize;
        if free_start != NODE_HEADER_SIZE + item_count * SLOT_SIZE
            || free_start > free_end
            || free_end > MAX_PAGE_PAYLOAD_SIZE
        {
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {page_id} has invalid free-space bounds {free_start}..{free_end}"
            )));
        }
        let leftmost_child = read_u64(bytes, 32);
        if !leaf {
            validate_child_page(page_id, leftmost_child)?;
        }
        Ok(Self {
            page_id,
            level,
            generation,
            leaf,
            leftmost_child,
            item_count,
            free_end,
            bytes: payload,
        })
    }

    /// The number of entries: leaf cells, or an internal node's keyed children.
    pub(crate) fn len(&self) -> usize {
        self.item_count
    }

    /// The bytes of the leaf cell at `index`, header, key and value, which the rewrite of a leaf
    /// cell by cell copies. A rewrite by runs takes a cell's bounds from [`Self::kept`].
    #[cfg(test)]
    fn leaf_cell_bytes(&self, index: usize) -> Result<&[u8]> {
        let bytes = &*self.bytes;
        let offset = self.cell_offset(index)?;
        let header_end = checked_end(offset, LEAF_CELL_HEADER_SIZE, bytes.len())?;
        let key_length = read_u16(bytes, offset) as usize;
        let value_length = read_u32(bytes, offset + 4) as usize;
        let key_end = checked_end(header_end, key_length, bytes.len())?;
        Ok(&bytes[offset..checked_end(key_end, value_length, bytes.len())?])
    }

    /// The fingerprint recorded for the child at `index`, where 0 is the leftmost child.
    fn child_hash(&self, index: usize) -> Result<u64> {
        let bytes = &*self.bytes;
        if index == 0 {
            return Ok(read_u64(bytes, 40));
        }
        let offset = self.cell_offset(index - 1)?;
        checked_end(offset, INTERNAL_CELL_HEADER_SIZE, bytes.len())?;
        Ok(read_u64(bytes, offset + 12))
    }

    fn into_payload(self) -> Vec<u8> {
        self.bytes.into_owned()
    }

    #[inline(always)]
    fn cell_offset(&self, index: usize) -> Result<usize> {
        if index >= self.item_count {
            return Err(self.cell_error(index));
        }
        let bytes = &*self.bytes;
        let offset = read_u16(bytes, NODE_HEADER_SIZE + index * SLOT_SIZE) as usize;
        if offset < self.free_end || offset >= bytes.len() {
            return Err(self.cell_error(index));
        }
        Ok(offset)
    }

    #[cold]
    #[inline(never)]
    fn cell_error(&self, index: usize) -> EngineError {
        if index >= self.item_count {
            invalid_btree(storage_diagnostic!(
                "B-tree page {} has no cell {index}",
                self.page_id
            ))
        } else {
            invalid_btree(storage_diagnostic!(
                "B-tree page {} cell {index} lies outside its cell area",
                self.page_id
            ))
        }
    }

    fn leaf_cell(&self, index: usize) -> Result<(&[u8], CellValue<'_>)> {
        let bytes = &*self.bytes;
        let offset = self.cell_offset(index)?;
        let header_end = checked_end(offset, LEAF_CELL_HEADER_SIZE, bytes.len())?;
        let key_length = read_u16(bytes, offset) as usize;
        let flags = read_u16(bytes, offset + 2);
        let value_length = read_u32(bytes, offset + 4) as usize;
        let key_end = checked_end(header_end, key_length, bytes.len())?;
        let value_end = checked_end(key_end, value_length, bytes.len())?;
        let value = match flags {
            INLINE_CELL_FLAGS => CellValue::Inline(&bytes[key_end..value_end]),
            OVERFLOW_CELL_FLAGS if value_length == OVERFLOW_DESCRIPTOR_SIZE => {
                let descriptor = OverflowDescriptor {
                    first_page_id: read_u64(bytes, key_end),
                    generation: read_u64(bytes, key_end + 8),
                    total_length: read_u32(bytes, key_end + 16),
                    checksum: read_u32(bytes, key_end + 20),
                };
                validate_overflow_descriptor(&descriptor, self.generation)?;
                CellValue::Overflow(descriptor)
            }
            _ => {
                return Err(unsupported_btree(format!(
                    "Leaf cell flags {flags:#06x} are not supported"
                )));
            }
        };
        Ok((&bytes[header_end..key_end], value))
    }

    /// The key and value of leaf cell `index`, if it is well formed and holds its value inline,
    /// as nearly every cell does. Compiled into the cursor's step, which reads every row a scan
    /// visits; [`Self::leaf_cell`] reads any other cell, and reports what is wrong with it.
    #[inline(always)]
    pub(crate) fn inline_leaf_cell(&self, index: usize) -> Option<(&[u8], &[u8])> {
        let bytes = &*self.bytes;
        if index >= self.item_count {
            return None;
        }
        let offset = u16_at(bytes, NODE_HEADER_SIZE + index * SLOT_SIZE) as usize;
        let header_end = offset + LEAF_CELL_HEADER_SIZE;
        if offset < self.free_end
            || header_end > bytes.len()
            || u16_at(bytes, offset + 2) != INLINE_CELL_FLAGS
        {
            return None;
        }
        let key_end = header_end + u16_at(bytes, offset) as usize;
        let value_end = key_end + u32_at(bytes, offset + 4) as usize;
        if value_end > bytes.len() || value_end < key_end {
            return None;
        }
        Some((&bytes[header_end..key_end], &bytes[key_end..value_end]))
    }

    /// The offset slot `index` holds, which no check has yet placed in the cell area. `index` must
    /// be below [`Self::len`].
    #[inline(always)]
    fn slot(&self, index: usize) -> usize {
        u16_at(&self.bytes, NODE_HEADER_SIZE + index * SLOT_SIZE) as usize
    }

    /// The key of leaf cell `index`, which must be below [`Self::len`], and the offsets its cell
    /// starts and ends at. A well-formed cell with an inline value, by exactly the tests of
    /// [`Self::inline_leaf_cell`], is read here, compiled into the merge that reads every cell
    /// of a leaf a batch reaches; [`Self::kept_other`] reads any other. `bytes` is this view's
    /// payload, which that merge takes once for the leaf: taken from the view here, it was a call
    /// for each cell.
    #[inline(always)]
    fn kept<'b>(&'b self, bytes: &'b [u8], index: usize) -> Result<(&'b [u8], usize, usize)> {
        let offset = u16_at(bytes, NODE_HEADER_SIZE + index * SLOT_SIZE) as usize;
        let header_end = offset + LEAF_CELL_HEADER_SIZE;
        if offset >= self.free_end
            && header_end <= bytes.len()
            && u16_at(bytes, offset + 2) == INLINE_CELL_FLAGS
        {
            let key_end = header_end + u16_at(bytes, offset) as usize;
            let value_end = key_end + u32_at(bytes, offset + 4) as usize;
            if value_end <= bytes.len() && value_end >= key_end {
                return Ok((&bytes[header_end..key_end], offset, value_end));
            }
        }
        self.kept_other(index, offset)
    }

    /// What [`Self::kept`] returns for a cell that is not a well-formed one with an inline value:
    /// the same of a cell whose value is in overflow pages, or what [`Self::leaf_cell`] reports
    /// is wrong with it. `start` is the offset the cell's slot holds, which reading the cell
    /// places in the cell area.
    #[cold]
    #[inline(never)]
    fn kept_other(&self, index: usize, start: usize) -> Result<(&[u8], usize, usize)> {
        let (key, value) = self.leaf_cell(index)?;
        let end = start + LEAF_CELL_HEADER_SIZE + key.len() + value.encoded_len();
        Ok((key, start, end))
    }

    /// Where the cells of `run`, which is not empty, lie in the leaf `source`, which packs its
    /// cells downward in slot order: from where the last of them starts to where the first ends,
    /// which is where the cell before it starts, or the end of the payload. Returns the leaf with
    /// the two offsets. A batch into an empty tree has no leaf to keep cells from, and keeps none.
    fn span<'s>(source: Option<&'s Self>, run: &Range<usize>) -> Result<(&'s Self, usize, usize)> {
        let Some(leaf) = source else {
            return Err(invalid_btree("A kept B-tree cell has no source page"));
        };
        let low = leaf.slot(run.end - 1);
        let high = match run.start {
            0 => leaf.bytes.len(),
            start => leaf.slot(start - 1),
        };
        if low < leaf.free_end || high < low || high > leaf.bytes.len() {
            return Err(leaf.cell_error(run.end - 1));
        }
        Ok((leaf, low, high))
    }

    /// The key of leaf cell `index`, reading only the key: binary searches read many keys and
    /// no values.
    fn leaf_key(&self, index: usize) -> Result<&[u8]> {
        let bytes = &*self.bytes;
        let offset = self.cell_offset(index)?;
        let header_end = checked_end(offset, LEAF_CELL_HEADER_SIZE, bytes.len())?;
        let key_length = read_u16(bytes, offset) as usize;
        Ok(&bytes[header_end..checked_end(header_end, key_length, bytes.len())?])
    }

    fn internal_key(&self, index: usize) -> Result<&[u8]> {
        let bytes = &*self.bytes;
        let offset = self.cell_offset(index)?;
        let header_end = checked_end(offset, INTERNAL_CELL_HEADER_SIZE, bytes.len())?;
        let key_length = read_u16(bytes, offset) as usize;
        Ok(&bytes[header_end..checked_end(header_end, key_length, bytes.len())?])
    }

    /// The child page at `index`, where 0 is the leftmost child and `index` n follows key n - 1.
    fn child(&self, index: usize) -> Result<PageId> {
        let child = if index == 0 {
            self.leftmost_child
        } else {
            let bytes = &*self.bytes;
            let offset = self.cell_offset(index - 1)?;
            checked_end(offset, INTERNAL_CELL_HEADER_SIZE, bytes.len())?;
            read_u64(bytes, offset + 4)
        };
        validate_child_page(self.page_id, child)?;
        Ok(child)
    }

    /// The child to follow for `key`: past every separator key at or below it.
    fn child_index_for(&self, key: &[u8]) -> Result<usize> {
        self.partition(0, |index| Ok(self.internal_key(index)? <= key))
    }

    /// The first leaf entry at or after `key`.
    fn lower_bound(&self, key: &[u8]) -> Result<usize> {
        self.lower_bound_from(0, key)
    }

    /// The first leaf entry at or after `key`, among those from `low` on.
    fn lower_bound_from(&self, low: usize, key: &[u8]) -> Result<usize> {
        self.partition(low, |index| Ok(self.leaf_key(index)? < key))
    }

    /// The leaf entry whose key is exactly `key`, as a lookup found it before
    /// [`Self::search`], which tests hold to it.
    #[cfg(test)]
    fn find(&self, key: &[u8]) -> Result<Option<usize>> {
        let index = self.lower_bound(key)?;
        Ok((index < self.item_count && self.leaf_key(index)? == key).then_some(index))
    }

    /// Where `key` falls among the node's keys, and whether the key there is `key` itself: in a
    /// leaf the first entry at or after it, as [`Self::lower_bound`] finds, and in an internal
    /// node the child to follow, as [`Self::child_index_for`] finds. One loop reads each key it
    /// probes in place, where those call a reader for each. `None` when a probed cell does not
    /// lie within the page, which the readers then report as they always have.
    #[inline(always)]
    fn search(&self, key: &[u8]) -> Option<(usize, bool)> {
        let page: &[u8; MAX_PAGE_PAYLOAD_SIZE] = (&*self.bytes).try_into().ok()?;
        let slots = page[NODE_HEADER_SIZE..].as_chunks::<SLOT_SIZE>().0;
        let header = if self.leaf {
            LEAF_CELL_HEADER_SIZE
        } else {
            INTERNAL_CELL_HEADER_SIZE
        };
        // A leaf's search passes the keys below `key`, and an internal node's those at or below.
        let passed = if self.leaf {
            Ordering::Less
        } else {
            Ordering::Equal
        };
        let (mut low, mut high) = (0, self.item_count);
        let mut found = false;
        while low < high {
            let middle = low + (high - low) / 2;
            let offset = usize::from(u16::from_le_bytes(*slots.get(middle)?));
            if offset < self.free_end {
                return None;
            }
            let length = page.get(offset..offset + 2)?;
            let start = offset + header;
            let end = start + usize::from(u16::from_le_bytes([length[0], length[1]]));
            let order = key_order(page.get(start..end)?, key);
            if order <= passed {
                low = middle + 1;
            } else {
                high = middle;
                found = order == Ordering::Equal;
            }
        }
        Some((low, found))
    }

    /// The error the readers [`Self::search`] stands in for report of the cell it refused. They
    /// probe the cells it probed, in its order, and so stop at the same one.
    #[cold]
    #[inline(never)]
    fn search_error(&self, key: &[u8]) -> EngineError {
        let searched = if self.leaf {
            self.lower_bound(key)
        } else {
            self.child_index_for(key)
        };
        debug_assert!(searched.is_err());
        searched.err().unwrap_or_else(|| invalid_btree(""))
    }

    /// The number of leading entries for which `before` holds, by binary search over sorted cells,
    /// all of which before `low` it holds for.
    fn partition(
        &self,
        low: usize,
        mut before: impl FnMut(usize) -> Result<bool>,
    ) -> Result<usize> {
        let (mut low, mut high) = (low.min(self.item_count), self.item_count);
        while low < high {
            let middle = low + (high - low) / 2;
            if before(middle)? {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }
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
            return Err(invalid_overflow(storage_diagnostic!(
                "Overflow chunk index {} is outside chunk count {}",
                self.chunk_index,
                self.chunk_count
            )));
        }
        if self.total_length == 0 || self.total_length as usize > MAX_BTREE_VALUE_BYTES {
            return Err(invalid_overflow(storage_diagnostic!(
                "Overflow total length {} is outside 1..={MAX_BTREE_VALUE_BYTES}",
                self.total_length
            )));
        }
        let expected_chunk_count = (self.total_length as usize).div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
        if self.chunk_count as usize != expected_chunk_count {
            return Err(invalid_overflow(storage_diagnostic!(
                "Overflow chunk count {} does not match total length {}",
                self.chunk_count,
                self.total_length
            )));
        }
        let expected_chunk_length = overflow_chunk_length(
            self.total_length as usize,
            self.chunk_index as usize,
            self.chunk_count as usize,
        )?;
        if self.chunk.len() != expected_chunk_length {
            return Err(invalid_overflow(storage_diagnostic!(
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
                return Err(invalid_overflow(storage_diagnostic!(
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
            return Err(invalid_overflow(storage_diagnostic!(
                "Overflow chain page {} has type {:?}",
                page.id,
                page.page_type
            )));
        }
        if page.payload.len() < OVERFLOW_HEADER_SIZE {
            return Err(invalid_overflow(storage_diagnostic!(
                "Overflow page {} payload is truncated to {} bytes",
                page.id,
                page.payload.len()
            )));
        }
        let bytes = &page.payload;
        if &bytes[..4] != OVERFLOW_MAGIC {
            return Err(invalid_overflow(storage_diagnostic!(
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
            return Err(invalid_overflow(storage_diagnostic!(
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
            return Err(invalid_overflow(storage_diagnostic!(
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
            return Err(invalid_overflow(storage_diagnostic!(
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
                return Err(invalid_overflow(storage_diagnostic!(
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
    leftmost_child_hash: u64,
    entries: Vec<InternalEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InternalEntry {
    key: Vec<u8>,
    right_child: PageId,
    child_hash: u64,
}

impl InternalNode {
    #[cfg(test)]
    fn child_index_for(&self, key: &[u8]) -> usize {
        self.entries
            .partition_point(|entry| entry.key.as_slice() <= key)
    }

    fn child(&self, index: usize) -> Result<PageId> {
        if index == 0 {
            Ok(self.leftmost_child)
        } else {
            self.entries
                .get(index - 1)
                .map(|entry| entry.right_child)
                .ok_or_else(|| {
                    invalid_btree(storage_diagnostic!(
                        "Internal child index {index} is out of range"
                    ))
                })
        }
    }

    #[cfg(test)]
    fn replace_child(&mut self, index: usize, page_id: PageId, child_hash: u64) -> Result<()> {
        if index == 0 {
            self.leftmost_child = page_id;
            self.leftmost_child_hash = child_hash;
            Ok(())
        } else if let Some(entry) = self.entries.get_mut(index - 1) {
            entry.right_child = page_id;
            entry.child_hash = child_hash;
            Ok(())
        } else {
            Err(invalid_btree(storage_diagnostic!(
                "Internal child index {index} is out of range"
            )))
        }
    }
}

impl Node {
    #[cfg(test)]
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
        leftmost_child_hash: u64,
        entries: Vec<InternalEntry>,
    ) -> Self {
        Self {
            tree_id,
            generation,
            level,
            kind: NodeKind::Internal(InternalNode {
                leftmost_child,
                leftmost_child_hash,
                entries,
            }),
        }
    }

    /// Fingerprints every entry beneath this node.
    ///
    /// A leaf combines its own entries; an internal node combines the fingerprints its children
    /// reported when they were written. Because [`combine`] is order-independent, the value
    /// depends only on the set of entries in the subtree and not on how the tree divides them
    /// across pages.
    fn subtree_hash(&self) -> u64 {
        match &self.kind {
            NodeKind::Leaf(entries) => entries.iter().fold(EMPTY_HASH, |hash, entry| {
                combine(hash, leaf_entry_hash(entry))
            }),
            NodeKind::Internal(internal) => internal
                .entries
                .iter()
                .fold(internal.leftmost_child_hash, |hash, entry| {
                    combine(hash, entry.child_hash)
                }),
        }
    }

    /// Encodes the node and reports the fingerprint of the subtree it roots.
    ///
    /// A node never stores its own fingerprint. The only durable copy is the one its parent
    /// holds, which keeps the tree free of a second source of truth that could silently disagree
    /// with the entries themselves. An internal node does store the fingerprint of its leftmost
    /// child, which has no cell of its own to carry it.
    fn encode(&self, page_id: PageId) -> Result<(Page, u64)> {
        validate_tree_id(self.tree_id)?;
        if self.generation == 0 {
            return Err(invalid_btree("B-tree page generation must be positive"));
        }
        let (page_type, leftmost_child, leftmost_child_hash, cells) = match &self.kind {
            NodeKind::Leaf(entries) => {
                if self.level != 0 {
                    return Err(invalid_btree("A leaf node must have level zero"));
                }
                validate_sorted_leaf_entries(entries, self.generation)?;
                let mut cells = Vec::with_capacity(entries.len());
                for entry in entries {
                    cells.push(encode_leaf_cell(entry)?);
                }
                (PageType::BtreeLeaf, NO_PAGE_ID, EMPTY_HASH, cells)
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
                let mut cells = Vec::with_capacity(internal.entries.len());
                for entry in &internal.entries {
                    cells.push(encode_internal_cell(entry)?);
                }
                (
                    PageType::BtreeInternal,
                    internal.leftmost_child,
                    internal.leftmost_child_hash,
                    cells,
                )
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
        payload[40..48].copy_from_slice(&leftmost_child_hash.to_le_bytes());

        let mut free_end = MAX_PAGE_PAYLOAD_SIZE;
        for (index, cell) in cells.iter().enumerate() {
            free_end -= cell.len();
            payload[free_end..free_end + cell.len()].copy_from_slice(cell);
            put_u16(
                &mut payload,
                NODE_HEADER_SIZE + index * SLOT_SIZE,
                free_end as u16,
            );
        }
        payload[28..30].copy_from_slice(&(free_end as u16).to_le_bytes());
        Ok((Page::new(page_id, page_type, payload)?, self.subtree_hash()))
    }

    fn decode(
        page: Page,
        expected_tree_id: TreeId,
        view_generation: u64,
        require_view_generation: bool,
    ) -> Result<Self> {
        Self::decode_in_place(
            page.as_page_ref(),
            expected_tree_id,
            view_generation,
            require_view_generation,
        )
    }

    /// Decodes a node, as [`Self::decode`] does, from a page read in place.
    fn decode_in_place(
        page: PageRef<'_>,
        expected_tree_id: TreeId,
        view_generation: u64,
        require_view_generation: bool,
    ) -> Result<Self> {
        if page.payload.len() != MAX_PAGE_PAYLOAD_SIZE {
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {} payload is {} bytes, not {MAX_PAGE_PAYLOAD_SIZE}",
                page.id,
                page.payload.len()
            )));
        }
        let bytes = &page.payload;
        if &bytes[..4] != NODE_MAGIC {
            return Err(invalid_btree(storage_diagnostic!(
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
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {} belongs to tree {tree_id}, not tree {expected_tree_id}",
                page.id
            )));
        }
        let generation = read_u64(bytes, 16);
        if generation == 0 || generation > view_generation {
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {} generation {generation} is outside view generation {view_generation}",
                page.id
            )));
        }
        if require_view_generation && generation != view_generation {
            return Err(invalid_btree(storage_diagnostic!(
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
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {} has invalid free-space bounds {free_start}..{free_end}",
                page.id
            )));
        }
        if bytes[30..32].iter().any(|byte| *byte != 0) {
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {} reserved header bytes must be zero",
                page.id
            )));
        }
        if bytes[free_start..free_end].iter().any(|byte| *byte != 0) {
            return Err(invalid_btree(storage_diagnostic!(
                "B-tree page {} free space must be zero-filled",
                page.id
            )));
        }
        let leftmost_child = read_u64(bytes, 32);
        let leftmost_child_hash = read_u64(bytes, 40);
        let mut expected_cell_end = MAX_PAGE_PAYLOAD_SIZE;

        let kind = match page.page_type {
            PageType::BtreeLeaf => {
                if level != 0 || leftmost_child != NO_PAGE_ID || leftmost_child_hash != EMPTY_HASH {
                    return Err(invalid_btree(storage_diagnostic!(
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
                    return Err(invalid_btree(storage_diagnostic!(
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
                    leftmost_child_hash,
                    entries,
                })
            }
            other => {
                return Err(invalid_btree(storage_diagnostic!(
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

    #[cfg(test)]
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

    #[cfg(test)]
    fn fits(&self) -> Result<bool> {
        Ok(self.encoded_size()? <= MAX_PAGE_PAYLOAD_SIZE)
    }
}

#[cfg(test)]
struct InsertedPage {
    page_id: PageId,
    hash: u64,
    split: Option<PageSplit>,
}

/// One page a batch wrote in place of a node it changed.
struct Piece {
    /// The first key beneath the page, when its parent must record a new one: for every page a
    /// division created, and for a page whose first key changed. `None` leaves the parent's
    /// separator, or the absence of one, as it was.
    first_key: Option<Vec<u8>>,
    page_id: PageId,
    hash: u64,
}

/// A child of an internal node, as a batch rebuilds the node: its separator, `None` for the
/// leftmost, its page, and its fingerprint.
struct Child {
    separator: Option<Vec<u8>>,
    page_id: PageId,
    hash: u64,
}

/// The state of one [`Btree::apply`].
struct BatchWriter<'t, 'p, D: PageDevice> {
    transaction: &'t mut PagerWriteTransaction<'p, D>,
    tree_id: TreeId,
    generation: u64,
    visited: PageSet,
    inserted: usize,
    removed: usize,
    /// Whether leaves are rewritten cell by cell, as they were before they were rewritten by
    /// runs of kept cells, for a test to compare the two.
    #[cfg(test)]
    by_cell: bool,
}

/// One entry of a leaf the batch writer writes: a run of consecutive cells kept, byte for byte,
/// from the leaf it rewrites, which is never empty, or a new cell.
enum LeafCell<'a> {
    Kept(Range<usize>),
    New { key: &'a [u8], value: CellValue<'a> },
}

/// One entry of a leaf as the batch writer listed them before it kept cells in runs: one cell
/// kept from the leaf it rewrites, or a new one.
#[cfg(test)]
enum CellByCell<'a> {
    Kept(usize),
    New { key: &'a [u8], value: CellValue<'a> },
}

/// A walk over the cells of a leaf that a batch merges its changes into, in slot order.
///
/// The walk passes every cell of the leaf once, and checks each as it does: that the cell is well
/// formed, which reading it shows; that it ends where the cell before it starts, or with the
/// payload when it is the first; and that its key is above the key before it. Every writer packs a
/// leaf's cells that way, downward from the end of the payload in key order, so the cells between
/// two slots are the bytes between two offsets, and a run of kept cells can be copied as one. A
/// leaf laid out otherwise is refused here, before any of it is written, and before the batch
/// knows whether it changes the leaf: a batch that would leave such a leaf as it was fails too.
struct LeafWalk<'n> {
    node: &'n NodeView<'n>,
    /// The leaf's payload, taken from the view once: [`NodeView::kept`] says why.
    bytes: &'n [u8],
    page_id: PageId,
    /// The next cell to pass.
    index: usize,
    /// The key of the cell passed last, which the next cell's key must be above.
    previous: Option<&'n [u8]>,
    /// Where the next cell must end: where the cell passed last starts, or the end of the payload.
    floor: usize,
}

impl LeafWalk<'_> {
    /// Passes the cells whose keys are below `bound`, or every cell left when there is none.
    /// Returns whether it stopped on the cell that holds `bound`: that cell has been passed, and
    /// [`Self::index`] is still its index, for the caller to step over once it has read the
    /// value. A cell above `bound` is not passed, and the next call reads it again.
    ///
    /// Compiled into the merge, so that reading and checking a cell is no call: this runs for
    /// every cell of every leaf a commit reaches.
    #[inline(always)]
    fn pass(&mut self, bound: Option<&[u8]>) -> Result<bool> {
        let node = self.node;
        // A cell that cannot be read, and one that no writer lays out so, leave by one way out:
        // the merge this is compiled into then hands on one error, where it copied each of three.
        let error = 'refused: {
            while self.index < node.len() {
                let (key, start, end) = match node.kept(self.bytes, self.index) {
                    Ok(cell) => cell,
                    Err(error) => break 'refused error,
                };
                let order = match bound {
                    Some(bound) => key_order(key, bound),
                    None => Ordering::Less,
                };
                if order == Ordering::Greater {
                    break;
                }
                let ordered = match self.previous {
                    Some(previous) => key_order(key, previous) == Ordering::Greater,
                    None => true,
                };
                if end != self.floor {
                    break 'refused unpacked_cell(self.page_id, self.index);
                }
                if !ordered {
                    break 'refused unordered_leaf(self.page_id);
                }
                self.previous = Some(key);
                self.floor = start;
                if order == Ordering::Equal {
                    return Ok(true);
                }
                self.index += 1;
            }
            return Ok(false);
        };
        Err(error)
    }
}

/// Adds the cells of `run`, when it has any, to the cells a leaf will hold, as part of the run
/// before them when they follow it in the leaf.
fn keep(cells: &mut Vec<LeafCell<'_>>, run: Range<usize>) {
    if run.is_empty() {
        return;
    }
    match cells.last_mut() {
        Some(LeafCell::Kept(last)) if last.end == run.start => last.end = run.end,
        _ => cells.push(LeafCell::Kept(run)),
    }
}

impl<D: PageDevice> BatchWriter<'_, '_, D> {
    /// Applies the changes that fall beneath one node, whose fingerprint its parent records as
    /// `hash`, if it has a parent. Returns `None` when they leave it as it was, and otherwise the
    /// pages that replace it, which are none once it is empty.
    fn apply(
        &mut self,
        page_id: PageId,
        changes: &[BatchChange<'_>],
        depth: usize,
        expected_level: Option<u8>,
        parent_generation: Option<u64>,
        hash: Option<u64>,
    ) -> Result<Option<(Vec<Piece>, u8)>> {
        let tree_id = self.tree_id;
        if depth >= MAX_TREE_DEPTH {
            return Err(invalid_btree(storage_diagnostic!(
                "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
            )));
        }
        if !self.visited.insert(page_id) {
            return Err(invalid_btree(storage_diagnostic!(
                "Tree {tree_id} contains a cycle through page {page_id}"
            )));
        }
        let owned = self.transaction.owns_page(page_id);
        // A copy, since writing the node's replacements changes the page cache it came from.
        let node = {
            let page = self.transaction.read_page_in_place(page_id)?;
            NodeView::from_payload(
                page.id,
                page.page_type,
                Cow::Owned(page.payload.to_vec()),
                tree_id,
                self.generation,
                owned,
            )?
        };
        validate_expected_level(page_id, node.level, expected_level)?;
        validate_child_generation(page_id, node.generation, parent_generation)?;
        let level = node.level;
        let pieces = if node.leaf {
            self.apply_leaf(page_id, owned, &node, hash, changes)?
        } else {
            self.apply_internal(page_id, owned, node, changes, depth)?
        };
        Ok(pieces.map(|pieces| (pieces, level)))
    }

    /// Merges changes into a leaf without decoding its cells: the cells it keeps are copied as
    /// they are, in runs, and the leaf's fingerprint, when its parent gave it, changes by exactly
    /// the entries that were removed, replaced and added.
    fn apply_leaf(
        &mut self,
        page_id: PageId,
        owned: bool,
        node: &NodeView<'_>,
        hash: Option<u64>,
        changes: &[BatchChange<'_>],
    ) -> Result<Option<Vec<Piece>>> {
        #[cfg(test)]
        if self.by_cell {
            return self.apply_leaf_by_cell(page_id, owned, node, hash, changes);
        }
        let count = node.len();
        let appends = (count == 0 || changes[0].key > node.leaf_key(count - 1)?)
            && changes.iter().all(|change| change.value.is_some());
        // The most segments a merge leaves: a cell for each change, and a run of kept cells before
        // each change and after the last, which are no more runs than the leaf has cells.
        let mut cells = Vec::with_capacity(changes.len() + count.min(changes.len() + 1));
        let mut delta = EMPTY_HASH;
        // The cells removed outright, whose fingerprints matter only if the leaf keeps others: a
        // leaf that empties is released, and its parent drops the fingerprint it records for it.
        let mut removed = Vec::new();
        let mut changed = false;
        let bytes = &*node.bytes;
        let mut walk = LeafWalk {
            node,
            bytes,
            page_id,
            index: 0,
            previous: None,
            floor: bytes.len(),
        };
        let mut changes = changes.iter();
        // The walk is compiled in here once, for each change and for the cells behind the last.
        loop {
            let change = changes.next();
            let start = walk.index;
            let held = walk.pass(change.map(|change| change.key))?;
            keep(&mut cells, start..walk.index);
            let Some(change) = change else {
                break;
            };
            if held {
                let index = walk.index;
                walk.index += 1;
                let (_, held) = node.leaf_cell(index)?;
                // An upsert of the value an entry already holds inline changes nothing.
                if let (CellValue::Inline(held), Some(value)) = (&held, change.value)
                    && *held == value
                {
                    keep(&mut cells, index..index + 1);
                    continue;
                }
                // The held cell's key is the change's, so a replaced entry takes one hash of it
                // for the fingerprint of the value it held and of the one it takes.
                let replacement = match change.value {
                    Some(value) => {
                        let seed = key_hash(change.key);
                        delta = combine(delta, value_hash(seed, &held));
                        Some((value, seed))
                    }
                    None => {
                        removed.push(index);
                        None
                    }
                };
                if let CellValue::Overflow(descriptor) = held {
                    release_leaf_value(
                        self.transaction,
                        self.tree_id,
                        self.generation,
                        node.generation,
                        &LeafValue::Overflow(descriptor),
                    )?;
                }
                changed = true;
                match replacement {
                    Some((value, seed)) => {
                        let value = self.new_value(change.key, value)?;
                        delta = combine(delta, value_hash(seed, &value));
                        cells.push(LeafCell::New {
                            key: change.key,
                            value,
                        });
                    }
                    None => self.removed += 1,
                }
            } else if let Some(value) = change.value {
                changed = true;
                self.inserted += 1;
                let value = self.new_value(change.key, value)?;
                delta = combine(delta, cell_hash(change.key, &value));
                cells.push(LeafCell::New {
                    key: change.key,
                    value,
                });
            }
        }
        if !changed {
            return Ok(None);
        }
        if cells.is_empty() {
            release_node_page(self.transaction, page_id, owned)?;
            return Ok(Some(Vec::new()));
        }
        for index in removed {
            let (key, value) = node.leaf_cell(index)?;
            delta = combine(delta, cell_hash(key, &value));
        }
        let first_key = match &cells[0] {
            LeafCell::Kept(run) => node.leaf_key(run.start)?,
            LeafCell::New { key, .. } => key,
        };
        let first_changed = count == 0 || first_key != node.leaf_key(0)?;
        let mut pieces = self.write_leaf_cells(
            Some((page_id, owned)),
            Some(node),
            &cells,
            appends,
            hash.map(|hash| combine(hash, delta)),
        )?;
        if !first_changed {
            pieces[0].first_key = None;
        }
        Ok(Some(pieces))
    }

    /// Merges changes into a leaf as [`Self::apply_leaf`] did before it kept cells in runs: it
    /// lists every kept cell, compares each with the kept cell before it, and leaves a kept
    /// cell's value and flags to [`Self::write_leaf_cells_by_cell`]. Tests compare the two.
    #[cfg(test)]
    fn apply_leaf_by_cell(
        &mut self,
        page_id: PageId,
        owned: bool,
        node: &NodeView<'_>,
        hash: Option<u64>,
        changes: &[BatchChange<'_>],
    ) -> Result<Option<Vec<Piece>>> {
        let count = node.len();
        let appends = (count == 0 || changes[0].key > node.leaf_key(count - 1)?)
            && changes.iter().all(|change| change.value.is_some());
        let mut cells = Vec::with_capacity(count + changes.len());
        let mut delta = EMPTY_HASH;
        // The cells removed outright, whose fingerprints matter only if the leaf keeps others: a
        // leaf that empties is released, and its parent drops the fingerprint it records for it.
        let mut removed = Vec::new();
        let mut changed = false;
        let mut index = 0;
        let mut previous: Option<&[u8]> = None;
        let mut kept_until = |index: &mut usize, bound: Option<&[u8]>, cells: &mut Vec<_>| {
            while *index < count {
                let key = node.leaf_key(*index)?;
                if bound.is_some_and(|bound| key >= bound) {
                    break;
                }
                if previous.is_some_and(|previous| previous >= key) {
                    return Err(invalid_btree(storage_diagnostic!(
                        "B-tree leaf {page_id} keys are not strictly increasing"
                    )));
                }
                previous = Some(key);
                cells.push(CellByCell::Kept(*index));
                *index += 1;
            }
            Ok(())
        };
        for change in changes {
            kept_until(&mut index, Some(change.key), &mut cells)?;
            let held = match index < count {
                true => {
                    let (key, value) = node.leaf_cell(index)?;
                    (key == change.key).then_some(value)
                }
                false => None,
            };
            if let Some(held) = held {
                index += 1;
                // An upsert of the value an entry already holds inline changes nothing.
                if let (CellValue::Inline(held), Some(value)) = (&held, change.value)
                    && *held == value
                {
                    cells.push(CellByCell::Kept(index - 1));
                    continue;
                }
                // The held cell's key is the change's, so a replaced entry takes one hash of it
                // for the fingerprint of the value it held and of the one it takes.
                let replacement = match change.value {
                    Some(value) => {
                        let seed = key_hash(change.key);
                        delta = combine(delta, value_hash(seed, &held));
                        Some((value, seed))
                    }
                    None => {
                        removed.push(index - 1);
                        None
                    }
                };
                if let CellValue::Overflow(descriptor) = held {
                    release_leaf_value(
                        self.transaction,
                        self.tree_id,
                        self.generation,
                        node.generation,
                        &LeafValue::Overflow(descriptor),
                    )?;
                }
                changed = true;
                match replacement {
                    Some((value, seed)) => {
                        let value = self.new_value(change.key, value)?;
                        delta = combine(delta, value_hash(seed, &value));
                        cells.push(CellByCell::New {
                            key: change.key,
                            value,
                        });
                    }
                    None => self.removed += 1,
                }
            } else if let Some(value) = change.value {
                changed = true;
                self.inserted += 1;
                let value = self.new_value(change.key, value)?;
                delta = combine(delta, cell_hash(change.key, &value));
                cells.push(CellByCell::New {
                    key: change.key,
                    value,
                });
            }
        }
        kept_until(&mut index, None, &mut cells)?;
        if !changed {
            return Ok(None);
        }
        if cells.is_empty() {
            release_node_page(self.transaction, page_id, owned)?;
            return Ok(Some(Vec::new()));
        }
        for index in removed {
            let (key, value) = node.leaf_cell(index)?;
            delta = combine(delta, cell_hash(key, &value));
        }
        let first_key = match &cells[0] {
            CellByCell::Kept(index) => node.leaf_key(*index)?,
            CellByCell::New { key, .. } => key,
        };
        let first_changed = count == 0 || first_key != node.leaf_key(0)?;
        let mut pieces = self.write_leaf_cells_by_cell(
            Some((page_id, owned)),
            Some(node),
            &cells,
            appends,
            hash.map(|hash| combine(hash, delta)),
        )?;
        if !first_changed {
            pieces[0].first_key = None;
        }
        Ok(Some(pieces))
    }

    /// Applies changes to the children of an internal node. When every child it changed is still
    /// one page with the same first key, only the node's references to them change, and the node
    /// is rewritten from its own bytes; otherwise its children are listed and redistributed.
    fn apply_internal(
        &mut self,
        page_id: PageId,
        owned: bool,
        node: NodeView<'static>,
        changes: &[BatchChange<'_>],
        depth: usize,
    ) -> Result<Option<Vec<Piece>>> {
        let count = node.len() + 1;
        let mut applied = Vec::new();
        let mut start = 0;
        for child in 0..count {
            // A child takes the changes below the next child's separator.
            let end = if child + 1 < count {
                let separator = node.internal_key(child)?;
                start + changes[start..].partition_point(|change| change.key < separator)
            } else {
                changes.len()
            };
            if end > start
                && let Some((pieces, _)) = self.apply(
                    node.child(child)?,
                    &changes[start..end],
                    depth + 1,
                    Some(node.level - 1),
                    Some(node.generation),
                    Some(node.child_hash(child)?),
                )?
            {
                applied.push((child, pieces));
            }
            start = end;
        }
        if applied.is_empty() {
            return Ok(None);
        }

        if applied
            .iter()
            .all(|(_, pieces)| pieces.len() == 1 && pieces[0].first_key.is_none())
        {
            let mut hash = EMPTY_HASH;
            for child in 0..count {
                hash = combine(hash, node.child_hash(child)?);
            }
            let mut references = Vec::with_capacity(applied.len());
            for (child, pieces) in &applied {
                let piece = &pieces[0];
                hash = combine(combine(hash, node.child_hash(*child)?), piece.hash);
                // The leftmost child's reference is in the header; each other's follows its
                // cell's key length and flags.
                let at = match child {
                    0 => 32,
                    child => node.cell_offset(child - 1)? + 4,
                };
                references.push((at, piece.page_id, piece.hash));
            }
            let mut payload = node.into_payload();
            for (at, child_page_id, child_hash) in references {
                put(&mut payload, at, child_page_id.to_le_bytes());
                put(&mut payload, at + 8, child_hash.to_le_bytes());
            }
            payload[16..24].copy_from_slice(&self.generation.to_le_bytes());
            let replacing = Some((page_id, owned));
            let written = self.page_for(0, replacing)?;
            self.write_payload(written, PageType::BtreeInternal, payload)?;
            self.release_replaced(replacing)?;
            return Ok(Some(vec![Piece {
                first_key: None,
                page_id: written,
                hash,
            }]));
        }

        let mut applied = applied.into_iter().peekable();
        let mut children = Vec::with_capacity(count + 1);
        for child in 0..count {
            let separator = match child {
                0 => None,
                child => Some(node.internal_key(child - 1)?.to_vec()),
            };
            let Some((_, pieces)) = applied.next_if(|(index, _)| *index == child) else {
                children.push(Child {
                    separator,
                    page_id: node.child(child)?,
                    hash: node.child_hash(child)?,
                });
                continue;
            };
            for (index, piece) in pieces.into_iter().enumerate() {
                let separator = match piece.first_key {
                    Some(key) => Some(key),
                    None if index == 0 => separator.clone(),
                    None => return Err(invalid_btree("A divided B-tree page has no first key")),
                };
                children.push(Child {
                    separator,
                    page_id: piece.page_id,
                    hash: piece.hash,
                });
            }
        }
        if children.is_empty() {
            release_node_page(self.transaction, page_id, owned)?;
            return Ok(Some(Vec::new()));
        }
        // The first child is the leftmost. A separator it carries, from a removed child before it
        // or a first key that changed, is the node's new first key.
        let first_key = children[0].separator.take();
        let mut pieces = self.write_internals(Some((page_id, owned)), node.level, children)?;
        pieces[0].first_key = first_key;
        Ok(Some(pieces))
    }

    /// The cell value for an upserted value, stored in overflow pages when it is large. Nearly
    /// every value is small, so the test of its size is compiled into each place that makes a
    /// cell, and storing a large one is kept out of line: as one function, each small value paid
    /// for a call that set up everything a chain of overflow pages needs.
    #[inline(always)]
    fn new_value<'a>(&mut self, key: &[u8], value: &'a [u8]) -> Result<CellValue<'a>> {
        if value.len() <= MAX_BTREE_INLINE_VALUE_BYTES
            && key.len() + value.len() <= MAX_BTREE_INLINE_ENTRY_BYTES
        {
            return Ok(CellValue::Inline(value));
        }
        self.overflow_value(value)
    }

    /// Stores a value too large for a leaf cell in overflow pages.
    #[cold]
    #[inline(never)]
    fn overflow_value<'a>(&mut self, value: &'a [u8]) -> Result<CellValue<'a>> {
        store_overflow_value(self.transaction, self.tree_id, self.generation, value)
            .map(CellValue::Overflow)
    }

    /// Writes leaf cells into as many pages as they need, the first in place of `replacing`, and
    /// reports each page with its first key. A run of kept cells is copied from `source` as the
    /// bytes it lies in, which are what its cells copied one at a time would be: the merge that
    /// kept the run found each of its cells ending where the cell before it starts. When the
    /// cells fit one page and `hash` is given, it is that page's fingerprint; otherwise each
    /// page's is computed from its entries.
    fn write_leaf_cells(
        &mut self,
        replacing: Option<(PageId, bool)>,
        source: Option<&NodeView<'_>>,
        cells: &[LeafCell<'_>],
        fill: bool,
        hash: Option<u64>,
    ) -> Result<Vec<Piece>> {
        #[cfg(test)]
        if self.by_cell {
            // Only a batch into an empty tree comes here to be written cell by cell.
            let cells = cells
                .iter()
                .map(|cell| match cell {
                    LeafCell::Kept(_) => unreachable!("a batch into an empty tree keeps no cell"),
                    LeafCell::New { key, value } => CellByCell::New {
                        key,
                        value: match value {
                            CellValue::Inline(value) => CellValue::Inline(value),
                            CellValue::Overflow(descriptor) => {
                                CellValue::Overflow(descriptor.clone())
                            }
                        },
                    },
                })
                .collect::<Vec<_>>();
            return self.write_leaf_cells_by_cell(replacing, source, &cells, fill, hash);
        }
        // The cells' sizes, slots included. A first pass sums them a segment at a time, and stops
        // once they pass what a page holds: nearly every leaf is written back as one page, and
        // takes no more than that. Only for a leaf that divides does a second pass size each
        // cell, for `divide`.
        let mut sizes = Vec::new();
        let mut each = false;
        let (mut count, mut size);
        loop {
            (count, size) = (0, 0);
            for cell in cells {
                match cell {
                    LeafCell::Kept(run) => {
                        let step = if each { 1 } else { run.len() };
                        let mut start = run.start;
                        while start < run.end {
                            let (_, low, high) = NodeView::span(source, &(start..start + step))?;
                            let bytes = high - low + step * SLOT_SIZE;
                            if each {
                                sizes.push(bytes);
                            }
                            (count, size, start) = (count + step, size + bytes, start + step);
                        }
                    }
                    LeafCell::New { key, value } => {
                        let bytes =
                            SLOT_SIZE + LEAF_CELL_HEADER_SIZE + key.len() + value.encoded_len();
                        if each {
                            sizes.push(bytes);
                        }
                        (count, size) = (count + 1, size + bytes);
                    }
                }
                if !each && size > NODE_CAPACITY {
                    break;
                }
            }
            if each || size <= NODE_CAPACITY {
                break;
            }
            each = true;
        }
        let (single, divided);
        let divisions: &[usize] = if each {
            divided = divide(&sizes, fill)?;
            &divided
        } else {
            single = [count];
            &single
        };
        let known = hash.filter(|_| divisions.len() == 1);
        let mut pieces = Vec::with_capacity(divisions.len());
        // The next cell to write is in segment `next`, and when that is a run of which earlier
        // pages took cells, it is the one after the `taken` they took.
        let (mut next, mut taken) = (0, 0);
        for (index, &count) in divisions.iter().enumerate() {
            let mut payload = vec![0; MAX_PAGE_PAYLOAD_SIZE];
            let mut free_end = MAX_PAGE_PAYLOAD_SIZE;
            let mut hash = EMPTY_HASH;
            let mut first_key = Vec::new();
            let mut slot = 0;
            while slot < count {
                // The key of the cell written, which the page reports when it is its first.
                let first = slot == 0;
                let key = match &cells[next] {
                    LeafCell::Kept(run) => {
                        // As many of the run's remaining cells as the page still takes.
                        let start = run.start + taken;
                        let end = run.end.min(start + count - slot);
                        let (source, low, high) = NodeView::span(source, &(start..end))?;
                        let bytes = &*source.bytes;
                        free_end -= high - low;
                        payload[free_end..free_end + high - low].copy_from_slice(&bytes[low..high]);
                        // The cells keep their places in the bytes, so each slot moves as far as
                        // the bytes did.
                        for cell in start..end {
                            let offset =
                                u16_at(bytes, NODE_HEADER_SIZE + cell * SLOT_SIZE) as usize;
                            if offset < low || offset >= high {
                                return Err(source.cell_error(cell));
                            }
                            put_u16(
                                &mut payload,
                                NODE_HEADER_SIZE + (slot + cell - start) * SLOT_SIZE,
                                (offset - low + free_end) as u16,
                            );
                            if known.is_none() {
                                let (key, value) = source.leaf_cell(cell)?;
                                hash = combine(hash, cell_hash(key, &value));
                            }
                        }
                        let key = match first {
                            true => source.leaf_key(start)?,
                            false => &[],
                        };
                        slot += end - start;
                        if end == run.end {
                            (next, taken) = (next + 1, 0);
                        } else {
                            taken = end - run.start;
                        }
                        key
                    }
                    LeafCell::New { key, value } => {
                        free_end -= LEAF_CELL_HEADER_SIZE + key.len() + value.encoded_len();
                        write_leaf_cell(&mut payload[free_end..], key, value);
                        if known.is_none() {
                            hash = combine(hash, cell_hash(key, value));
                        }
                        put_u16(
                            &mut payload,
                            NODE_HEADER_SIZE + slot * SLOT_SIZE,
                            free_end as u16,
                        );
                        slot += 1;
                        next += 1;
                        key
                    }
                };
                if first {
                    first_key = key.to_vec();
                }
            }
            write_node_header(
                &mut payload,
                self.tree_id,
                self.generation,
                0,
                count,
                free_end,
                (NO_PAGE_ID, EMPTY_HASH),
            );
            let page_id = self.page_for(index, replacing)?;
            self.write_payload(page_id, PageType::BtreeLeaf, payload)?;
            pieces.push(Piece {
                first_key: Some(first_key),
                page_id,
                hash: known.unwrap_or(hash),
            });
        }
        self.release_replaced(replacing)?;
        Ok(pieces)
    }

    /// Writes leaf cells as [`Self::write_leaf_cells`] did before it copied kept cells in runs:
    /// it sizes, copies and reads again each kept cell of `source`, one at a time.
    #[cfg(test)]
    fn write_leaf_cells_by_cell(
        &mut self,
        replacing: Option<(PageId, bool)>,
        source: Option<&NodeView<'_>>,
        cells: &[CellByCell<'_>],
        fill: bool,
        hash: Option<u64>,
    ) -> Result<Vec<Piece>> {
        let kept = |index: usize| {
            source
                .ok_or_else(|| invalid_btree("A kept B-tree cell has no source page"))?
                .leaf_cell_bytes(index)
        };
        let mut sizes = Vec::with_capacity(cells.len());
        for cell in cells {
            sizes.push(
                SLOT_SIZE
                    + match cell {
                        CellByCell::Kept(index) => kept(*index)?.len(),
                        CellByCell::New { key, value } => {
                            LEAF_CELL_HEADER_SIZE + key.len() + value.encoded_len()
                        }
                    },
            );
        }
        let divisions = divide(&sizes, fill)?;
        let known = hash.filter(|_| divisions.len() == 1);
        let mut pieces = Vec::with_capacity(divisions.len());
        let mut start = 0;
        for (index, count) in divisions.into_iter().enumerate() {
            let mut payload = vec![0; MAX_PAGE_PAYLOAD_SIZE];
            let mut free_end = MAX_PAGE_PAYLOAD_SIZE;
            let mut hash = EMPTY_HASH;
            let mut first_key = Vec::new();
            for (slot, cell) in cells[start..start + count].iter().enumerate() {
                let key = match cell {
                    CellByCell::Kept(index) => {
                        let bytes = kept(*index)?;
                        free_end -= bytes.len();
                        payload[free_end..free_end + bytes.len()].copy_from_slice(bytes);
                        let (key, value) = source
                            .expect("a kept cell was read from its source above")
                            .leaf_cell(*index)?;
                        if known.is_none() {
                            hash = combine(hash, cell_hash(key, &value));
                        }
                        key
                    }
                    CellByCell::New { key, value } => {
                        free_end -= LEAF_CELL_HEADER_SIZE + key.len() + value.encoded_len();
                        write_leaf_cell(&mut payload[free_end..], key, value);
                        if known.is_none() {
                            hash = combine(hash, cell_hash(key, value));
                        }
                        key
                    }
                };
                if slot == 0 {
                    first_key = key.to_vec();
                }
                put_u16(
                    &mut payload,
                    NODE_HEADER_SIZE + slot * SLOT_SIZE,
                    free_end as u16,
                );
            }
            start += count;
            write_node_header(
                &mut payload,
                self.tree_id,
                self.generation,
                0,
                count,
                free_end,
                (NO_PAGE_ID, EMPTY_HASH),
            );
            let page_id = self.page_for(index, replacing)?;
            self.write_payload(page_id, PageType::BtreeLeaf, payload)?;
            pieces.push(Piece {
                first_key: Some(first_key),
                page_id,
                hash: known.unwrap_or(hash),
            });
        }
        self.release_replaced(replacing)?;
        Ok(pieces)
    }

    fn write_payload(
        &mut self,
        page_id: PageId,
        page_type: PageType,
        payload: Vec<u8>,
    ) -> Result<()> {
        self.transaction
            .write_new_page(&Page::new(page_id, page_type, payload)?)
    }

    /// Writes the children of an internal node at `level` into as many pages as they need, the
    /// first in place of `replacing`. The first child of every page after the first becomes its
    /// leftmost, and that page reports the child's separator as its first key.
    fn write_internals(
        &mut self,
        replacing: Option<(PageId, bool)>,
        level: u8,
        children: Vec<Child>,
    ) -> Result<Vec<Piece>> {
        let sizes = children
            .iter()
            .map(|child| {
                child
                    .separator
                    .as_ref()
                    .map_or(0, |key| SLOT_SIZE + INTERNAL_CELL_HEADER_SIZE + key.len())
            })
            .collect::<Vec<_>>();
        let mut children = children.into_iter();
        let mut pieces = Vec::new();
        for (index, count) in divide(&sizes, false)?.into_iter().enumerate() {
            let leftmost = children
                .next()
                .expect("every division holds at least one child");
            let mut entries = Vec::with_capacity(count - 1);
            for child in children.by_ref().take(count - 1) {
                entries.push(InternalEntry {
                    key: child
                        .separator
                        .ok_or_else(|| invalid_btree("A B-tree child has no separator"))?,
                    right_child: child.page_id,
                    child_hash: child.hash,
                });
            }
            let page_id = self.page_for(index, replacing)?;
            let hash = write_node(
                self.transaction,
                page_id,
                &Node::internal(
                    self.tree_id,
                    self.generation,
                    level,
                    leftmost.page_id,
                    leftmost.hash,
                    entries,
                ),
            )?;
            pieces.push(Piece {
                first_key: leftmost.separator,
                page_id,
                hash,
            });
        }
        self.release_replaced(replacing)?;
        Ok(pieces)
    }

    /// The page for the `index`th piece of a node: the node's own page for the first when the
    /// transaction owns it, and otherwise a new one.
    fn page_for(&mut self, index: usize, replacing: Option<(PageId, bool)>) -> Result<PageId> {
        match replacing {
            Some((page_id, true)) if index == 0 => Ok(page_id),
            _ => self.transaction.allocate_page(),
        }
    }

    /// Frees a committed page its new pieces replaced.
    fn release_replaced(&mut self, replacing: Option<(PageId, bool)>) -> Result<()> {
        match replacing {
            Some((page_id, false)) => self.transaction.free_shared_page(page_id),
            _ => Ok(()),
        }
    }

    /// Builds new levels above the pieces a batch left at `level` until one root holds them.
    fn root(&mut self, mut pieces: Vec<Piece>, mut level: u8) -> Result<(Option<PageId>, u64)> {
        loop {
            if pieces.len() <= 1 {
                return Ok(pieces.first().map_or((None, EMPTY_HASH), |piece| {
                    (Some(piece.page_id), piece.hash)
                }));
            }
            level = level
                .checked_add(1)
                .filter(|level| (*level as usize) < MAX_TREE_DEPTH)
                .ok_or_else(|| {
                    limit_error(format!(
                        "The B-tree exceeds the maximum depth of {MAX_TREE_DEPTH}"
                    ))
                })?;
            let children = pieces
                .into_iter()
                .enumerate()
                .map(|(index, piece)| Child {
                    separator: if index == 0 { None } else { piece.first_key },
                    page_id: piece.page_id,
                    hash: piece.hash,
                })
                .collect();
            pieces = self.write_internals(None, level, children)?;
        }
    }
}

/// Divides cells, given their sizes including slots, into pages, returning how many cells each
/// page takes. Cells that fit one page stay together. Otherwise `fill` packs each page before
/// starting the next, and without it cells are spread evenly over the fewest pages that hold them.
fn divide(sizes: &[usize], fill: bool) -> Result<Vec<usize>> {
    if sizes.iter().any(|size| *size > NODE_CAPACITY) {
        return Err(limit_error("A single B-tree cell cannot fit in a page"));
    }
    let total = sizes.iter().sum::<usize>();
    if total <= NODE_CAPACITY {
        return Ok(vec![sizes.len()]);
    }
    let mut pages = total.div_ceil(NODE_CAPACITY);
    loop {
        let target = if fill {
            NODE_CAPACITY
        } else {
            total.div_ceil(pages)
        };
        let mut counts = Vec::with_capacity(pages);
        let (mut count, mut size) = (0, 0);
        for cell in sizes {
            if count > 0 && size + cell > target {
                counts.push(count);
                (count, size) = (0, 0);
            }
            count += 1;
            size += cell;
        }
        counts.push(count);
        // Every page but the last stays at or under the target; the last takes the rest.
        if size <= NODE_CAPACITY {
            return Ok(counts);
        }
        pages += 1;
    }
}

#[cfg(test)]
struct PageSplit {
    separator: Vec<u8>,
    right_page_id: PageId,
    right_hash: u64,
    left_level: u8,
}

#[cfg(test)]
struct DeletedPage {
    page_id: Option<PageId>,
    /// The fingerprint of the surviving subtree, or `None` when no entry matched and the subtree
    /// is unchanged. A parent must leave the fingerprint it already holds in place in that case.
    hash: Option<u64>,
    removed: bool,
    first_key_changed: bool,
    first_key: Option<Vec<u8>>,
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn delete_recursive<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    page_id: PageId,
    tree_id: TreeId,
    generation: u64,
    key: &[u8],
    visited: &mut PageSet,
    depth: usize,
    expected_level: Option<u8>,
    parent_generation: Option<u64>,
) -> Result<DeletedPage> {
    if depth >= MAX_TREE_DEPTH {
        return Err(invalid_btree(storage_diagnostic!(
            "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
        )));
    }
    if !visited.insert(page_id) {
        return Err(invalid_btree(storage_diagnostic!(
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
                    hash: None,
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
                    hash: None,
                    removed: false,
                    first_key_changed: false,
                    first_key: None,
                });
            }
            let child_hash = child
                .hash
                .ok_or_else(|| invalid_btree("A child reported a removal without a fingerprint"))?;
            match child.page_id {
                Some(child_page_id) => {
                    internal.replace_child(child_index, child_page_id, child_hash)?;
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
                    internal.leftmost_child_hash = replacement.child_hash;
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
            hash: Some(EMPTY_HASH),
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
        hash: Some(materialized.hash),
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

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn insert_recursive<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    page_id: PageId,
    tree_id: TreeId,
    generation: u64,
    key: &[u8],
    value: &[u8],
    visited: &mut PageSet,
    depth: usize,
    expected_level: Option<u8>,
    parent_generation: Option<u64>,
) -> Result<InsertedPage> {
    if depth >= MAX_TREE_DEPTH {
        return Err(invalid_btree(storage_diagnostic!(
            "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
        )));
    }
    if !visited.insert(page_id) {
        return Err(invalid_btree(storage_diagnostic!(
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
            internal.replace_child(child_index, child.page_id, child.hash)?;
            if let Some(split) = child.split {
                if split.left_level + 1 != node_level {
                    return Err(invalid_btree(storage_diagnostic!(
                        "Child split level {} does not match parent level {}",
                        split.left_level,
                        node_level
                    )));
                }
                internal.entries.insert(
                    child_index,
                    InternalEntry {
                        key: split.separator,
                        right_child: split.right_page_id,
                        child_hash: split.right_hash,
                    },
                );
            }
        }
    }
    node.generation = generation;
    materialize_node(transaction, page_id, owned, node)
}

#[cfg(test)]
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
        let hash = write_node(transaction, page_id, &node)?;
        if !old_owned {
            transaction.free_shared_page(old_page_id)?;
        }
        return Ok(InsertedPage {
            page_id,
            hash,
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
    let left_hash = write_node(transaction, left_page_id, &left)?;
    let right_hash = write_node(transaction, right_page_id, &right)?;
    if !old_owned {
        transaction.free_shared_page(old_page_id)?;
    }
    Ok(InsertedPage {
        page_id: left_page_id,
        hash: left_hash,
        split: Some(PageSplit {
            separator,
            right_page_id,
            right_hash,
            left_level: level,
        }),
    })
}

#[cfg(test)]
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
                    internal.leftmost_child_hash,
                    left_entries,
                ),
                Node::internal(
                    node.tree_id,
                    node.generation,
                    node.level,
                    promoted.right_child,
                    promoted.child_hash,
                    right_entries,
                ),
                promoted.key,
            ))
        }
    }
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn internal_entries_size(entries: &[InternalEntry]) -> Result<usize> {
    entries.iter().try_fold(
        NODE_HEADER_SIZE + entries.len() * SLOT_SIZE,
        |size, entry| {
            size.checked_add(INTERNAL_CELL_HEADER_SIZE + entry.key.len())
                .ok_or_else(|| limit_error("B-tree node size overflowed"))
        },
    )
}

/// Writes one node and reports the fingerprint of the subtree it roots.
fn write_node<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    page_id: PageId,
    node: &Node,
) -> Result<u64> {
    let (page, subtree_hash) = node.encode(page_id)?;
    transaction.write_new_page(&page)?;
    Ok(subtree_hash)
}

#[cfg(test)]
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
    store_overflow_value(transaction, tree_id, generation, value).map(LeafValue::Overflow)
}

/// Writes a value to a chain of overflow pages and returns the descriptor a leaf cell holds.
fn store_overflow_value<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    tree_id: TreeId,
    generation: u64,
    value: &[u8],
) -> Result<OverflowDescriptor> {
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
    let checksum = checksum32(value);
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
    Ok(OverflowDescriptor {
        first_page_id: page_ids[0],
        generation,
        total_length: value.len() as u32,
        checksum,
    })
}

#[derive(Clone, Copy)]
struct PendingReclaimPage {
    page_id: PageId,
    depth: usize,
    expected_level: Option<u8>,
    parent_generation: Option<u64>,
}

fn reclaim_tree<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    root_page_id: PageId,
    tree_id: TreeId,
) -> Result<()> {
    let view_generation = transaction.generation()?;
    let mut pending = vec![PendingReclaimPage {
        page_id: root_page_id,
        depth: 0,
        expected_level: None,
        parent_generation: None,
    }];
    let mut reachable = PageSet::default();
    let mut pages = Vec::new();

    while let Some(current) = pending.pop() {
        if current.depth >= MAX_TREE_DEPTH {
            return Err(invalid_btree(storage_diagnostic!(
                "Tree {tree_id} exceeds the maximum depth of {MAX_TREE_DEPTH}"
            )));
        }
        if !reachable.insert(current.page_id) {
            return Err(invalid_btree(storage_diagnostic!(
                "Tree {tree_id} references page {} more than once",
                current.page_id
            )));
        }

        let owned = transaction.owns_page(current.page_id);
        let node = Node::decode_in_place(
            transaction.read_page_in_place(current.page_id)?,
            tree_id,
            view_generation,
            owned,
        )?;
        validate_expected_level(current.page_id, node.level, current.expected_level)?;
        validate_child_generation(current.page_id, node.generation, current.parent_generation)?;
        pages.push(current.page_id);

        match node.kind {
            NodeKind::Leaf(entries) => {
                for entry in entries {
                    let LeafValue::Overflow(descriptor) = entry.value else {
                        continue;
                    };
                    // Reclamation must validate the complete value, including its end-to-end
                    // checksum, before changing the candidate allocation bitmap.
                    let (_, overflow_pages) = read_overflow_chain(
                        &mut |page_id| transaction.read_page(page_id),
                        tree_id,
                        view_generation,
                        node.generation,
                        &descriptor,
                    )?;
                    for page_id in overflow_pages {
                        if !reachable.insert(page_id) {
                            return Err(invalid_btree(storage_diagnostic!(
                                "Tree {tree_id} references page {page_id} more than once"
                            )));
                        }
                        pages.push(page_id);
                    }
                }
            }
            NodeKind::Internal(internal) => {
                let expected_level = node.level.checked_sub(1).ok_or_else(|| {
                    invalid_btree(storage_diagnostic!(
                        "Internal B-tree page {} has no child level",
                        current.page_id
                    ))
                })?;
                for child_index in (0..=internal.entries.len()).rev() {
                    pending.push(PendingReclaimPage {
                        page_id: internal.child(child_index)?,
                        depth: current.depth + 1,
                        expected_level: Some(expected_level),
                        parent_generation: Some(node.generation),
                    });
                }
            }
        }
    }

    for page_id in pages {
        if transaction.owns_page(page_id) {
            transaction.release_new_page(page_id)?;
        } else {
            transaction.free_shared_page(page_id)?;
        }
    }
    Ok(())
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
        &mut |page_id| transaction.read_page(page_id),
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
    read_page: &mut dyn FnMut(PageId) -> Result<Page>,
    tree_id: TreeId,
    view_generation: u64,
    leaf_generation: u64,
    descriptor: &OverflowDescriptor,
) -> Result<(Vec<u8>, Vec<PageId>)> {
    validate_overflow_descriptor(descriptor, leaf_generation)?;
    if descriptor.generation > view_generation {
        return Err(invalid_overflow(storage_diagnostic!(
            "Overflow generation {} exceeds view generation {view_generation}",
            descriptor.generation
        )));
    }
    let chunk_count = (descriptor.total_length as usize).div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
    let mut value = Vec::with_capacity(descriptor.total_length as usize);
    let mut pages = Vec::with_capacity(chunk_count);
    let mut visited = PageSet::with_capacity_and_hasher(chunk_count, Default::default());
    let mut page_id = descriptor.first_page_id;
    for index in 0..chunk_count {
        if !visited.insert(page_id) {
            return Err(invalid_overflow(storage_diagnostic!(
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
                invalid_overflow(storage_diagnostic!(
                    "Overflow chain ended after page {page_id}"
                ))
            })?;
        }
    }
    if value.len() != descriptor.total_length as usize {
        return Err(invalid_overflow(storage_diagnostic!(
            "Overflow chain materialized {} bytes, not {}",
            value.len(),
            descriptor.total_length
        )));
    }
    if checksum32(&value) != descriptor.checksum {
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
    cell.extend_from_slice(&entry.child_hash.to_le_bytes());
    cell.extend_from_slice(&entry.key);
    Ok(cell)
}

/// Fingerprints one stored entry from its key and its logical value.
///
/// An overflow value is represented by the length and checksum already held in its descriptor, so
/// fingerprinting never reads an overflow chain. [`store_leaf_value`] chooses between the inline
/// and overflow representations from the key and value lengths alone, so two databases holding the
/// same entry always store it the same way and therefore always fingerprint it the same way.
fn leaf_entry_hash(entry: &LeafEntry) -> u64 {
    let value = match &entry.value {
        LeafValue::Inline(value) => CellValue::Inline(value),
        LeafValue::Overflow(descriptor) => CellValue::Overflow(descriptor.clone()),
    };
    cell_hash(&entry.key, &value)
}

/// Fingerprints one entry, as [`leaf_entry_hash`] does, from its key and the value its cell holds.
///
/// The XXH64 of the key, which covers its length, seeds the XXH64 of the value, so bytes cannot
/// move between the two without changing the result. An overflow value's length and checksum are
/// hashed from the complement of that seed, so they never pass for an inline value of those bytes.
///
/// The two hashes are two functions, [`key_hash`] and [`value_hash`], because replacing an
/// entry's value takes two fingerprints of one key, the held value's and the new one's. This is
/// their composition, compiled into its callers so that it adds no call of its own to theirs.
#[inline(always)]
fn cell_hash(key: &[u8], value: &CellValue<'_>) -> u64 {
    value_hash(key_hash(key), value)
}

/// The hash of an entry's key, which seeds the hash of its value.
#[inline(always)]
fn key_hash(key: &[u8]) -> u64 {
    xxh64(key, 0)
}

/// An entry's fingerprint from the hash of its key and the value its cell holds.
fn value_hash(key_hash: u64, value: &CellValue<'_>) -> u64 {
    match value {
        CellValue::Inline(value) => xxh64(value, key_hash),
        CellValue::Overflow(descriptor) => {
            let mut summary = [0; 8];
            summary[..4].copy_from_slice(&descriptor.total_length.to_le_bytes());
            summary[4..].copy_from_slice(&descriptor.checksum.to_le_bytes());
            xxh64(&summary, !key_hash)
        }
    }
}

/// Writes a leaf cell holding `key` and `value` at the start of `destination`.
fn write_leaf_cell(destination: &mut [u8], key: &[u8], value: &CellValue<'_>) {
    let flags = match value {
        CellValue::Inline(_) => INLINE_CELL_FLAGS,
        CellValue::Overflow(_) => OVERFLOW_CELL_FLAGS,
    };
    // The last field first, so that its bounds test is the only one the header's eight bytes take.
    put_u32(destination, 4, value.encoded_len() as u32);
    put_u16(destination, 2, flags);
    put_u16(destination, 0, key.len() as u16);
    let key_end = LEAF_CELL_HEADER_SIZE + key.len();
    destination[LEAF_CELL_HEADER_SIZE..key_end].copy_from_slice(key);
    match value {
        CellValue::Inline(value) => {
            // Every index entry's value is empty, and a copy of no bytes still costs its calls.
            if !value.is_empty() {
                destination[key_end..key_end + value.len()].copy_from_slice(value);
            }
        }
        CellValue::Overflow(descriptor) => {
            destination[key_end..key_end + 8]
                .copy_from_slice(&descriptor.first_page_id.to_le_bytes());
            destination[key_end + 8..key_end + 16]
                .copy_from_slice(&descriptor.generation.to_le_bytes());
            destination[key_end + 16..key_end + 20]
                .copy_from_slice(&descriptor.total_length.to_le_bytes());
            destination[key_end + 20..key_end + 24]
                .copy_from_slice(&descriptor.checksum.to_le_bytes());
        }
    }
}

/// Writes a node's header: `count` cells whose area starts at `free_end`, and for an internal
/// node, its leftmost child and that child's fingerprint.
fn write_node_header(
    payload: &mut [u8],
    tree_id: TreeId,
    generation: u64,
    level: u8,
    count: usize,
    free_end: usize,
    (leftmost_child, leftmost_child_hash): (PageId, u64),
) {
    payload[..4].copy_from_slice(NODE_MAGIC);
    payload[4..6].copy_from_slice(&NODE_FORMAT_VERSION.to_le_bytes());
    payload[6] = level;
    payload[7] = NODE_FLAGS;
    payload[8..16].copy_from_slice(&tree_id.to_le_bytes());
    payload[16..24].copy_from_slice(&generation.to_le_bytes());
    payload[24..26].copy_from_slice(&(count as u16).to_le_bytes());
    let free_start = NODE_HEADER_SIZE + count * SLOT_SIZE;
    payload[26..28].copy_from_slice(&(free_start as u16).to_le_bytes());
    payload[28..30].copy_from_slice(&(free_end as u16).to_le_bytes());
    payload[32..40].copy_from_slice(&leftmost_child.to_le_bytes());
    payload[40..48].copy_from_slice(&leftmost_child_hash.to_le_bytes());
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
                return Err(invalid_btree(storage_diagnostic!(
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
        return Err(invalid_btree(storage_diagnostic!(
            "Internal key length {key_length} exceeds {MAX_BTREE_KEY_BYTES}"
        )));
    }
    let key_end = checked_end(header_end, key_length, bytes.len())?;
    Ok((
        InternalEntry {
            key: bytes[header_end..key_end].to_vec(),
            right_child: read_u64(bytes, offset + 4),
            child_hash: read_u64(bytes, offset + 12),
        },
        key_end,
    ))
}

fn read_slot(bytes: &[u8], index: usize, free_start: usize) -> Result<usize> {
    let offset = NODE_HEADER_SIZE + index * SLOT_SIZE;
    if offset + SLOT_SIZE > free_start || offset + SLOT_SIZE > bytes.len() {
        return Err(invalid_btree(storage_diagnostic!(
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
        return Err(unpacked_cell(page_id, index));
    }
    Ok(())
}

/// The error for a cell that does not end where the cell before it starts, or with the payload
/// when it is the first. Decoding a node and rewriting a leaf both test every cell for it, so it
/// is built out of line.
#[cold]
#[inline(never)]
fn unpacked_cell(page_id: PageId, index: usize) -> EngineError {
    invalid_btree(storage_diagnostic!(
        "B-tree page {page_id} cell {index} is overlapping, out of order, or not tightly packed"
    ))
}

/// The error for a leaf a batch reaches whose keys do not increase from each cell to the next.
#[cold]
#[inline(never)]
fn unordered_leaf(page_id: PageId) -> EngineError {
    invalid_btree(storage_diagnostic!(
        "B-tree leaf {page_id} keys are not strictly increasing"
    ))
}

fn validate_cell_floor(page_id: PageId, actual: usize, expected: usize) -> Result<()> {
    if actual != expected {
        return Err(invalid_btree(storage_diagnostic!(
            "B-tree page {page_id} cell area starts at {actual}, not {expected}"
        )));
    }
    Ok(())
}

/// Where a cell field of `length` bytes at `offset` ends, if it ends within `bound`. Every cell
/// read checks its fields this way, so the check is inlined and its error kept out of line.
#[inline(always)]
fn checked_end(offset: usize, length: usize, bound: usize) -> Result<usize> {
    match offset.checked_add(length) {
        Some(end) if end <= bound => Ok(end),
        _ => Err(cell_range_error(offset, length, bound)),
    }
}

#[cold]
#[inline(never)]
fn cell_range_error(offset: usize, length: usize, bound: usize) -> EngineError {
    match offset.checked_add(length) {
        None => invalid_btree("B-tree cell offset overflowed"),
        Some(end) => invalid_btree(storage_diagnostic!(
            "B-tree cell range {offset}..{end} exceeds page payload {bound}"
        )),
    }
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
    let mut children = PageSet::with_capacity_and_hasher(entries.len() + 1, Default::default());
    children.insert(leftmost_child);
    for entry in entries {
        validate_key(&entry.key)?;
        validate_child_page(page_id, entry.right_child)?;
        if !children.insert(entry.right_child) {
            return Err(invalid_btree(storage_diagnostic!(
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

/// Refuses a key longer than a cell may hold. Every key of a batch and of a lookup passes this
/// test, so it is compiled into its callers, and the error is built out of line.
#[inline(always)]
fn validate_key(key: &[u8]) -> Result<()> {
    if key.len() > MAX_BTREE_KEY_BYTES {
        key_limit_exceeded(key.len())
    } else {
        Ok(())
    }
}

/// The refusal of [`validate_key`], as the result its callers test: were it a bare error, each of
/// them would hold the code that wraps it.
#[cold]
#[inline(never)]
fn key_limit_exceeded(length: usize) -> Result<()> {
    Err(limit_error(format!(
        "B-tree key is {length} bytes, exceeding {MAX_BTREE_KEY_BYTES}"
    )))
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
        return Err(invalid_overflow(storage_diagnostic!(
            "Overflow generation {} is outside leaf generation {leaf_generation}",
            descriptor.generation
        )));
    }
    let total_length = descriptor.total_length as usize;
    if total_length == 0 || total_length > MAX_BTREE_VALUE_BYTES {
        return Err(invalid_overflow(storage_diagnostic!(
            "Overflow total length {total_length} is outside 1..={MAX_BTREE_VALUE_BYTES}"
        )));
    }
    let chunk_count = total_length.div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
    if chunk_count == 0 || chunk_count > MAX_OVERFLOW_PAGE_COUNT {
        return Err(invalid_overflow(storage_diagnostic!(
            "Overflow descriptor requires invalid chunk count {chunk_count}"
        )));
    }
    Ok(())
}

fn validate_overflow_page_id(page_id: PageId) -> Result<()> {
    if !(FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT).contains(&page_id) {
        return Err(invalid_overflow(storage_diagnostic!(
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
        return Err(invalid_overflow(storage_diagnostic!(
            "Overflow chunk index {chunk_index} is outside count {chunk_count}"
        )));
    }
    let expected_count = total_length.div_ceil(MAX_OVERFLOW_CHUNK_BYTES);
    if expected_count != chunk_count {
        return Err(invalid_overflow(storage_diagnostic!(
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
        return Err(invalid_btree(storage_diagnostic!(
            "Decoded inline entry lengths {key_length}+{value_length} exceed the supported bounds"
        )));
    }
    Ok(())
}

fn validate_child_page(parent_page_id: PageId, child_page_id: PageId) -> Result<()> {
    if !(FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT).contains(&child_page_id) {
        return Err(invalid_btree(storage_diagnostic!(
            "Internal page {parent_page_id} references invalid child page {child_page_id}"
        )));
    }
    if child_page_id == parent_page_id {
        return Err(invalid_btree(storage_diagnostic!(
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
        return Err(invalid_btree(storage_diagnostic!(
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
        return Err(invalid_btree(storage_diagnostic!(
            "B-tree page {page_id} generation {generation} is newer than its parent generation {parent_generation}"
        )));
    }
    Ok(())
}

/// The order of a cell's key and another key, as the slices order, decided without a call when
/// their first eight bytes decide it: when they differ there, or are those eight bytes and no
/// more, as the key of an integer is. The lowest byte in which two little-endian words differ
/// is the first byte in which the keys do. A lookup's search of a node compares each key it
/// probes through this, and the walk of a leaf a batch rewrites compares every cell's twice,
/// where a comparison of slices is a call that makes two more.
#[inline(always)]
fn key_order(cell: &[u8], key: &[u8]) -> Ordering {
    if let (Some(cell_start), Some(key_start)) = (cell.first_chunk::<8>(), key.first_chunk::<8>()) {
        let (a, b) = (
            u64::from_le_bytes(*cell_start),
            u64::from_le_bytes(*key_start),
        );
        if a != b {
            let shift = (a ^ b).trailing_zeros() & 56;
            return ((a >> shift) as u8).cmp(&((b >> shift) as u8));
        }
        if cell.len() == 8 && key.len() == 8 {
            return Ordering::Equal;
        }
    }
    cell.cmp(key)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16_at(bytes, offset)
}

/// Reads a little-endian number byte by byte, compiled into its caller: the cell reader a scan
/// runs for every row reads several, and a copy of a slice into an array would be a call. The
/// readers above and below stay calls: compiled into the many places that read a number once,
/// they cost 2.6 KiB of compressed code for a gain of about a percent.
#[inline(always)]
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[inline(always)]
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Writes `value` at `offset` as one store of its size, where copying it from a slice would call
/// `memcpy` for a few bytes. Taking the slice and making an array of it are still two calls. An
/// internal node pays them for each child reference a batch patches into it; a cell's fields and
/// its slot, which every row written would pay for, are stored by [`put_u16`] and [`put_u32`].
#[inline(always)]
fn put<const N: usize>(bytes: &mut [u8], offset: usize, value: [u8; N]) {
    *<&mut [u8; N]>::try_from(&mut bytes[offset..offset + N]).expect("bounded write") = value;
}

/// Writes a little-endian number byte by byte, compiled into its caller with no slice taken, as
/// [`u16_at`] reads one. A leaf is written with a slot and three header fields for each of its
/// cells, so a call for each would be several for every row a commit writes. The last byte is
/// stored first: where the offset is a constant, its bounds test is then the only one made.
#[inline(always)]
fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    let value = value.to_le_bytes();
    bytes[offset + 1] = value[1];
    bytes[offset] = value[0];
}

#[inline(always)]
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    let value = value.to_le_bytes();
    bytes[offset + 3] = value[3];
    bytes[offset + 2] = value[2];
    bytes[offset + 1] = value[1];
    bytes[offset] = value[0];
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32_at(bytes, offset)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from(u32_at(bytes, offset)) | (u64::from(u32_at(bytes, offset + 4)) << 32)
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
    use std::collections::{BTreeMap, HashMap};

    use super::*;
    use crate::{MemoryPageDevice, PageDevice};

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

    /// The pager's device, without the root its active root replaced. These tests alter the
    /// active root's pages or metadata on purpose; recovery would otherwise take that for an
    /// interrupted commit and reopen the predecessor instead.
    fn into_device_without_predecessor(pager: Pager<MemoryPageDevice>) -> MemoryPageDevice {
        let inactive = pager.active_metadata().superblock.slot.inactive();
        let mut device = pager.into_device();
        device
            .write_page(inactive.page_id(), &[0; crate::PAGE_SIZE])
            .unwrap();
        device
    }

    fn create_tree(pager: &mut Pager<MemoryPageDevice>) -> PageId {
        let mut transaction = pager.begin_write().unwrap();
        let root = Btree::create(&mut transaction, TREE).unwrap();
        transaction.commit(1, EMPTY_HASH, Some(root)).unwrap();
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
        let root = Btree::upsert(&mut transaction, root, TREE, key, value)
            .unwrap()
            .root_page_id;
        transaction
            .commit(revision, EMPTY_HASH, Some(root))
            .unwrap();
        root
    }

    /// Walks a committed tree and checks the invariant a future synchronization descent needs:
    /// every internal cell records the fingerprint of the child it points at. Returns the
    /// fingerprint of the subtree rooted at `page_id`.
    fn verified_subtree_hash(pager: &mut Pager<MemoryPageDevice>, page_id: PageId) -> u64 {
        let generation = pager.generation();
        let node =
            Node::decode(pager.read_page(page_id).unwrap(), TREE, generation, false).unwrap();
        if let NodeKind::Internal(internal) = &node.kind {
            assert_eq!(
                verified_subtree_hash(pager, internal.leftmost_child),
                internal.leftmost_child_hash,
                "internal page {page_id} misreports its leftmost child"
            );
            for entry in &internal.entries {
                assert_eq!(
                    verified_subtree_hash(pager, entry.right_child),
                    entry.child_hash,
                    "internal page {page_id} misreports child {}",
                    entry.right_child
                );
            }
        }
        node.subtree_hash()
    }

    fn page_count_of(pager: &mut Pager<MemoryPageDevice>, page_id: PageId) -> usize {
        let generation = pager.generation();
        let node =
            Node::decode(pager.read_page(page_id).unwrap(), TREE, generation, false).unwrap();
        match &node.kind {
            NodeKind::Leaf(_) => 1,
            NodeKind::Internal(internal) => {
                let mut total = 1 + page_count_of(pager, internal.leftmost_child);
                for entry in &internal.entries {
                    total += page_count_of(pager, entry.right_child);
                }
                total
            }
        }
    }

    /// The fingerprint the tree must produce, computed from the entries alone.
    fn expected_hash(entries: &[(Vec<u8>, Vec<u8>)]) -> u64 {
        entries.iter().fold(EMPTY_HASH, |hash, (key, value)| {
            combine(
                hash,
                leaf_entry_hash(&LeafEntry {
                    key: key.clone(),
                    value: LeafValue::Inline(value.clone()),
                }),
            )
        })
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
        let mut active = pager.active_metadata().clone();
        let mut memory = into_device_without_predecessor(pager);
        for id in FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT {
            active.allocation_bitmap.set_allocated(id, true).unwrap();
        }
        for id in free_pages {
            active.allocation_bitmap.set_allocated(*id, false).unwrap();
        }
        active.superblock.live_data_page_count =
            (MAX_PAGE_COUNT - FIRST_DATA_PAGE_ID - free_pages.len() as u64) as u32;
        for (id, page) in active.encode_pages().unwrap() {
            memory.write_page(id, &page).unwrap();
        }

        let physical_count = memory.page_count();
        let mut pages = HashMap::new();
        for id in 0..physical_count {
            let mut bytes = [0; crate::PAGE_SIZE];
            memory.read_page(id, &mut bytes).unwrap();
            pages.insert(id, bytes);
        }
        Pager::open_or_create(SparseDevice { pages }).unwrap()
    }

    /// Every entry of a committed tree, in key order.
    fn all_entries(
        pager: &mut Pager<MemoryPageDevice>,
        root: Option<PageId>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Some(root) = root else {
            return Vec::new();
        };
        let mut cursor = Btree::validating_cursor(pager, root, TREE).unwrap();
        let mut entries = Vec::new();
        while let Some(entry) = cursor.next(pager).unwrap() {
            entries.push(entry);
        }
        entries
    }

    /// The same numbers on every run.
    struct Random(u64);

    impl Random {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound as u64) as usize
        }
    }

    /// A change to one key: the value it takes, or `None` to delete it.
    type OwnedChange = (Vec<u8>, Option<Vec<u8>>);

    /// A batch for the `round`th commit of a tree: upserts and deletes in key order, of keys
    /// `key_bytes` long, which decides how many entries share a page, with values that are
    /// inline, empty or long enough for overflow pages.
    fn random_changes(random: &mut Random, key_bytes: usize, round: usize) -> Vec<OwnedChange> {
        let key = |number: usize| {
            let mut key = vec![b'k'; key_bytes];
            key[..4].copy_from_slice(&(number as u32).to_be_bytes());
            key
        };
        let value = |random: &mut Random| match random.below(10) {
            0 => vec![b'o'; 3_000 + random.below(9_000)],
            1 => Vec::new(),
            _ => vec![random.below(256) as u8; random.below(200)],
        };
        let count = [1, 7, 60, 400][random.below(4)];
        let mut numbers = (0..count)
            .map(|_| random.below(if round == 0 { 400 } else { 600 }))
            .collect::<Vec<_>>();
        numbers.sort_unstable();
        numbers.dedup();
        numbers
            .iter()
            .map(|number| (key(*number), (random.below(4) != 0).then(|| value(random))))
            .collect()
    }

    fn as_batch(changes: &[OwnedChange]) -> Vec<BatchChange<'_>> {
        changes
            .iter()
            .map(|(key, value)| BatchChange {
                key,
                value: value.as_deref(),
            })
            .collect()
    }

    #[test]
    fn batches_agree_with_single_changes_and_leak_no_pages() {
        let mut random = Random(0x0ba7c4);
        for case in 0..60 {
            let key_bytes = [4, 24, 300, 900][case % 4];
            let mut batched = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
            let mut single = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
            let (mut batched_root, mut single_root) = (None, None);
            let (mut batched_hash, mut single_hash) = (EMPTY_HASH, EMPTY_HASH);
            let mut model = BTreeMap::new();
            for round in 0..4 {
                let changes = random_changes(&mut random, key_bytes, round);
                let batch = as_batch(&changes);

                let revision = round as u64 + 2;
                let mut transaction = batched.begin_write().unwrap();
                let applied = Btree::apply(&mut transaction, batched_root, TREE, &batch).unwrap();
                batched_root = applied.root_page_id;
                batched_hash = applied.hash.unwrap_or(batched_hash);
                transaction
                    .commit(revision, EMPTY_HASH, batched_root)
                    .unwrap();

                let mut transaction = single.begin_write().unwrap();
                let (mut inserted, mut removed) = (0, 0);
                for (key, value) in &changes {
                    match value {
                        Some(value) => {
                            let root = match single_root {
                                Some(root) => root,
                                None => Btree::create(&mut transaction, TREE).unwrap(),
                            };
                            let upserted =
                                Btree::upsert(&mut transaction, root, TREE, key, value).unwrap();
                            single_root = Some(upserted.root_page_id);
                            single_hash = upserted.hash;
                            if model.insert(key.clone(), value.clone()).is_none() {
                                inserted += 1;
                            }
                        }
                        None => {
                            if let Some(root) = single_root {
                                let deleted =
                                    Btree::delete(&mut transaction, root, TREE, key).unwrap();
                                single_root = deleted.root_page_id;
                                single_hash = deleted.hash.unwrap_or(single_hash);
                            }
                            if model.remove(key).is_some() {
                                removed += 1;
                            }
                        }
                    }
                }
                transaction
                    .commit(revision, EMPTY_HASH, single_root)
                    .unwrap();

                let context = format!("case {case}, round {round}, {} changes", changes.len());
                assert_eq!(applied.inserted, inserted, "{context}");
                assert_eq!(applied.removed, removed, "{context}");
                let expected = model
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<Vec<_>>();
                assert_eq!(
                    all_entries(&mut batched, batched_root),
                    expected,
                    "{context}"
                );
                assert_eq!(all_entries(&mut single, single_root), expected, "{context}");
                assert_eq!(batched_hash, single_hash, "{context}");
                if let Some(root) = batched_root {
                    assert_eq!(
                        verified_subtree_hash(&mut batched, root),
                        batched_hash,
                        "{context}"
                    );
                }
            }
            // Reclaiming the tree must free every page the batches left allocated.
            if let Some(root) = batched_root {
                let mut transaction = batched.begin_write().unwrap();
                Btree::reclaim(&mut transaction, root, TREE).unwrap();
                transaction.commit(10, EMPTY_HASH, None).unwrap();
            }
            assert_eq!(
                batched.active_metadata().superblock.live_data_page_count,
                0,
                "case {case}"
            );
            let reopened = Pager::open_or_create(batched.into_device()).unwrap();
            assert_eq!(
                reopened.active_metadata().superblock.live_data_page_count,
                0
            );
        }
    }

    /// Two pagers that hold one tree and are kept in step: a batch rewrites the leaves of the
    /// first by runs of kept cells, as [`Btree::apply`] does, and those of the second cell by
    /// cell, as [`Btree::apply_by_cell`] does.
    struct Rewrites {
        by_runs: Option<Pager<MemoryPageDevice>>,
        by_cell: Option<Pager<MemoryPageDevice>>,
        root: Option<PageId>,
        revision: u64,
    }

    impl Rewrites {
        /// Two pagers with the tree that `changes` build, each applied by [`Btree::upsert`] or
        /// [`Btree::delete`], which encode every leaf they touch as releases before v0.4.0 did.
        fn of_single_changes(changes: &[OwnedChange]) -> Self {
            let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
            let mut root = None;
            let mut transaction = pager.begin_write().unwrap();
            for (key, value) in changes {
                match (value, root) {
                    (Some(value), held) => {
                        let held = held.unwrap_or_else(|| {
                            Btree::create(&mut transaction, TREE).expect("an empty tree")
                        });
                        let upserted = Btree::upsert(&mut transaction, held, TREE, key, value);
                        root = Some(upserted.unwrap().root_page_id);
                    }
                    (None, Some(held)) => {
                        let deleted = Btree::delete(&mut transaction, held, TREE, key);
                        root = deleted.unwrap().root_page_id;
                    }
                    (None, None) => {}
                }
            }
            transaction.commit(1, EMPTY_HASH, root).unwrap();
            let device = pager.into_device();
            Self {
                by_runs: Some(Pager::open_or_create(device.clone()).unwrap()),
                by_cell: Some(Pager::open_or_create(device).unwrap()),
                root,
                revision: 1,
            }
        }

        /// Applies `changes` to both trees and commits them, and asserts that the two rewrites
        /// report the same, leave every page of their devices the same, and wrote their pages in
        /// the same order. Returns what they reported.
        fn apply(&mut self, changes: &[OwnedChange], context: &str) -> BtreeBatch {
            let batch = as_batch(changes);
            self.revision += 1;
            let mut applied = Vec::new();
            for (pager, by_cell) in [(&mut self.by_runs, false), (&mut self.by_cell, true)] {
                let pager = pager
                    .as_mut()
                    .expect("both pagers are open between batches");
                let mut transaction = pager.begin_write().unwrap();
                let result = match by_cell {
                    false => Btree::apply(&mut transaction, self.root, TREE, &batch),
                    true => Btree::apply_by_cell(&mut transaction, self.root, TREE, &batch),
                };
                let result = result.unwrap_or_else(|error| panic!("{context}: {error:?}"));
                transaction
                    .commit(self.revision, EMPTY_HASH, result.root_page_id)
                    .unwrap();
                applied.push(result);
            }
            assert_eq!(applied[0], applied[1], "{context}");
            self.root = applied[0].root_page_id;

            // The devices show every page, so the pagers give them up and open them again.
            let by_runs = self.by_runs.take().unwrap().into_device();
            let by_cell = self.by_cell.take().unwrap().into_device();
            assert_eq!(by_runs.page_count(), by_cell.page_count(), "{context}");
            for id in 0..by_runs.page_count() {
                assert!(
                    by_runs.page(id).unwrap() == by_cell.page(id).unwrap(),
                    "{context}: page {id} differs"
                );
            }
            assert_eq!(by_runs.writes(), by_cell.writes(), "{context}");
            let mut by_runs = Pager::open_or_create(by_runs).unwrap();
            // Every node of the tree still decodes with every check.
            all_entries(&mut by_runs, self.root);
            self.by_runs = Some(by_runs);
            self.by_cell = Some(Pager::open_or_create(by_cell).unwrap());
            applied[0]
        }
    }

    /// Builds the tree of `held` both ways a tree is built, by one batch and by single changes,
    /// and applies `batches` to each, by runs and by cell. Returns what each batch reported, for
    /// the tree one batch built and then for the tree single changes built.
    fn compare_rewrites(
        name: &str,
        held: &[OwnedChange],
        batches: &[Vec<OwnedChange>],
    ) -> Vec<BtreeBatch> {
        let mut reported = Vec::new();
        for singly in [false, true] {
            let built = if singly { "single changes" } else { "a batch" };
            let mut rewrites = Rewrites::of_single_changes(if singly { held } else { &[] });
            if !singly {
                rewrites.apply(held, &format!("{name}: the tree built by {built}"));
            }
            for (index, batch) in batches.iter().enumerate() {
                let context = format!("{name}: batch {index} on a tree built by {built}");
                reported.push(rewrites.apply(batch, &context));
            }
        }
        reported
    }

    #[test]
    fn leaves_rewritten_by_runs_match_leaves_rewritten_by_cell() {
        // The property test's batches: keys of four lengths, values inline, empty and in overflow
        // pages, upserts and deletes. The first batch builds the tree and the rest change it.
        let mut random = Random(0x0ba7c4);
        for case in 0..60 {
            let key_bytes = [4, 24, 300, 900][case % 4];
            let batches = (0..4)
                .map(|round| random_changes(&mut random, key_bytes, round))
                .collect::<Vec<_>>();
            compare_rewrites(&format!("case {case}"), &batches[0], &batches[1..]);
        }

        // A tree of several leaves, with room between its keys. Every seventh value is in
        // overflow pages and every eleventh is empty, so every leaf keeps cells of each kind.
        let key = |number: u32| {
            let mut key = vec![b'k'; 24];
            key[..4].copy_from_slice(&number.to_be_bytes());
            key
        };
        let inline = |number: u32| vec![number as u8; 40 + number as usize % 30];
        let value = |number: u32| match number {
            number if number % 7 == 0 => vec![number as u8; 5_000 + number as usize],
            number if number % 11 == 0 => Vec::new(),
            number => inline(number),
        };
        let numbers = || (1..=300).map(|number| number * 100);
        let held = numbers()
            .map(|number| (key(number), Some(value(number / 100))))
            .collect::<Vec<_>>();
        // Whichever way the tree was built, a batch changes the same entries.
        let compare = |name: &str, batch: Vec<OwnedChange>| {
            let reported = compare_rewrites(name, &held, &[batch]);
            let entries = |batch: &BtreeBatch| (batch.hash, batch.inserted, batch.removed);
            assert_eq!(entries(&reported[0]), entries(&reported[1]), "{name}");
            reported[0]
        };

        compare(
            "every cell replaced",
            numbers()
                .map(|number| (key(number), Some(inline(number / 100 + 1))))
                .collect(),
        );
        compare(
            "every other cell replaced or removed",
            numbers()
                .step_by(2)
                .map(|number| (key(number), (number % 400 != 100).then(|| inline(number))))
                .collect(),
        );
        compare("one cell replaced", vec![(key(15_000), Some(inline(1)))]);
        compare("one cell removed", vec![(key(15_000), None)]);
        // The leftmost leaf takes a first key, and loses the one it had: its parents are rewritten.
        compare("a key below the first", vec![(key(50), Some(inline(2)))]);
        compare("the first key removed", vec![(key(100), None)]);
        // Enough below the first key to divide the leftmost leaf within the run of cells it
        // keeps, so that its second page starts with a kept cell.
        compare(
            "keys below the first",
            (1..=25)
                .map(|number| (key(number), Some(inline(number))))
                .collect(),
        );
        // Enough appends to divide the rightmost leaf, which keeps cells with overflow values.
        compare(
            "appends past the last key",
            (1..=120)
                .map(|number| (key(30_000 + number), Some(inline(number))))
                .collect(),
        );
        // Enough between two cells to divide their leaf within the runs it keeps.
        compare(
            "upserts between two cells",
            (1..=60)
                .map(|number| (key(15_000 + number), Some(inline(number))))
                .collect(),
        );
        // More than a leaf holds, so that at least one leaf loses every cell and is released.
        let emptied = compare(
            "every cell of a leaf removed",
            (100..=200)
                .map(|number| (key(number * 100), None))
                .collect(),
        );
        assert_eq!(emptied.removed, 101);
        let emptied = compare(
            "every cell of the tree removed",
            numbers().map(|number| (key(number), None)).collect(),
        );
        assert_eq!((emptied.root_page_id, emptied.removed), (None, 300));
        // Deletes of keys the tree lacks stop on cells they leave, between upserts that add.
        compare(
            "keys the tree lacks",
            vec![
                (key(150), None),
                (key(250), Some(inline(3))),
                (key(15_050), None),
                (key(99_999), None),
            ],
        );
        // An upsert of the inline value an entry holds changes nothing, so a batch of them
        // reports no fingerprint and leaves the tree's pages as they were.
        let unchanged = numbers()
            .filter(|number| number / 100 % 7 != 0)
            .map(|number| (key(number), Some(value(number / 100))))
            .collect::<Vec<_>>();
        let reported = compare("the values the entries hold", unchanged.clone());
        assert_eq!(
            (reported.hash, reported.inserted, reported.removed),
            (None, 0, 0)
        );
        // With one entry changed among them, the kept cells around it join into runs.
        let mut nearly = unchanged;
        nearly[100].1 = Some(inline(4));
        assert!(
            compare("all but one value the entries hold", nearly)
                .hash
                .is_some()
        );

        // A leaf that is the root: its fingerprint is computed from its cells, kept ones too, and
        // the tree has no root once a batch removes them all.
        let reported = compare_rewrites(
            "a root leaf",
            &held[..5],
            &[
                vec![(key(250), Some(inline(5))), (key(300), Some(inline(6)))],
                [100, 200, 250, 300, 400, 500]
                    .map(|number| (key(number), None))
                    .to_vec(),
            ],
        );
        for batches in reported.chunks(2) {
            assert_eq!((batches[0].inserted, batches[0].removed), (1, 0));
            assert_eq!((batches[1].root_page_id, batches[1].removed), (None, 6));
        }
        // A root leaf with keys added below its first and above its last, enough for three pages:
        // the run of cells it keeps starts on the first page and ends on the third.
        let reported = compare_rewrites(
            "a root leaf divided in three",
            &held[..40],
            &[(1..=25)
                .chain(30_001..=30_030)
                .map(|number| (key(number), Some(inline(number))))
                .collect()],
        );
        assert_eq!(reported[0].inserted, 55);
        // A batch into an empty tree fills each page before it starts the next.
        compare_rewrites("an empty tree", &[], std::slice::from_ref(&held));
        compare_rewrites(
            "an empty tree and a batch of deletes",
            &[],
            &[vec![(key(1), None)]],
        );
    }

    #[test]
    fn kept_cells_join_the_run_they_follow() {
        let mut cells = Vec::new();
        keep(&mut cells, 0..2);
        // No cell is no run, and cells that follow a run extend it.
        keep(&mut cells, 2..2);
        keep(&mut cells, 2..5);
        // A run starts behind a new cell, and behind a cell that was not kept.
        cells.push(LeafCell::New {
            key: b"key",
            value: CellValue::Inline(b"value"),
        });
        keep(&mut cells, 5..6);
        keep(&mut cells, 7..9);
        keep(&mut cells, 9..10);
        let runs = cells.iter().map(|cell| match cell {
            LeafCell::Kept(run) => Some(run.clone()),
            LeafCell::New { .. } => None,
        });
        assert_eq!(
            runs.collect::<Vec<_>>(),
            [Some(0..5), None, Some(5..6), Some(7..10)]
        );
    }

    /// The offset of cell `index` of the leaf in `payload`.
    fn cell_at(payload: &[u8], index: usize) -> usize {
        read_u16(payload, NODE_HEADER_SIZE + index * SLOT_SIZE) as usize
    }

    /// The key of the one entry in the leaf beside the leaf [`damaged_leaf`] damages.
    const SIBLING: u8 = 200;

    /// A device whose tree is a root over two leaves, and the root's page. The first leaf holds
    /// each of `keys`, which are below [`SIBLING`], as a one-byte key with a value of four bytes,
    /// and `damage` is done to its payload: it passes its page checksum, and no release writes
    /// it. Its parent records a fingerprint for it, so a batch that rewrites it by runs has no
    /// cause to read its kept cells again, as it reads those of a leaf that is the root.
    fn damaged_leaf(keys: &[u8], damage: impl FnOnce(&mut [u8])) -> (MemoryPageDevice, PageId) {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        let generation = transaction.generation().unwrap();
        let leaf_of = |keys: &[u8]| {
            let entries = keys.iter().map(|key| LeafEntry {
                key: vec![*key],
                value: LeafValue::Inline(vec![*key; 4]),
            });
            Node::leaf(TREE, generation, entries.collect())
        };
        let [leaf, sibling, root] = [(); 3].map(|()| transaction.allocate_page().unwrap());
        let (mut page, hash) = leaf_of(keys).encode(leaf).unwrap();
        damage(&mut page.payload);
        transaction.write_new_page(&page).unwrap();
        let sibling_hash = write_node(&mut transaction, sibling, &leaf_of(&[SIBLING])).unwrap();
        let above = InternalEntry {
            key: vec![SIBLING],
            right_child: sibling,
            child_hash: sibling_hash,
        };
        let node = Node::internal(TREE, generation, 1, leaf, hash, vec![above]);
        write_node(&mut transaction, root, &node).unwrap();
        transaction.commit(1, EMPTY_HASH, Some(root)).unwrap();
        (pager.into_device(), root)
    }

    /// Exchanges the keys of cells `left` and `right` of a leaf [`damaged_leaf`] encoded.
    fn exchange_keys(payload: &mut [u8], left: usize, right: usize) {
        let (left, right) = (cell_at(payload, left), cell_at(payload, right));
        payload.swap(left + LEAF_CELL_HEADER_SIZE, right + LEAF_CELL_HEADER_SIZE);
    }

    /// Exchanges the places of cells 2 and 3 of a leaf [`damaged_leaf`] encoded, which are as
    /// long as each other, and their slots: the keys stay in order, and the cells no longer lie
    /// in the order of their slots.
    fn unpack(payload: &mut [u8]) {
        let (left, right) = (cell_at(payload, 2), cell_at(payload, 3));
        for byte in 0..left - right {
            payload.swap(left + byte, right + byte);
        }
        let slots = NODE_HEADER_SIZE + 2 * SLOT_SIZE;
        payload.swap(slots, slots + SLOT_SIZE);
        payload.swap(slots + 1, slots + SLOT_SIZE + 1);
    }

    /// What a batch of `changes` to the first leaf of `device` comes to when it rewrites the leaf
    /// by runs of kept cells, and when it rewrites it cell by cell: what the batch reported with
    /// the keys the leaf then holds, or the code of the error.
    fn rewritten(
        (device, root): &(MemoryPageDevice, PageId),
        changes: &[(u8, Option<&[u8]>)],
    ) -> [std::result::Result<(BtreeBatch, Vec<u8>), String>; 2] {
        let changes = changes
            .iter()
            .map(|(key, value)| (vec![*key], value.map(<[u8]>::to_vec)))
            .collect::<Vec<_>>();
        [false, true].map(|by_cell| {
            let mut pager = Pager::open_or_create(device.clone()).unwrap();
            let mut transaction = pager.begin_write().unwrap();
            let batch = as_batch(&changes);
            let applied = match by_cell {
                false => Btree::apply(&mut transaction, Some(*root), TREE, &batch),
                true => Btree::apply_by_cell(&mut transaction, Some(*root), TREE, &batch),
            };
            let applied = match applied {
                Ok(applied) => applied,
                Err(error) => {
                    transaction.abort();
                    return Err(error.code);
                }
            };
            transaction
                .commit(2, EMPTY_HASH, applied.root_page_id)
                .unwrap();
            // A batch that left the leaf as it was left its damage, which a cursor would refuse.
            // One that wrote the leaf must have left a leaf that decodes with every check.
            let mut keys = Vec::new();
            if applied.hash.is_some() {
                let root = applied.root_page_id.expect("the sibling keeps its entry");
                let mut read = || {
                    let mut cursor = Btree::validating_cursor(&mut pager, root, TREE)?;
                    while let Some((key, _)) = cursor.next(&mut pager)? {
                        keys.push(key[0]);
                    }
                    Ok(())
                };
                if let Err::<(), EngineError>(error) = read() {
                    return Err(format!("wrote a leaf that reads as {}", error.code));
                }
                assert_eq!(keys.pop(), Some(SIBLING));
            }
            Ok((applied, keys))
        })
    }

    #[test]
    fn a_leaf_rewrite_refuses_what_a_cell_by_cell_rewrite_refuses() {
        let keys = [1, 2, 3, 4, 5, 6, 7, 8];
        let refused = |code: &str| [Err(code.to_string()), Err(code.to_string())];
        // Each batch changes the leaf without its damage, by runs and by cell alike.
        let sound = damaged_leaf(&keys, |_| {});
        for changes in [
            [(9, Some(&b"new"[..]))],
            [(1, Some(b"new"))],
            [(3, Some(b"new"))],
        ] {
            let [by_runs, by_cell] = rewritten(&sound, &changes);
            assert_eq!(by_runs, by_cell);
            assert!(by_runs.unwrap().0.hash.is_some());
        }

        // Two kept keys exchanged: the leaf holds 1, 2, 4, 3, 5, 6, 7, 8.
        let exchanged = damaged_leaf(&keys, |payload| exchange_keys(payload, 2, 3));
        assert_eq!(
            rewritten(&exchanged, &[(9, Some(b"new"))]),
            refused("INVALID_BTREE_PAGE")
        );
        // An upsert of 3 stops on 4, which is not it, and adds a second 3 before it.
        assert_eq!(
            rewritten(&exchanged, &[(3, Some(b"new"))]),
            refused("INVALID_BTREE_PAGE")
        );
        // A kept cell with flags no release writes.
        let flagged = damaged_leaf(&keys, |payload| {
            let cell = cell_at(payload, 4);
            payload[cell + 2] = 2;
        });
        assert_eq!(
            rewritten(&flagged, &[(1, Some(b"new"))]),
            refused("UNSUPPORTED_BTREE_PAGE")
        );
        // A kept cell whose value would end past the page.
        let long = damaged_leaf(&keys, |payload| {
            let cell = cell_at(payload, 4);
            payload[cell + 4..cell + 8].copy_from_slice(&5_000_u32.to_le_bytes());
        });
        assert_eq!(
            rewritten(&long, &[(1, Some(b"new"))]),
            refused("INVALID_BTREE_PAGE")
        );
        // A kept cell whose slot points below the cells, into the slots.
        let astray = damaged_leaf(&keys, |payload| {
            let slot = NODE_HEADER_SIZE + 4 * SLOT_SIZE;
            payload[slot..slot + SLOT_SIZE].copy_from_slice(&50_u16.to_le_bytes());
        });
        assert_eq!(
            rewritten(&astray, &[(1, Some(b"new"))]),
            refused("INVALID_BTREE_PAGE")
        );
    }

    #[test]
    fn a_leaf_rewrite_refuses_more_than_a_cell_by_cell_rewrite() {
        let refused = |code: &str| Err(code.to_string());
        let keys = [1, 2, 3, 4, 5, 6, 7, 8];
        // Cells out of the order of their slots, with their keys in order. Cell by cell they are
        // copied in slot order, which packs them; a run would be copied as it lies.
        let unpacked = damaged_leaf(&keys, unpack);
        let [by_runs, by_cell] = rewritten(&unpacked, &[(1, Some(b"new"))]);
        assert_eq!(by_runs, refused("INVALID_BTREE_PAGE"));
        assert_eq!(by_cell.unwrap().1, keys);

        // The leaf holds 0, 1, 4, 2, and the batch removes the 4 that is out of order. Cell by
        // cell, a removed cell is compared with neither neighbour, and the leaf comes out sound.
        let exchanged = damaged_leaf(&[0, 1, 2, 4], |payload| exchange_keys(payload, 2, 3));
        let [by_runs, by_cell] = rewritten(&exchanged, &[(2, None), (4, None)]);
        assert_eq!(by_runs, refused("INVALID_BTREE_PAGE"));
        let (applied, held) = by_cell.unwrap();
        assert_eq!((applied.removed, held), (1, vec![0, 1, 2]));

        // By runs, a batch checks every cell of a leaf it reaches before it knows whether it
        // changes the leaf. Cell by cell, a batch that leaves the leaf as it was reads only the
        // keys it passes and the cells its changes stop on, and so reports no change: for cells
        // out of the order of their slots; for a key out of order whose own value an upsert
        // repeats; and for a cell with flags no release writes, which it does not stop on.
        let unchanged = |by_cell: std::result::Result<(BtreeBatch, Vec<u8>), String>| {
            assert_eq!(by_cell.unwrap().0.hash, None);
        };
        let [by_runs, by_cell] = rewritten(&unpacked, &[(9, None)]);
        assert_eq!(by_runs, refused("INVALID_BTREE_PAGE"));
        unchanged(by_cell);
        let [by_runs, by_cell] = rewritten(&exchanged, &[(4, Some(&[2; 4]))]);
        assert_eq!(by_runs, refused("INVALID_BTREE_PAGE"));
        unchanged(by_cell);
        let flagged = damaged_leaf(&keys, |payload| {
            let cell = cell_at(payload, 4);
            payload[cell + 2] = 2;
        });
        let [by_runs, by_cell] = rewritten(&flagged, &[(9, None)]);
        assert_eq!(by_runs, refused("UNSUPPORTED_BTREE_PAGE"));
        unchanged(by_cell);
    }

    #[test]
    fn backward_cursors_visit_keys_in_reverse_from_any_bound() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        // Even numbers, so that odd bounds fall between keys.
        let numbers = (0..120u32).map(|number| number * 2).collect::<Vec<_>>();
        let mut transaction = pager.begin_write().unwrap();
        for number in &numbers {
            root = Btree::upsert(&mut transaction, root, TREE, &key(*number), &value(*number))
                .unwrap()
                .root_page_id;
        }
        transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
        assert!(
            page_count_of(&mut pager, root) > 20,
            "the tree must have several levels"
        );

        let keys = |cursor: &mut BtreeCursor, pager: &mut Pager<MemoryPageDevice>| {
            let mut keys = Vec::new();
            while let Some((key, value)) = cursor.next(pager).unwrap() {
                assert_eq!(value[..4], key[..4]);
                keys.push(key);
            }
            keys
        };
        let forward = keys(
            &mut Btree::cursor(&mut pager, root, TREE).unwrap(),
            &mut pager,
        );
        assert_eq!(
            forward,
            numbers
                .iter()
                .map(|number| key(*number))
                .collect::<Vec<_>>()
        );
        for bound in [
            None,
            Some(Vec::new()),
            Some(vec![0]),
            Some(key(0)),
            Some(key(1)),
            Some(key(2)),
            Some(key(119)),
            Some(key(120)),
            Some(key(238)),
            Some(key(239)),
            Some(key(1_000)),
        ] {
            let expected = forward
                .iter()
                .filter(|key| bound.as_ref().is_none_or(|bound| *key < bound))
                .rev()
                .cloned()
                .collect::<Vec<_>>();
            let mut cursor =
                Btree::cursor_before(&mut pager, root, TREE, bound.as_deref()).unwrap();
            assert_eq!(
                keys(&mut cursor, &mut pager),
                expected,
                "{:?}",
                bound.map(|bound| bound.len())
            );
        }

        let mut transaction = pager.begin_write().unwrap();
        let mut cursor =
            Btree::cursor_before_in_transaction(&mut transaction, root, TREE, None).unwrap();
        let mut backward = Vec::new();
        while let Some((key, _)) = cursor.next_in_transaction(&mut transaction).unwrap() {
            backward.push(key);
        }
        backward.reverse();
        assert_eq!(backward, forward);
    }

    #[test]
    fn fingerprints_describe_the_entries_and_not_the_shape_of_the_tree() {
        // The same entries inserted in opposite orders produce differently shaped trees. A
        // fingerprint which depended on page boundaries would disagree between them, and a
        // synchronization protocol built on it would report a difference where there is none.
        let entries = (0..40u32)
            .map(|number| (key(number), value(number)))
            .collect::<Vec<_>>();

        let mut forwards = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut forwards_root = create_tree(&mut forwards);
        for (revision, (key, value)) in entries.iter().enumerate() {
            forwards_root = upsert_and_commit(
                &mut forwards,
                forwards_root,
                revision as u64 + 2,
                key,
                value,
            );
        }

        let mut backwards = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut backwards_root = create_tree(&mut backwards);
        for (revision, (key, value)) in entries.iter().rev().enumerate() {
            backwards_root = upsert_and_commit(
                &mut backwards,
                backwards_root,
                revision as u64 + 2,
                key,
                value,
            );
        }

        assert!(
            page_count_of(&mut forwards, forwards_root) > 1
                && page_count_of(&mut backwards, backwards_root) > 1,
            "the fixture must split so that the two trees can differ in shape"
        );
        assert_ne!(
            page_count_of(&mut forwards, forwards_root),
            page_count_of(&mut backwards, backwards_root),
            "insertion order must actually produce different trees"
        );

        let hash = verified_subtree_hash(&mut forwards, forwards_root);
        assert_eq!(hash, verified_subtree_hash(&mut backwards, backwards_root));
        assert_eq!(hash, expected_hash(&entries));
        assert_ne!(hash, EMPTY_HASH);
    }

    #[test]
    fn fingerprints_follow_replacement_removal_and_restoration() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        for (revision, number) in (0..24u32).enumerate() {
            root = upsert_and_commit(
                &mut pager,
                root,
                revision as u64 + 2,
                &key(number),
                &value(number),
            );
        }
        let original = verified_subtree_hash(&mut pager, root);

        root = upsert_and_commit(&mut pager, root, 30, &key(7), b"replacement");
        let replaced = verified_subtree_hash(&mut pager, root);
        assert_ne!(replaced, original, "a changed value must change the tree");

        root = upsert_and_commit(&mut pager, root, 31, &key(7), &value(7));
        assert_eq!(
            verified_subtree_hash(&mut pager, root),
            original,
            "restoring the original value must restore the fingerprint"
        );

        // Removing every entry must return the tree to the identity fingerprint, so that an empty
        // table and a never-written table compare equal.
        let mut current = Some(root);
        for (revision, number) in (0..24u32).enumerate() {
            let mut transaction = pager.begin_write().unwrap();
            let deleted = Btree::delete(
                &mut transaction,
                current.expect("the tree still holds entries"),
                TREE,
                &key(number),
            )
            .unwrap();
            assert!(deleted.removed);
            current = deleted.root_page_id;
            let hash = deleted.hash.expect("a removal reports a fingerprint");
            transaction
                .commit(revision as u64 + 32, hash, current)
                .unwrap();
            if let Some(page_id) = current {
                assert_eq!(verified_subtree_hash(&mut pager, page_id), hash);
            } else {
                assert_eq!(hash, EMPTY_HASH);
            }
        }
        assert_eq!(current, None);
    }

    #[test]
    fn a_removal_which_matches_nothing_reports_no_fingerprint() {
        // The caller keeps the fingerprint it already holds, so an absent key must not be able to
        // present itself as a new one.
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        root = upsert_and_commit(&mut pager, root, 2, &key(1), &value(1));
        let before = verified_subtree_hash(&mut pager, root);

        let mut transaction = pager.begin_write().unwrap();
        let deleted = Btree::delete(&mut transaction, root, TREE, &key(99)).unwrap();
        transaction.abort();
        assert!(!deleted.removed);
        assert_eq!(deleted.hash, None);
        assert_eq!(deleted.root_page_id, Some(root));
        assert_eq!(verified_subtree_hash(&mut pager, root), before);
    }

    #[test]
    fn entry_fingerprints_keep_the_key_and_value_apart() {
        // Moving bytes between an entry's key and its value makes a different entry, and so does
        // storing an overflow value's summary as an inline value of the same bytes.
        let split = cell_hash(b"ab", &CellValue::Inline(b"c"));
        assert_ne!(split, cell_hash(b"a", &CellValue::Inline(b"bc")));
        assert_ne!(split, cell_hash(b"abc", &CellValue::Inline(b"")));
        let descriptor = OverflowDescriptor {
            first_page_id: FIRST_DATA_PAGE_ID,
            generation: 1,
            total_length: 5_000,
            checksum: 0x1234_5678,
        };
        let mut summary = [0; 8];
        summary[..4].copy_from_slice(&5_000_u32.to_le_bytes());
        summary[4..].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        assert_ne!(
            cell_hash(b"key", &CellValue::Overflow(descriptor)),
            cell_hash(b"key", &CellValue::Inline(&summary))
        );
    }

    #[test]
    fn entry_fingerprints_are_the_values_stored_in_released_databases() {
        // The fingerprints that every released database records, for each child of an internal
        // node and for each tree, combine these numbers. So however an entry's fingerprint is
        // computed, each must stay the number it is here. The literals come from outside the
        // engine: a second XXH64, checked against the algorithm's published vectors. Each is also
        // taken the way a replaced entry takes it, from one hash of its key.
        let descriptor = OverflowDescriptor {
            first_page_id: FIRST_DATA_PAGE_ID,
            generation: 1,
            total_length: 5_000,
            checksum: 0x1234_5678,
        };
        let mut summary = [0; 8];
        summary[..4].copy_from_slice(&5_000_u32.to_le_bytes());
        summary[4..].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
        let seed = xxh64(b"key", 0);
        assert_eq!(seed, 0x4477_6256_2de1_4334);
        assert_eq!(key_hash(b"key"), seed);

        let inline = cell_hash(b"key", &CellValue::Inline(b"value"));
        assert_eq!(inline, 0x7b57_2dc2_312a_e10d);
        assert_eq!(inline, xxh64(b"value", seed));
        assert_eq!(inline, value_hash(seed, &CellValue::Inline(b"value")));

        let empty = cell_hash(b"key", &CellValue::Inline(b""));
        assert_eq!(empty, 0x2d5a_85ce_20c0_e388);
        assert_eq!(empty, xxh64(b"", seed));
        assert_eq!(empty, value_hash(seed, &CellValue::Inline(b"")));

        let overflow = cell_hash(b"key", &CellValue::Overflow(descriptor.clone()));
        assert_eq!(overflow, 0x27de_bccb_e91d_723e);
        assert_eq!(overflow, xxh64(&summary, !seed));
        assert_eq!(overflow, value_hash(seed, &CellValue::Overflow(descriptor)));

        // A key and a value long enough for the hash to take them in stripes.
        let long_key = (0..40).collect::<Vec<u8>>();
        let long_value = (0..100).map(|index| 255 - index).collect::<Vec<u8>>();
        let long = cell_hash(&long_key, &CellValue::Inline(&long_value));
        assert_eq!(long, 0x3563_e867_c1dc_c826);
        assert_eq!(long, xxh64(&long_value, xxh64(&long_key, 0)));
        assert_eq!(
            long,
            value_hash(key_hash(&long_key), &CellValue::Inline(&long_value))
        );
    }

    #[test]
    fn overflow_values_are_fingerprinted_from_their_descriptors() {
        // An overflow value is summarized by its length and checksum, so a fingerprint never has
        // to walk a chain of pages. The summary must still follow the content.
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        let large = vec![b'x'; MAX_BTREE_INLINE_VALUE_BYTES + 1];
        root = upsert_and_commit(&mut pager, root, 2, b"large", &large);
        let first = verified_subtree_hash(&mut pager, root);

        let mut different = large.clone();
        different[0] = b'y';
        root = upsert_and_commit(&mut pager, root, 3, b"large", &different);
        assert_ne!(verified_subtree_hash(&mut pager, root), first);

        root = upsert_and_commit(&mut pager, root, 4, b"large", &large);
        assert_eq!(verified_subtree_hash(&mut pager, root), first);

        // A value which spills is never fingerprinted as though it were stored inline, and the
        // choice between the two depends only on the key and value lengths.
        let inline = vec![b'x'; MAX_BTREE_INLINE_VALUE_BYTES];
        root = upsert_and_commit(&mut pager, root, 5, b"large", &inline);
        assert_ne!(verified_subtree_hash(&mut pager, root), first);
    }

    #[test]
    fn inserts_splits_reads_scans_seeks_updates_and_reopens() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);

        {
            let mut transaction = pager.begin_write().unwrap();
            for number in (0..80).rev() {
                root = Btree::upsert(&mut transaction, root, TREE, &key(number), &value(number))
                    .unwrap()
                    .root_page_id;
            }
            // Repeated right-edge changes must rewrite candidate-owned pages rather than leaking
            // a fresh COW path for every change in one transaction.
            for _ in 0..3 {
                root = Btree::upsert(&mut transaction, root, TREE, &key(79), &value(79))
                    .unwrap()
                    .root_page_id;
            }
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
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
            root = Btree::upsert(&mut transaction, root, TREE, &key(37), b"replacement")
                .unwrap()
                .root_page_id;
            transaction.commit(3, EMPTY_HASH, Some(root)).unwrap();
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
                    .unwrap()
                    .root_page_id;
            }
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
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
            let BtreeDelete {
                root_page_id: next,
                removed,
                ..
            } = Btree::delete(&mut transaction, root, TREE, &boundary_key).unwrap();
            assert!(removed);
            root = next.unwrap();
            transaction.commit(3, EMPTY_HASH, Some(root)).unwrap();
        }
        assert!(assert_exact_separators(&mut pager, root).is_some());

        let committed_root = root;
        {
            let mut transaction = pager.begin_write().unwrap();
            let BtreeDelete {
                root_page_id: unchanged,
                removed,
                ..
            } = Btree::delete(&mut transaction, root, TREE, &key(100)).unwrap();
            assert!(!removed);
            assert_eq!(unchanged, Some(root));
            for number in 0..70 {
                if number == boundary_number {
                    continue;
                }
                let BtreeDelete {
                    root_page_id: next,
                    removed,
                    ..
                } = Btree::delete(&mut transaction, root, TREE, &key(number)).unwrap();
                assert!(removed);
                root = next.unwrap();
            }
            transaction.commit(4, EMPTY_HASH, Some(root)).unwrap();
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
            root = Btree::upsert(&mut transaction, root, TREE, &key(65), &value(65))
                .unwrap()
                .root_page_id;
            transaction.commit(5, EMPTY_HASH, Some(root)).unwrap();
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
            let BtreeDelete {
                root_page_id: same_root,
                removed,
                ..
            } = Btree::delete(&mut transaction, root, TREE, b"absent").unwrap();
            assert!(!removed);
            assert_eq!(same_root, Some(root));
            let BtreeDelete {
                root_page_id: empty_root,
                removed,
                ..
            } = Btree::delete(&mut transaction, root, TREE, b"large").unwrap();
            assert!(removed);
            assert_eq!(empty_root, None);
            transaction.commit(3, EMPTY_HASH, None).unwrap();
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
                    .unwrap()
                    .root_page_id;
            }
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
        }
        let live_before = pager.active_metadata().superblock.live_data_page_count;
        let mut next_root = Some(root);
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in (0..80).rev() {
                let BtreeDelete {
                    root_page_id: next,
                    removed,
                    ..
                } = Btree::delete(&mut transaction, next_root.unwrap(), TREE, &key(number))
                    .unwrap();
                assert!(removed);
                next_root = next;
            }
            assert_eq!(next_root, None);
            transaction.commit(3, EMPTY_HASH, None).unwrap();
        }
        assert!(pager.active_metadata().superblock.live_data_page_count < live_before);

        let mut transaction = pager.begin_write().unwrap();
        let reused = Btree::create(&mut transaction, TREE).unwrap();
        let reused = Btree::upsert(&mut transaction, reused, TREE, b"again", b"works")
            .unwrap()
            .root_page_id;
        transaction.commit(4, EMPTY_HASH, Some(reused)).unwrap();
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
            root = Btree::upsert(&mut transaction, root, TREE, b"inline", &inline)
                .unwrap()
                .root_page_id;
            root = Btree::upsert(&mut transaction, root, TREE, b"overflow", &forced_overflow)
                .unwrap()
                .root_page_id;
            root = Btree::upsert(&mut transaction, root, TREE, b"maximum", &maximum)
                .unwrap()
                .root_page_id;
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
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
    fn entries_are_keys_followed_by_values() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        // Values a leaf holds inline, from none to the most it holds, and values in one overflow
        // page and in several, under keys from the empty one to the longest, among enough others
        // for the tree to have internal levels.
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (Vec::new(), b"under the empty key".to_vec()),
            (b"empty".to_vec(), Vec::new()),
            (b"short".to_vec(), b"value".to_vec()),
            (b"inline".to_vec(), vec![11; MAX_BTREE_INLINE_VALUE_BYTES]),
            (
                b"overflow".to_vec(),
                vec![12; MAX_BTREE_INLINE_VALUE_BYTES + 1],
            ),
            (
                b"pages".to_vec(),
                (0..3 * MAX_OVERFLOW_CHUNK_BYTES + 5)
                    .map(|index| (index % 251) as u8)
                    .collect(),
            ),
            (vec![b'z'; MAX_BTREE_KEY_BYTES], vec![13; 300]),
            (vec![b'y'; MAX_BTREE_KEY_BYTES], vec![14; 2_000]),
        ];
        entries.extend((0..40).map(|number| (key(number), value(number))));
        let entry_of = |prefix: &[u8], value: &[u8]| [prefix, value].concat();
        {
            let mut transaction = pager.begin_write().unwrap();
            for (key, value) in &entries {
                root = Btree::upsert(&mut transaction, root, TREE, key, value)
                    .unwrap()
                    .root_page_id;
            }
            // A candidate's pages are read as committed ones are.
            for (key, value) in &entries {
                assert_eq!(
                    get_prefixed_from(&mut transaction, root, TREE, key, key).unwrap(),
                    Some(entry_of(key, value))
                );
            }
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
        }
        let root_node = Node::decode(
            pager.read_page(root).unwrap(),
            TREE,
            pager.generation(),
            false,
        )
        .unwrap();
        assert!(root_node.level >= 1, "the keys must fill several leaves");

        for (key, value) in &entries {
            assert_eq!(
                Btree::get(&mut pager, root, TREE, key).unwrap().as_ref(),
                Some(value)
            );
            let entry = Btree::get_entry(&mut pager, root, TREE, key)
                .unwrap()
                .unwrap();
            assert_eq!(entry, entry_of(key, value));
            // One allocation of exactly the entry's length, which boxing it does not copy again.
            assert_eq!(entry.capacity(), entry.len());
            // The prefix need not be the key, and without one the value comes back alone.
            for prefix in [b"p".as_slice(), b"another prefix", b""] {
                let prefixed = get_prefixed_from(&mut pager, root, TREE, key, prefix)
                    .unwrap()
                    .unwrap();
                assert_eq!(prefixed, entry_of(prefix, value));
                assert_eq!(prefixed.capacity(), prefixed.len());
            }
        }
        // A key the tree does not hold has no entry, whatever would be put in front of it.
        for absent in [
            b"absent".to_vec(),
            b"shor".to_vec(),
            b"shorter".to_vec(),
            key(40),
            vec![b'z'; MAX_BTREE_KEY_BYTES - 1],
        ] {
            assert_eq!(Btree::get(&mut pager, root, TREE, &absent).unwrap(), None);
            assert_eq!(
                Btree::get_entry(&mut pager, root, TREE, &absent).unwrap(),
                None
            );
        }
        // The lookup's refusals do not depend on the prefix either.
        let too_long = vec![0; MAX_BTREE_KEY_BYTES + 1];
        assert_eq!(
            Btree::get_entry(&mut pager, root, TREE, &too_long)
                .unwrap_err()
                .message,
            Btree::get(&mut pager, root, TREE, &too_long)
                .unwrap_err()
                .message
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
            root = Btree::upsert(&mut transaction, root, TREE, &key, &inline)
                .unwrap()
                .root_page_id;
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
            root = Btree::upsert(&mut transaction, root, TREE, &key, &spilled)
                .unwrap()
                .root_page_id;
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
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
        }
        assert_eq!(
            Btree::get(&mut pager, root, TREE, &key).unwrap(),
            Some(spilled)
        );
    }

    #[test]
    fn a_batch_stores_values_inline_or_in_overflow_pages_as_single_changes_do() {
        /// How a cell stores its value: `None` inline, or the length and checksum that its
        /// overflow descriptor records.
        type Form = Option<(u32, u32)>;

        /// Every entry beneath a page, in key order, with how its cell stores its value.
        fn stored(pager: &mut Pager<MemoryPageDevice>, page_id: PageId) -> Vec<(Vec<u8>, Form)> {
            let node = Node::decode(
                pager.read_page(page_id).unwrap(),
                TREE,
                pager.generation(),
                false,
            )
            .unwrap();
            match node.kind {
                NodeKind::Leaf(entries) => entries
                    .into_iter()
                    .map(|entry| {
                        let overflow = match entry.value {
                            LeafValue::Inline(_) => None,
                            LeafValue::Overflow(descriptor) => {
                                Some((descriptor.total_length, descriptor.checksum))
                            }
                        };
                        (entry.key, overflow)
                    })
                    .collect(),
                NodeKind::Internal(internal) => {
                    let mut cells = stored(pager, internal.leftmost_child);
                    for entry in internal.entries {
                        cells.extend(stored(pager, entry.right_child));
                    }
                    cells
                }
            }
        }

        // A value alone may take 1,024 bytes of a cell, and a key and value together 1,536: on
        // each side of both limits, in key order.
        let long_key = |byte: u8| vec![byte; MAX_BTREE_KEY_BYTES];
        let entries = [
            (b"k1".to_vec(), vec![1; MAX_BTREE_INLINE_VALUE_BYTES]),
            (b"k2".to_vec(), vec![2; MAX_BTREE_INLINE_VALUE_BYTES + 1]),
            (
                long_key(b'l'),
                vec![3; MAX_BTREE_INLINE_ENTRY_BYTES - MAX_BTREE_KEY_BYTES],
            ),
            (
                long_key(b'm'),
                vec![4; MAX_BTREE_INLINE_ENTRY_BYTES - MAX_BTREE_KEY_BYTES + 1],
            ),
        ];
        fn batch_of(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<BatchChange<'_>> {
            entries
                .iter()
                .map(|(key, value)| BatchChange {
                    key,
                    value: Some(value),
                })
                .collect()
        }
        // The batch writer makes a cell in three places: for a tree with no root, for a key its
        // leaf holds, and for a key its leaf lacks. `held` is what the leaf holds beforehand.
        let replaced = entries
            .iter()
            .map(|(key, _)| (key.clone(), b"held".to_vec()))
            .collect::<Vec<_>>();
        let beside = vec![(b"a".to_vec(), b"beside".to_vec())];
        for (case, held) in [None, Some(replaced), Some(beside)].into_iter().enumerate() {
            let mut batched = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
            let mut batched_root = None;
            let mut single = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
            let mut single_root = create_tree(&mut single);
            if let Some(held) = &held {
                let mut transaction = batched.begin_write().unwrap();
                batched_root = Btree::apply(&mut transaction, None, TREE, &batch_of(held))
                    .unwrap()
                    .root_page_id;
                transaction.commit(2, EMPTY_HASH, batched_root).unwrap();
                let mut transaction = single.begin_write().unwrap();
                for (key, value) in held {
                    single_root = Btree::upsert(&mut transaction, single_root, TREE, key, value)
                        .unwrap()
                        .root_page_id;
                }
                transaction
                    .commit(2, EMPTY_HASH, Some(single_root))
                    .unwrap();
            }

            let mut transaction = batched.begin_write().unwrap();
            let applied =
                Btree::apply(&mut transaction, batched_root, TREE, &batch_of(&entries)).unwrap();
            let batched_root = applied.root_page_id.expect("the tree holds entries");
            transaction
                .commit(3, EMPTY_HASH, Some(batched_root))
                .unwrap();
            assert_eq!(
                applied.inserted,
                if case == 1 { 0 } else { entries.len() },
                "case {case}"
            );

            let mut transaction = single.begin_write().unwrap();
            let mut single_hash = EMPTY_HASH;
            for (key, value) in &entries {
                let upserted =
                    Btree::upsert(&mut transaction, single_root, TREE, key, value).unwrap();
                single_root = upserted.root_page_id;
                single_hash = upserted.hash;
            }
            transaction
                .commit(3, EMPTY_HASH, Some(single_root))
                .unwrap();

            let cells = stored(&mut batched, batched_root);
            let forms = entries
                .iter()
                .map(|(key, _)| {
                    let (_, overflow) = cells
                        .iter()
                        .find(|(stored_key, _)| stored_key == key)
                        .expect("the tree holds every entry of the batch");
                    overflow.is_some()
                })
                .collect::<Vec<_>>();
            assert_eq!(forms, [false, true, false, true], "case {case}");
            assert_eq!(cells, stored(&mut single, single_root), "case {case}");
            assert_eq!(applied.hash, Some(single_hash), "case {case}");
            assert_eq!(
                verified_subtree_hash(&mut batched, batched_root),
                single_hash,
                "case {case}"
            );
            for (key, value) in &entries {
                assert_eq!(
                    Btree::get(&mut batched, batched_root, TREE, key).unwrap(),
                    Some(value.clone()),
                    "case {case}"
                );
            }
        }
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
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", &large_a)
                .unwrap()
                .root_page_id;
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", b"tiny")
                .unwrap()
                .root_page_id;
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", &large_b)
                .unwrap()
                .root_page_id;
            root = Btree::upsert(&mut transaction, root, TREE, b"same-tx", &large_c)
                .unwrap()
                .root_page_id;
            transaction.commit(6, EMPTY_HASH, Some(root)).unwrap();
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
        let next_root = Btree::upsert(&mut transaction, root, TREE, b"a", b"b")
            .unwrap()
            .root_page_id;
        transaction.commit(2, EMPTY_HASH, Some(next_root)).unwrap();
        assert_eq!(
            cursor.next(&mut pager).unwrap_err().code,
            "CURSOR_INVALIDATED"
        );
    }

    #[test]
    fn candidate_cursor_streams_shared_and_candidate_trees_during_one_write() {
        const TARGET_TREE: TreeId = TREE + 1;

        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut source_root = create_tree(&mut pager);
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in 0..20 {
                source_root = Btree::upsert(
                    &mut transaction,
                    source_root,
                    TREE,
                    &key(number),
                    &value(number),
                )
                .unwrap()
                .root_page_id;
            }
            transaction
                .commit(2, EMPTY_HASH, Some(source_root))
                .unwrap();
        }

        let mut transaction = pager.begin_write().unwrap();
        let mut source = Btree::cursor_in_transaction(&mut transaction, source_root, TREE).unwrap();
        let mut target_root = Btree::create(&mut transaction, TARGET_TREE).unwrap();
        let mut copied = 0_u32;
        while let Some((source_key, source_value)) =
            source.next_in_transaction(&mut transaction).unwrap()
        {
            target_root = Btree::upsert(
                &mut transaction,
                target_root,
                TARGET_TREE,
                &source_key,
                &source_value,
            )
            .unwrap()
            .root_page_id;
            copied += 1;
        }
        assert_eq!(copied, 20);

        let mut candidate =
            Btree::cursor_from_in_transaction(&mut transaction, target_root, TARGET_TREE, &key(7))
                .unwrap();
        assert_eq!(
            candidate.next_in_transaction(&mut transaction).unwrap(),
            Some((key(7), value(7)))
        );
        assert_eq!(
            candidate.next_in_transaction(&mut transaction).unwrap(),
            Some((key(8), value(8)))
        );
        transaction
            .commit(3, EMPTY_HASH, Some(target_root))
            .unwrap();

        let mut cursor = Btree::cursor(&mut pager, target_root, TARGET_TREE).unwrap();
        let mut copied = 0;
        while cursor.next(&mut pager).unwrap().is_some() {
            copied += 1;
        }
        assert_eq!(copied, 20);
    }

    #[test]
    fn candidate_cursor_rejects_the_wrong_transaction_and_wrong_generation() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let root = create_tree(&mut pager);
        let mut transaction = pager.begin_write().unwrap();
        let mut candidate = Btree::cursor_in_transaction(&mut transaction, root, TREE).unwrap();
        assert_eq!(
            candidate
                .next(&mut Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap())
                .unwrap_err()
                .code,
            "CURSOR_INVALIDATED"
        );
        transaction.abort();

        let mut other = pager.begin_write().unwrap();
        assert_eq!(
            candidate.next_in_transaction(&mut other).unwrap_err().code,
            "CURSOR_INVALIDATED"
        );
        other.abort();
    }

    #[test]
    fn candidate_cursor_is_invalidated_only_when_its_tree_changes() {
        const OTHER_TREE: TreeId = TREE + 1;

        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let root = create_tree(&mut pager);
        let mut transaction = pager.begin_write().unwrap();
        let mut cursor = Btree::cursor_in_transaction(&mut transaction, root, TREE).unwrap();
        let other_root = Btree::create(&mut transaction, OTHER_TREE).unwrap();
        let _other_root = Btree::upsert(
            &mut transaction,
            other_root,
            OTHER_TREE,
            b"unrelated",
            b"value",
        )
        .unwrap()
        .root_page_id;
        assert_eq!(cursor.next_in_transaction(&mut transaction).unwrap(), None);

        let mut cursor = Btree::cursor_in_transaction(&mut transaction, root, TREE).unwrap();
        let _next_root = Btree::upsert(&mut transaction, root, TREE, b"changed", b"value")
            .unwrap()
            .root_page_id;
        assert_eq!(
            cursor
                .next_in_transaction(&mut transaction)
                .unwrap_err()
                .code,
            "CURSOR_INVALIDATED"
        );
        transaction.abort();
    }

    #[test]
    fn reclaim_frees_internal_leaf_and_overflow_pages_and_abort_preserves_them() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        let overflow = vec![9; MAX_OVERFLOW_CHUNK_BYTES * 2 + 31];
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in 0..30 {
                let inline = value(number);
                let stored_value = if number == 14 {
                    overflow.as_slice()
                } else {
                    inline.as_slice()
                };
                root = Btree::upsert(&mut transaction, root, TREE, &key(number), stored_value)
                    .unwrap()
                    .root_page_id;
            }
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
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
                > 0
        );
        let live_before = pager.active_metadata().superblock.live_data_page_count;

        let mut aborted = pager.begin_write().unwrap();
        Btree::reclaim(&mut aborted, root, TREE).unwrap();
        aborted.abort();
        assert_eq!(
            Btree::get(&mut pager, root, TREE, &key(14)).unwrap(),
            Some(overflow.clone())
        );
        assert_eq!(
            pager.active_metadata().superblock.live_data_page_count,
            live_before
        );

        let mut transaction = pager.begin_write().unwrap();
        Btree::reclaim(&mut transaction, root, TREE).unwrap();
        transaction.commit(3, EMPTY_HASH, None).unwrap();
        assert_eq!(pager.active_metadata().superblock.live_data_page_count, 0);
        assert_eq!(
            pager.read_page(root).unwrap_err().code,
            "PAGE_NOT_ALLOCATED"
        );

        let device = pager.into_device();
        let mut reopened = Pager::open_or_create(device).unwrap();
        assert_eq!(
            reopened.active_metadata().superblock.live_data_page_count,
            0
        );
        let mut transaction = reopened.begin_write().unwrap();
        let reused = Btree::create(&mut transaction, TREE).unwrap();
        assert!(reused >= FIRST_DATA_PAGE_ID);
        transaction.abort();
    }

    #[test]
    fn reclaim_releases_a_wholly_candidate_tree_without_publication() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let live_before = pager.active_metadata().superblock.live_data_page_count;
        let mut transaction = pager.begin_write().unwrap();
        let mut root = Btree::create(&mut transaction, TREE).unwrap();
        root = Btree::upsert(
            &mut transaction,
            root,
            TREE,
            b"overflow",
            &vec![1; MAX_OVERFLOW_CHUNK_BYTES + 1],
        )
        .unwrap()
        .root_page_id;
        Btree::reclaim(&mut transaction, root, TREE).unwrap();
        transaction.commit(1, EMPTY_HASH, None).unwrap();
        assert_eq!(
            pager.active_metadata().superblock.live_data_page_count,
            live_before
        );
    }

    #[test]
    fn reclaim_corruption_fails_before_freeing_any_page_and_poison_requires_abort() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        root = upsert_and_commit(
            &mut pager,
            root,
            2,
            b"overflow",
            &vec![3; MAX_OVERFLOW_CHUNK_BYTES + 19],
        );
        let descriptor = overflow_descriptor_from_root(&mut pager, root, b"overflow");
        let active = pager.active_metadata().clone();
        let mut device = into_device_without_predecessor(pager);
        let mut overflow = Page::decode(device.page(descriptor.first_page_id).unwrap()).unwrap();
        overflow.payload[OVERFLOW_HEADER_SIZE] ^= 1;
        device
            .write_page(descriptor.first_page_id, &overflow.encode().unwrap())
            .unwrap();
        let mut pager = Pager::open_or_create(device).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        assert_eq!(
            Btree::reclaim(&mut transaction, root, TREE)
                .unwrap_err()
                .code,
            "INVALID_OVERFLOW_PAGE"
        );
        assert_eq!(
            transaction.generation().unwrap_err().code,
            "TRANSACTION_FAILED"
        );
        transaction.abort();
        assert_eq!(
            pager.active_metadata().allocation_bitmap,
            active.allocation_bitmap
        );
        assert_eq!(
            pager.active_metadata().superblock.live_data_page_count,
            active.superblock.live_data_page_count
        );
        assert_eq!(pager.read_page(root).unwrap().id, root);
    }

    #[test]
    fn failed_cow_path_cannot_publish_a_bitmap_with_a_live_child_freed() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut root = create_tree(&mut pager);
        {
            let mut transaction = pager.begin_write().unwrap();
            for number in 0..10 {
                root = Btree::upsert(&mut transaction, root, TREE, &key(number), &value(number))
                    .unwrap()
                    .root_page_id;
            }
            transaction.commit(2, EMPTY_HASH, Some(root)).unwrap();
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

        let mut active = pager.active_metadata().clone();
        let mut memory = into_device_without_predecessor(pager);
        for id in FIRST_DATA_PAGE_ID..MAX_PAGE_COUNT {
            active.allocation_bitmap.set_allocated(id, true).unwrap();
        }
        active
            .allocation_bitmap
            .set_allocated(MAX_PAGE_COUNT - 1, false)
            .unwrap();
        active.superblock.live_data_page_count = (MAX_PAGE_COUNT - FIRST_DATA_PAGE_ID - 1) as u32;
        for (id, page) in active.encode_pages().unwrap() {
            memory.write_page(id, &page).unwrap();
        }

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
            transaction
                .commit(3, EMPTY_HASH, Some(root))
                .unwrap_err()
                .code,
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
        let long_key = vec![0; MAX_BTREE_KEY_BYTES + 1];
        let error = Btree::apply(
            &mut transaction,
            Some(root),
            TREE,
            &[BatchChange {
                key: &long_key,
                value: Some(b"v"),
            }],
        )
        .unwrap_err();
        assert_eq!(error.code, "BTREE_LIMIT");
        assert_eq!(error.message, "B-tree key is 1025 bytes, exceeding 1024");
        transaction.abort();
        assert_eq!(pager.physical_page_count(), page_count);
        // A lookup refuses the key as a write does, and the longest key allowed is merely absent.
        let error = Btree::get(&mut pager, root, TREE, &long_key).unwrap_err();
        assert_eq!(error.code, "BTREE_LIMIT");
        assert_eq!(error.message, "B-tree key is 1025 bytes, exceeding 1024");
        assert_eq!(
            Btree::get(&mut pager, root, TREE, &long_key[1..]).unwrap(),
            None
        );
    }

    #[test]
    fn a_batch_out_of_order_is_refused_before_anything_is_written() {
        type Change<'a> = (&'a [u8], Option<&'a [u8]>);
        fn batch_of<'a>(changes: &[Change<'a>]) -> Vec<BatchChange<'a>> {
            changes
                .iter()
                .map(|&(key, value)| BatchChange { key, value })
                .collect()
        }
        // Every batch begins with a value long enough for overflow pages, which a batch
        // allocates as it reaches the value: a refusal that came after the batch had begun would
        // leave those pages allocated.
        let long = vec![b'o'; MAX_OVERFLOW_CHUNK_BYTES * 3];
        let held: [Change<'_>; 2] = [(b"c", Some(b"held")), (b"e", Some(b"held"))];
        let ordered: [Change<'_>; 4] = [
            (b"a", Some(&long)),
            (b"c", None),
            (b"d", Some(b"new")),
            (b"f", Some(b"")),
        ];
        // A key removed and then added, which is what sorting would leave of a batch that held
        // one key twice; two keys exchanged; and the last key twice, behind changes in order.
        let unordered: [[Change<'_>; 4]; 3] = [
            [
                (b"a", Some(&long)),
                (b"c", None),
                (b"c", Some(b"")),
                (b"f", Some(b"")),
            ],
            [
                (b"a", Some(&long)),
                (b"d", Some(b"new")),
                (b"c", None),
                (b"f", Some(b"")),
            ],
            [
                (b"a", Some(&long)),
                (b"c", None),
                (b"f", Some(b"")),
                (b"f", Some(b"")),
            ],
        ];
        for with_root in [false, true] {
            // Two pagers hold the same tree, or none. One is offered every unordered batch before
            // the ordered one and must end as the other does, which was offered only that.
            let open = || {
                let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
                let mut root = None;
                if with_root {
                    let mut transaction = pager.begin_write().unwrap();
                    root = Btree::apply(&mut transaction, None, TREE, &batch_of(&held))
                        .unwrap()
                        .root_page_id;
                    transaction.commit(1, EMPTY_HASH, root).unwrap();
                }
                (pager, root)
            };
            let (mut refusing, root) = open();
            let (mut plain, plain_root) = open();
            assert_eq!(root.is_some(), with_root);
            assert_eq!(root, plain_root);

            // A candidate takes its first page at or below the end of the file, so the pages up
            // to there show whether a refused batch allocated any.
            let end = refusing.physical_page_count();
            let mut transaction = refusing.begin_write().unwrap();
            for (case, batch) in unordered.iter().enumerate() {
                let context = format!("root {with_root}, batch {case}");
                let error =
                    Btree::apply(&mut transaction, root, TREE, &batch_of(batch)).unwrap_err();
                assert_eq!(error.code, "INVALID_PAGED_ARGUMENT", "{context}");
                assert_eq!(
                    error.message, "A B-tree batch must be sorted by strictly increasing key",
                    "{context}"
                );
                assert!((0..=end).all(|id| !transaction.owns_page(id)), "{context}");
                // The batch is refused as an argument, so the transaction is not marked failed.
                transaction.generation().expect(&context);
            }
            let applied = Btree::apply(&mut transaction, root, TREE, &batch_of(&ordered)).unwrap();
            transaction
                .commit(2, EMPTY_HASH, applied.root_page_id)
                .unwrap();

            let mut transaction = plain.begin_write().unwrap();
            let expected =
                Btree::apply(&mut transaction, plain_root, TREE, &batch_of(&ordered)).unwrap();
            transaction
                .commit(2, EMPTY_HASH, expected.root_page_id)
                .unwrap();

            assert_eq!(applied, expected, "root {with_root}");
            assert_eq!(applied.inserted, 3, "root {with_root}");
            assert_eq!(applied.removed, usize::from(with_root), "root {with_root}");
            let root = applied.root_page_id.expect("the tree holds entries");
            assert_eq!(
                Btree::get(&mut refusing, root, TREE, b"a").unwrap(),
                Some(long.clone())
            );
            let (refusing, plain) = (refusing.into_device(), plain.into_device());
            assert_eq!(
                refusing.page_count(),
                plain.page_count(),
                "root {with_root}"
            );
            for id in 0..plain.page_count() {
                assert!(
                    refusing.page(id).unwrap() == plain.page(id).unwrap(),
                    "root {with_root}, page {id}"
                );
            }
        }
    }

    #[test]
    fn put_stores_little_endian_bytes() {
        // Each store against the bytes `to_le_bytes` gives, at the start of a buffer, at an odd
        // offset and at the last offset that holds the number, leaving every other byte alone.
        for offset in [0, 5, 14] {
            let mut bytes = [0xaa; 16];
            put_u16(&mut bytes, offset, 0x1234);
            let mut expected = [0xaa; 16];
            expected[offset..offset + 2].copy_from_slice(&0x1234_u16.to_le_bytes());
            assert_eq!(bytes, expected, "two bytes at {offset}");
            assert_eq!(read_u16(&bytes, offset), 0x1234);
        }
        for offset in [0, 5, 12] {
            let mut bytes = [0xaa; 16];
            put_u32(&mut bytes, offset, 0x1234_5678);
            let mut expected = [0xaa; 16];
            expected[offset..offset + 4].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
            assert_eq!(bytes, expected, "four bytes at {offset}");
            assert_eq!(read_u32(&bytes, offset), 0x1234_5678);
        }
    }

    /// A sequence of numbers that is the same in every run, for tests that generate keys.
    fn numbers(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
    }

    /// Keys of up to twenty bytes over five values, so that many share their first eight bytes,
    /// differ within them at every place, or are prefixes of one another.
    fn generated_keys(count: usize, seed: u64) -> Vec<Vec<u8>> {
        let mut next = numbers(seed);
        let mut keys = vec![Vec::new()];
        for _ in 0..count {
            let length = (next() % 21) as usize;
            keys.push(
                (0..length)
                    .map(|_| [0x00, 0x01, 0x7f, 0x80, 0xff][(next() % 5) as usize])
                    .collect(),
            );
        }
        keys
    }

    #[test]
    fn key_order_orders_keys_as_slices_do() {
        let keys = generated_keys(500, 0x9e37_79b9_7f4a_7c15);
        for left in &keys {
            for right in &keys {
                assert_eq!(
                    key_order(left, right),
                    left.as_slice().cmp(right),
                    "{left:?} {right:?}"
                );
            }
        }
        // Keys of exactly eight bytes, as an integer's are, in each order and equal.
        for (left, right) in [(1_u64, 2_u64), (2, 1), (7, 7), (255, 256), (u64::MAX, 0)] {
            let (left, right) = (left.to_be_bytes(), right.to_be_bytes());
            assert_eq!(key_order(&left, &right), left.cmp(&right));
        }
    }

    /// A leaf of `keys` and an internal node whose separators they are, as views of their pages,
    /// after `spoil` has changed what it will of each payload.
    fn searched_nodes(
        keys: &[Vec<u8>],
        spoil: impl Fn(&mut Vec<u8>, bool),
    ) -> [NodeView<'static>; 2] {
        let leaf = Node::leaf(
            TREE,
            2,
            keys.iter()
                .map(|key| LeafEntry {
                    key: key.clone(),
                    value: LeafValue::Inline(b"v".to_vec()),
                })
                .collect(),
        );
        let internal = Node::internal(
            TREE,
            2,
            1,
            FIRST_DATA_PAGE_ID + 1,
            EMPTY_HASH,
            keys.iter()
                .enumerate()
                .map(|(index, key)| InternalEntry {
                    key: key.clone(),
                    right_child: FIRST_DATA_PAGE_ID + 2 + index as PageId,
                    child_hash: EMPTY_HASH,
                })
                .collect(),
        );
        [(leaf, true), (internal, false)].map(|(node, is_leaf)| {
            let (mut page, _) = node.encode(FIRST_DATA_PAGE_ID).unwrap();
            spoil(&mut page.payload, is_leaf);
            NodeView::from_payload(
                page.id,
                page.page_type,
                Cow::Owned(page.payload),
                TREE,
                2,
                false,
            )
            .unwrap()
        })
    }

    /// Holds a view's one-loop search to the readers it stands in for, for `key`: the same place
    /// and the same finding, or no answer exactly where they refuse a cell, with their error.
    fn assert_searched_as_read(view: &NodeView<'_>, key: &[u8]) {
        let read = if view.leaf {
            view.lower_bound(key).and_then(|index| {
                let found = index < view.len() && view.leaf_key(index)? == key;
                Ok((index, found))
            })
        } else {
            view.child_index_for(key).map(|index| (index, false))
        };
        match (view.search(key), read) {
            (Some((index, found)), Ok(read)) => {
                assert_eq!((index, view.leaf && found), read, "{key:?}");
                if view.leaf {
                    assert_eq!(view.find(key).unwrap(), found.then_some(index), "{key:?}");
                }
            }
            (None, Err(error)) => assert_eq!(view.search_error(key), error, "{key:?}"),
            (searched, read) => panic!("{key:?}: searched {searched:?}, read {read:?}"),
        }
    }

    #[test]
    fn a_node_is_searched_as_its_readers_search_it() {
        let probes = generated_keys(400, 0x2545_f491_4f6c_dd1d);
        for (count, seed) in [(0, 1), (1, 2), (2, 3), (7, 4), (60, 5), (90, 6)] {
            let mut keys = generated_keys(count, seed);
            if count == 0 {
                keys.clear();
            }
            keys.sort();
            keys.dedup();
            for view in &searched_nodes(&keys, |_, _| {}) {
                for key in keys.iter().chain(&probes) {
                    assert_searched_as_read(view, key);
                }
            }
            // A page whose checksum holds may still have its cells out of order. The search and
            // the readers probe the same cells of it, and so end at the same place.
            if keys.len() > 3 {
                let exchange = |payload: &mut Vec<u8>, _: bool| {
                    let (first, last) = (NODE_HEADER_SIZE, NODE_HEADER_SIZE + 3 * SLOT_SIZE);
                    for byte in 0..SLOT_SIZE {
                        payload.swap(first + byte, last + byte);
                    }
                };
                for view in &searched_nodes(&keys, exchange) {
                    for key in keys.iter().chain(&probes) {
                        assert_searched_as_read(view, key);
                    }
                }
            }
        }
    }

    #[test]
    fn a_node_search_refuses_the_cell_its_readers_refuse() {
        let probes = generated_keys(200, 0x1234_5678_9abc_def1);
        let mut keys = generated_keys(40, 7);
        keys.sort();
        keys.dedup();
        let count = keys.len();
        // A slot that points below the cells, into the free space, whose zero bytes would read
        // as a cell with an empty key, or a byte short of the first cell; one that points at
        // the page's last byte, or past its end; and a cell whose key is longer than the page
        // has room for.
        type Spoil = fn(&mut Vec<u8>, usize, bool);
        let spoils: [Spoil; 5] = [
            |payload, slot, _| {
                let free_end = read_u16(payload, 28);
                payload[slot..slot + SLOT_SIZE].copy_from_slice(&(free_end - 64).to_le_bytes());
            },
            |payload, slot, _| {
                let free_end = read_u16(payload, 28);
                payload[slot..slot + SLOT_SIZE].copy_from_slice(&(free_end - 1).to_le_bytes());
            },
            |payload, slot, _| {
                let last = (MAX_PAGE_PAYLOAD_SIZE - 1) as u16;
                payload[slot..slot + SLOT_SIZE].copy_from_slice(&last.to_le_bytes());
            },
            |payload, slot, _| {
                payload[slot..slot + SLOT_SIZE].copy_from_slice(&u16::MAX.to_le_bytes());
            },
            |payload, slot, _| {
                let cell = usize::from(read_u16(payload, slot));
                payload[cell..cell + 2].copy_from_slice(&u16::MAX.to_le_bytes());
            },
        ];
        let mut refused = 0;
        for spoil in spoils {
            for index in [0, 1, count / 2, count - 2, count - 1] {
                let slot = NODE_HEADER_SIZE + index * SLOT_SIZE;
                let views = searched_nodes(&keys, |payload, is_leaf| spoil(payload, slot, is_leaf));
                for view in &views {
                    for key in keys.iter().chain(&probes) {
                        refused += usize::from(view.search(key).is_none());
                        assert_searched_as_read(view, key);
                    }
                }
            }
        }
        // The cells were spoiled where searches pass, so many searches met one.
        assert!(refused > 1_000, "{refused}");
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
        let (page, _) = Node::leaf(TREE, 2, entries.clone())
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
    fn page_envelope_checksum_catches_payload_corruption() {
        let page = Node::leaf(
            TREE,
            2,
            vec![LeafEntry {
                key: b"a".to_vec(),
                value: LeafValue::Inline(b"b".to_vec()),
            }],
        )
        .encode(FIRST_DATA_PAGE_ID)
        .unwrap()
        .0;
        let mut bytes = page.encode().unwrap();
        bytes[100] ^= 1;
        assert_eq!(Page::decode(&bytes).unwrap_err().code, "INVALID_PAGE");
    }

    #[test]
    fn overflow_codec_rejects_cycle_truncation_position_tree_generation_and_checksum() {
        let value = vec![9; MAX_OVERFLOW_CHUNK_BYTES + 13];
        let checksum = checksum32(&value);
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
                &mut |id| Ok(pages.get(&id).unwrap().clone()),
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
                &mut |id| Ok(pages.get(&id).unwrap().clone()),
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
            (into_device_without_predecessor(pager), root, descriptor)
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
        let mut device = into_device_without_predecessor(pager);
        let corrupt = Node::internal(
            TREE,
            generation,
            1,
            root,
            EMPTY_HASH,
            vec![InternalEntry {
                key: b"m".to_vec(),
                right_child: root + 1,
                child_hash: EMPTY_HASH,
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
                EMPTY_HASH,
                vec![InternalEntry {
                    key: b"m".to_vec(),
                    right_child: right,
                    child_hash: EMPTY_HASH,
                }],
            )
            .encode(duplicate_root)
            .unwrap()
            .0;
            let cell_offset = read_u16(&page.payload, NODE_HEADER_SIZE) as usize;
            page.payload[cell_offset + 4..cell_offset + 12].copy_from_slice(&left.to_le_bytes());
            transaction.write_new_page(&page).unwrap();
            transaction
                .commit(1, EMPTY_HASH, Some(duplicate_root))
                .unwrap();
        }
        assert_eq!(
            Btree::validating_cursor(&mut pager, duplicate_root, TREE)
                .unwrap_err()
                .code,
            "INVALID_BTREE_PAGE"
        );
        // A reading cursor checks structure as it reaches it, and refuses to revisit the leaf.
        let mut cursor = Btree::cursor(&mut pager, duplicate_root, TREE).unwrap();
        assert_eq!(
            cursor.next(&mut pager).unwrap_err().code,
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
                    EMPTY_HASH,
                    vec![InternalEntry {
                        key: b"m".to_vec(),
                        right_child: right,
                        child_hash: EMPTY_HASH,
                    }],
                ),
            )
            .unwrap();
            transaction
                .commit(1, EMPTY_HASH, Some(wrong_level_root))
                .unwrap();
        }
        assert_eq!(
            Btree::get(&mut pager, wrong_level_root, TREE, b"a")
                .unwrap_err()
                .code,
            "INVALID_BTREE_PAGE"
        );
    }
}
