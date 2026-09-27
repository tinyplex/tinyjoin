//! Keeps recently freed small blocks for reuse, in front of the system allocator.
//!
//! Every statement allocates and frees dozens of short-lived vectors, strings, and map nodes, and
//! dlmalloc spends more on each than the engine spends using it. A freed block of up to
//! [`MAX_KEPT_SIZE`] bytes goes onto a list of blocks of its size, rounded up to a [`GRANULE`],
//! and the next allocation of that size takes it back without calling dlmalloc. A list keeps
//! [`KEPT_BYTES`], or four blocks of its size, so the memory the lists hold stays small.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::ptr;

const GRANULE: usize = 16;
const SIZES: usize = 128;
const MAX_KEPT_SIZE: usize = GRANULE * SIZES;
/// The alignment of every kept block: dlmalloc's own, which serves any smaller one.
const ALIGN: usize = 8;
const KEPT_BYTES: usize = 4096;

#[derive(Clone, Copy)]
struct Kept {
    /// The first free block, whose first bytes point at the next.
    head: *mut u8,
    count: usize,
}

pub(crate) struct Recycling {
    kept: UnsafeCell<[Kept; SIZES]>,
}

// SAFETY: this allocator is only installed where WebAssembly runs on a single thread, so the lists
// are never reached from two threads at once.
unsafe impl Sync for Recycling {}

/// The list that serves `layout`, and the layout of its blocks, if a block that small is kept.
fn kept_block(layout: Layout) -> Option<(usize, Layout)> {
    let size = layout.size();
    if size == 0 || size > MAX_KEPT_SIZE || layout.align() > ALIGN {
        return None;
    }
    let index = (size - 1) / GRANULE;
    let block = Layout::from_size_align((index + 1) * GRANULE, ALIGN).ok()?;
    Some((index, block))
}

impl Recycling {
    pub(crate) const fn new() -> Self {
        Self {
            kept: UnsafeCell::new(
                [Kept {
                    head: ptr::null_mut(),
                    count: 0,
                }; SIZES],
            ),
        }
    }

    /// The list at `index`, which no other reference reaches while this one is used.
    #[allow(clippy::mut_from_ref)]
    fn list(&self, index: usize) -> &mut Kept {
        // SAFETY: the lists are reached from one thread, and each method below holds a reference
        // to at most one list at a time, calling nothing that could reach the lists meanwhile.
        unsafe { &mut (*self.kept.get())[index] }
    }
}

unsafe impl GlobalAlloc for Recycling {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some((index, block)) = kept_block(layout) else {
            return unsafe { System.alloc(layout) };
        };
        let list = self.list(index);
        if list.count == 0 {
            return unsafe { System.alloc(block) };
        }
        let head = list.head;
        // SAFETY: a kept block is free, and starts with the pointer to the next.
        list.head = unsafe { head.cast::<*mut u8>().read() };
        list.count -= 1;
        head
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if kept_block(layout).is_none() {
            // dlmalloc knows when fresh memory is already zero.
            return unsafe { System.alloc_zeroed(layout) };
        }
        let pointer = unsafe { self.alloc(layout) };
        if !pointer.is_null() {
            // SAFETY: the block holds at least `layout.size()` bytes.
            unsafe { pointer.write_bytes(0, layout.size()) };
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let Some((index, block)) = kept_block(layout) else {
            return unsafe { System.dealloc(pointer, layout) };
        };
        let list = self.list(index);
        if list.count * block.size() >= KEPT_BYTES.max(4 * block.size()) {
            return unsafe { System.dealloc(pointer, block) };
        }
        // SAFETY: the block is free, aligned for a pointer, and at least a granule long.
        unsafe { pointer.cast::<*mut u8>().write(list.head) };
        list.head = pointer;
        list.count += 1;
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees a nonzero size that does not overflow at this alignment.
        let resized = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        match (kept_block(layout), kept_block(resized)) {
            // Blocks too large to keep came straight from dlmalloc, which can resize in place.
            (None, None) => unsafe { System.realloc(pointer, layout, new_size) },
            (Some((old, _)), Some((new, _))) if old == new => pointer,
            _ => {
                let moved = unsafe { self.alloc(resized) };
                if !moved.is_null() {
                    // SAFETY: the blocks are distinct, and both hold at least the smaller size.
                    unsafe {
                        ptr::copy_nonoverlapping(pointer, moved, layout.size().min(new_size));
                        self.dealloc(pointer, layout);
                    }
                }
                moved
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_reused_by_size_and_keep_their_contents_when_resized() {
        let recycling = Recycling::new();
        let layout = |size, align| Layout::from_size_align(size, align).unwrap();
        unsafe {
            // A freed block serves the next allocation of any size in its granule.
            let first = recycling.alloc(layout(40, 8));
            recycling.dealloc(first, layout(40, 8));
            assert_eq!(recycling.alloc(layout(33, 4)), first);
            recycling.dealloc(first, layout(33, 4));

            // Growing and shrinking across granules, and past the largest kept size, copies.
            let mut pointer = recycling.alloc(layout(3, 1));
            let mut size = 3;
            for next in [
                7,
                16,
                17,
                1000,
                MAX_KEPT_SIZE,
                MAX_KEPT_SIZE + 1,
                70_000,
                5,
                2,
            ] {
                for index in 0..size {
                    pointer.add(index).write(index as u8);
                }
                pointer = recycling.realloc(pointer, layout(size, 1), next);
                for index in 0..size.min(next) {
                    assert_eq!(pointer.add(index).read(), index as u8);
                }
                size = next;
            }
            recycling.dealloc(pointer, layout(size, 1));

            // Larger alignments, and zeroed blocks, whatever a reused block held.
            let aligned = recycling.alloc(layout(64, 64));
            assert_eq!(aligned as usize % 64, 0);
            recycling.dealloc(aligned, layout(64, 64));
            let dirty = recycling.alloc(layout(48, 8));
            dirty.write_bytes(0xff, 48);
            recycling.dealloc(dirty, layout(48, 8));
            let zeroed = recycling.alloc_zeroed(layout(48, 8));
            assert_eq!(zeroed, dirty);
            assert!((0..48).all(|index| zeroed.add(index).read() == 0));
            recycling.dealloc(zeroed, layout(48, 8));

            // A list keeps only so many blocks; the rest go back to the system allocator.
            let blocks: Vec<_> = (0..300).map(|_| recycling.alloc(layout(16, 8))).collect();
            for block in &blocks {
                recycling.dealloc(*block, layout(16, 8));
            }
            assert_eq!(recycling.list(0).count, KEPT_BYTES / 16);
        }
    }
}
