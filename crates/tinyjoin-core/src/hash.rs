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
//!
//! Each fingerprint is an XXH64 hash ([`xxh64`]), whose final avalanche leaves the hashes of
//! neighboring inputs uncorrelated, so that the exclusive-or of many stays close to uniform.

use crate::checksum::xxh64;

/// The fingerprint of an empty collection.
///
/// Exclusive-or makes this the identity element, so an empty subtree contributes nothing to its
/// ancestors.
pub(crate) const EMPTY_HASH: u64 = 0;

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
/// whose contents have been exchanged, so a fingerprint is bound to its name, as the XXH64 of the
/// name seeded with the fingerprint, before it is combined with its siblings.
pub(crate) fn identify(name: &[u8], hash: u64) -> u64 {
    xxh64(name, hash)
}

/// A quick multiplicative hash for the engine's hash maps and sets, whose keys are page IDs,
/// names, encoded keys, and grouped or joined values.
///
/// SipHash resists keys chosen to collide only when its keys are random, and WebAssembly without
/// a host source of randomness gives the standard library's hasher none: it derives them from
/// memory addresses. This hash gives up nothing there, costs less per key, and is a fraction of
/// SipHash's code.
#[derive(Default)]
pub(crate) struct KeyHasher(u64);

impl std::hash::Hasher for KeyHasher {
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

    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_isize(&mut self, value: isize) {
        self.write_u64(value as u64);
    }

    fn write_i128(&mut self, value: i128) {
        self.write_u64(value as u64);
        self.write_u64((value >> 64) as u64);
    }
}

/// A hash map keyed with [`KeyHasher`].
pub(crate) type KeyMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<KeyHasher>>;

/// A hash set keyed with [`KeyHasher`].
pub(crate) type KeySet<K> = std::collections::HashSet<K, std::hash::BuildHasherDefault<KeyHasher>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(bytes: &[u8]) -> u64 {
        xxh64(bytes, 0)
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
        // Sequential keys are the common case for a primary key, and a hash without a final
        // avalanche would leave their fingerprints correlated enough for whole ranges to cancel.
        let mut combined = EMPTY_HASH;
        for key in 0u64..1_024 {
            combined = combine(combined, hash_of(&key.to_le_bytes()));
        }
        assert_ne!(combined, EMPTY_HASH);
        assert!(combined.count_ones() > 16, "{combined:#018x} is not mixed");
    }
}
