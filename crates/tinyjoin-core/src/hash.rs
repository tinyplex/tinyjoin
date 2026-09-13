//! Order-independent content fingerprints for stored data.
//!
//! These are non-cryptographic. They exist so that two databases holding the same logical data
//! can recognize that fact cheaply, and so that a future synchronization protocol can localize a
//! difference to a key range without scanning either side. They are not a defense against a
//! deliberately constructed collision, and they are never used to authenticate data.
//!
//! Entry fingerprints are combined with [`combine`], which is exclusive-or. That makes a subtree
//! fingerprint depend on the set of entries beneath it and not on the shape of the tree holding
//! them, so a page split, a copy-on-write rewrite, or a rebuild that redistributes the same
//! entries leaves every ancestor fingerprint unchanged. Because exclusive-or is self-inverse, an
//! ancestor can also be updated in place by removing an old child fingerprint and adding a new
//! one.

/// The fingerprint of an empty collection.
///
/// Exclusive-or makes this the identity element, so an empty subtree contributes nothing to its
/// ancestors.
pub(crate) const EMPTY_HASH: u64 = 0;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Accumulates length-framed fields into one 64-bit fingerprint.
///
/// Every variable-length field is framed with its length so that adjacent fields cannot be shifted
/// between one another without changing the result.
pub(crate) struct Hasher {
    state: u64,
}

impl Hasher {
    pub(crate) fn new() -> Self {
        Self {
            state: FNV_OFFSET_BASIS,
        }
    }

    pub(crate) fn write_u8(&mut self, value: u8) {
        self.state = (self.state ^ u64::from(value)).wrapping_mul(FNV_PRIME);
    }

    pub(crate) fn write_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.write_u8(byte);
        }
    }

    /// Writes a length-framed byte string.
    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        for byte in bytes {
            self.write_u8(*byte);
        }
    }

    /// Finishes with an avalanche step.
    ///
    /// FNV-1a leaves neighboring inputs highly correlated in their low bits, and correlated
    /// fingerprints would cancel each other in [`combine`]. Mixing the accumulator before it is
    /// combined keeps the exclusive-or of many entries close to uniform.
    pub(crate) fn finish(self) -> u64 {
        let mut state = self.state;
        state ^= state >> 30;
        state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        state ^= state >> 27;
        state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
        state ^= state >> 31;
        state
    }
}

/// Combines the fingerprints of two disjoint collections.
///
/// This is commutative, associative, and self-inverse, so callers may combine in any order and may
/// remove a previously combined fingerprint by combining it again.
pub(crate) const fn combine(left: u64, right: u64) -> u64 {
    left ^ right
}

/// Binds a fingerprint to the name of the collection that produced it.
///
/// [`combine`] alone cannot distinguish two identically named collections from two collections
/// whose contents have been exchanged, so a fingerprint is bound to its name before it is combined
/// with its siblings.
pub(crate) fn identify(name: &[u8], hash: u64) -> u64 {
    let mut hasher = Hasher::new();
    hasher.write_bytes(name);
    hasher.write_u64(hash);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(bytes: &[u8]) -> u64 {
        let mut hasher = Hasher::new();
        hasher.write_bytes(bytes);
        hasher.finish()
    }

    #[test]
    fn framing_separates_adjacent_fields() {
        let mut split = Hasher::new();
        split.write_bytes(b"ab");
        split.write_bytes(b"c");

        let mut shifted = Hasher::new();
        shifted.write_bytes(b"a");
        shifted.write_bytes(b"bc");

        assert_ne!(split.finish(), shifted.finish());
    }

    #[test]
    fn combination_is_order_independent_and_self_inverse() {
        let first = hash_of(b"first");
        let second = hash_of(b"second");
        let third = hash_of(b"third");

        let forwards = combine(combine(first, second), third);
        let backwards = combine(third, combine(second, first));
        assert_eq!(forwards, backwards);

        assert_eq!(combine(forwards, third), combine(first, second));
        assert_eq!(combine(first, first), EMPTY_HASH);
        assert_eq!(combine(first, EMPTY_HASH), first);
    }

    #[test]
    fn naming_distinguishes_exchanged_collections() {
        let left = hash_of(b"left rows");
        let right = hash_of(b"right rows");

        let correct = combine(identify(b"a", left), identify(b"b", right));
        let exchanged = combine(identify(b"a", right), identify(b"b", left));
        assert_ne!(correct, exchanged);
    }

    #[test]
    fn neighboring_inputs_do_not_cancel_in_combination() {
        // Sequential keys are the common case for a primary key, and an unmixed FNV-1a would leave
        // their fingerprints correlated enough for whole ranges to cancel.
        let mut combined = EMPTY_HASH;
        for key in 0u64..1_024 {
            let mut hasher = Hasher::new();
            hasher.write_u64(key);
            combined = combine(combined, hasher.finish());
        }
        assert_ne!(combined, EMPTY_HASH);
        assert!(combined.count_ones() > 16, "{combined:#018x} is not mixed");
    }
}
