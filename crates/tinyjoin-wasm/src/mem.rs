//! Comparison of memory a word at a time.
//!
//! The compiler's builtins compare memory a byte at a time on `wasm32`, and nearly every key the
//! engine compares, in a B-tree search, a sort, or a staged-row map, is eight bytes or more:
//! an encoded integer key alone is eight. Comparing eight bytes at a time takes a few
//! instructions where the byte loop takes dozens, and most of all before V8 optimizes the code,
//! when a statement over many rows runs the loop for every one of them.

#[cfg(any(target_arch = "wasm32", test))]
use std::cmp::Ordering;

/// The order of two byte strings of one length, comparing eight bytes at a time: their bytes are
/// read as little-endian words, which are equal exactly when the bytes are, and the first
/// differing pair is ordered as big-endian words, which is the order of the bytes.
#[cfg(any(target_arch = "wasm32", test))]
#[inline(always)]
fn compare(left: &[u8], right: &[u8]) -> Ordering {
    debug_assert_eq!(left.len(), right.len());
    let length = left.len();
    let mut offset = 0;
    while offset + 8 <= length {
        let (a, b) = (word(left, offset), word(right, offset));
        if a != b {
            return a.swap_bytes().cmp(&b.swap_bytes());
        }
        offset += 8;
    }
    while offset < length {
        let (a, b) = (left[offset], right[offset]);
        if a != b {
            return a.cmp(&b);
        }
        offset += 1;
    }
    Ordering::Equal
}

/// Whether two byte strings of one length differ, eight bytes at a time.
#[cfg(any(target_arch = "wasm32", test))]
#[inline(always)]
fn differ(left: &[u8], right: &[u8]) -> bool {
    debug_assert_eq!(left.len(), right.len());
    let length = left.len();
    let mut offset = 0;
    while offset + 8 <= length {
        if word(left, offset) != word(right, offset) {
            return true;
        }
        offset += 8;
    }
    while offset < length {
        if left[offset] != right[offset] {
            return true;
        }
        offset += 1;
    }
    false
}

/// The eight bytes at `offset` as a little-endian word, read unaligned, which WebAssembly loads
/// natively. Read byte by byte, the compiler would call the very function this replaces.
#[cfg(any(target_arch = "wasm32", test))]
#[inline(always)]
fn word(bytes: &[u8], offset: usize) -> u64 {
    let bytes = &bytes[offset..offset + 8];
    // SAFETY: the slice holds exactly eight bytes, and an unaligned read is permitted.
    unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<u64>()) }
}

/// `memcmp` and `bcmp`, which the compiler calls to compare and to test slices for equality,
/// provided here in place of the builtins' byte loops. Only the WebAssembly build defines them:
/// a native test binary has its platform's.
#[cfg(target_arch = "wasm32")]
mod overrides {
    /// # Safety
    /// `left` and `right` must each point to `length` readable bytes.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn memcmp(left: *const u8, right: *const u8, length: usize) -> i32 {
        // SAFETY: the caller promises both pointers address `length` readable bytes.
        let (left, right) = unsafe {
            (
                std::slice::from_raw_parts(left, length),
                std::slice::from_raw_parts(right, length),
            )
        };
        super::compare(left, right) as i32
    }

    /// # Safety
    /// `left` and `right` must each point to `length` readable bytes.
    #[unsafe(no_mangle)]
    pub(crate) unsafe extern "C" fn bcmp(left: *const u8, right: *const u8, length: usize) -> i32 {
        // SAFETY: the caller promises both pointers address `length` readable bytes.
        let (left, right) = unsafe {
            (
                std::slice::from_raw_parts(left, length),
                std::slice::from_raw_parts(right, length),
            )
        };
        i32::from(super::differ(left, right))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_as_the_bytes_order_at_every_length_and_offset() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for length in 0..40 {
            for _ in 0..200 {
                let left = (0..length).map(|_| (next() % 4) as u8).collect::<Vec<_>>();
                let mut right = left.clone();
                if length > 0 && next() % 3 != 0 {
                    let at = (next() as usize) % length;
                    right[at] = (next() % 256) as u8;
                }
                // Both at an odd offset too, since keys sit anywhere in a page.
                let (padded_left, padded_right) = (
                    [&[1][..], &left].concat(),
                    [&[1][..], &right].concat(),
                );
                for (left, right) in [(&left[..], &right[..]), (&padded_left[1..], &padded_right[1..])] {
                    assert_eq!(compare(left, right), left.cmp(right), "{left:?} {right:?}");
                    assert_eq!(differ(left, right), left != right, "{left:?} {right:?}");
                }
            }
        }
    }
}
