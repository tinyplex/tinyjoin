//! The checksums that stored pages and values carry, and the hash that fingerprints are built from.
//!
//! Superblocks carry the standard IEEE CRC-32, as every page of v0.1.0 through v0.3.0 did. Other
//! pages, overflow values and fingerprints use XXH64, the 64-bit xxHash, which reads eight bytes at
//! a time through four independent lanes, where a table-driven CRC-32 needs a lookup for every byte.

// The standard IEEE CRC-32 checksum, sixteen bytes at a time.
// Keep the polynomial, initial state, and final complement compatible with stored superblocks.
use std::sync::OnceLock;

/// Tables for "slicing by 16": entry `n` of table `k` is the checksum of byte value `n` followed
/// by `k` zero bytes, so sixteen independent lookups advance the checksum by sixteen bytes, where
/// one lookup per byte would each wait for the last. Table 0 is the checksum of each byte value
/// alone. They are built on first use, since as constants, even table 0 alone, they would add
/// their high-entropy bytes to the engine's download.
///
/// Four more advance a checksum over 256 zero bytes: entry `n` of table `16 + k` is what the state
/// `n << 8k` becomes. Over zero bytes, each bit of the state moves the result independently, so
/// four lookups pass a run that would take sixty-four. Pages are mostly zero wherever they are
/// not full, from the free space in a B-tree node to an allocation bitmap's unused pages.
static TABLES: OnceLock<Box<[[u32; 256]; 20]>> = OnceLock::new();

fn tables() -> &'static [[u32; 256]; 20] {
    TABLES.get_or_init(|| {
        let mut tables = Box::new([[0; 256]; 20]);
        for (byte, entry) in tables[0].iter_mut().enumerate() {
            let mut checksum = byte as u32;
            for _ in 0..8 {
                checksum = (checksum >> 1) ^ (0xedb8_8320 & (checksum & 1).wrapping_neg());
            }
            *entry = checksum;
        }
        for slice in 1..16 {
            for byte in 0..256 {
                let previous = tables[slice - 1][byte];
                tables[slice][byte] = (previous >> 8) ^ tables[0][(previous & 255) as usize];
            }
        }
        for bit in 0..32 {
            let mut state = 1 << bit;
            for _ in 0..16 {
                state = skip(&tables, state);
            }
            let mask = 1 << (bit % 8);
            for (value, entry) in tables[16 + bit / 8].iter_mut().enumerate() {
                if value & mask != 0 {
                    *entry ^= state;
                }
            }
        }
        tables
    })
}

/// The state after sixteen zero bytes: a slicing step with nothing but the state folded in.
fn skip(tables: &[[u32; 256]; 20], state: u32) -> u32 {
    tables[15][(state & 255) as usize]
        ^ tables[14][((state >> 8) & 255) as usize]
        ^ tables[13][((state >> 16) & 255) as usize]
        ^ tables[12][(state >> 24) as usize]
}

#[cfg(test)]
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    !crc32_update(u32::MAX, bytes)
}

/// Continues a CRC-32 over further bytes. Start from `u32::MAX` and complement the result, as
/// [`crc32`] does, to checksum a message supplied in parts.
pub(crate) fn crc32_update(mut checksum: u32, bytes: &[u8]) -> u32 {
    let tables = tables();
    let (chunks, remainder) = bytes.as_chunks::<16>();
    let mut index = 0;
    while let Some(chunk) = chunks.get(index) {
        if u128::from_ne_bytes(*chunk) == 0 {
            let run = chunks[index..]
                .iter()
                .take_while(|chunk| u128::from_ne_bytes(**chunk) == 0)
                .count();
            index += run;
            for _ in 0..run / 16 {
                checksum = tables[16][(checksum & 255) as usize]
                    ^ tables[17][((checksum >> 8) & 255) as usize]
                    ^ tables[18][((checksum >> 16) & 255) as usize]
                    ^ tables[19][(checksum >> 24) as usize];
            }
            for _ in 0..run % 16 {
                checksum = skip(tables, checksum);
            }
            continue;
        }
        index += 1;
        let word = |offset: usize| {
            u32::from_le_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ])
        };
        let (first, second, third, fourth) = (word(0) ^ checksum, word(4), word(8), word(12));
        let at =
            |slice: usize, word: u32, shift: u32| tables[slice][((word >> shift) & 255) as usize];
        checksum = at(15, first, 0)
            ^ at(14, first, 8)
            ^ at(13, first, 16)
            ^ at(12, first, 24)
            ^ at(11, second, 0)
            ^ at(10, second, 8)
            ^ at(9, second, 16)
            ^ at(8, second, 24)
            ^ at(7, third, 0)
            ^ at(6, third, 8)
            ^ at(5, third, 16)
            ^ at(4, third, 24)
            ^ at(3, fourth, 0)
            ^ at(2, fourth, 8)
            ^ at(1, fourth, 16)
            ^ at(0, fourth, 24);
    }
    for byte in remainder {
        checksum = (checksum >> 8) ^ tables[0][((checksum ^ u32::from(*byte)) & 255) as usize];
    }
    checksum
}

const PRIME_1: u64 = 0x9e37_79b1_85eb_ca87;
const PRIME_2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const PRIME_3: u64 = 0x1656_67b1_9e37_79f9;
const PRIME_4: u64 = 0x85eb_ca77_c2b2_ae63;
const PRIME_5: u64 = 0x27d4_eb2f_1656_67c5;

#[inline(always)]
fn round(accumulator: u64, lane: u64) -> u64 {
    accumulator
        .wrapping_add(lane.wrapping_mul(PRIME_2))
        .rotate_left(31)
        .wrapping_mul(PRIME_1)
}

/// XXH64 of a message given as its whole 32-byte stripes, each as four little-endian lanes, and
/// then the bytes after them. A caller that reads the stripes itself can choose which to add.
pub(crate) struct Xxh64 {
    seed: u64,
    accumulators: [u64; 4],
    striped: bool,
}

impl Xxh64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            seed,
            accumulators: [
                seed.wrapping_add(PRIME_1).wrapping_add(PRIME_2),
                seed.wrapping_add(PRIME_2),
                seed,
                seed.wrapping_sub(PRIME_1),
            ],
            striped: false,
        }
    }

    /// The four lanes of a stripe.
    #[inline(always)]
    pub(crate) fn lanes(stripe: &[u8; 32]) -> [u64; 4] {
        let (words, _) = stripe.as_chunks::<8>();
        [
            u64::from_le_bytes(words[0]),
            u64::from_le_bytes(words[1]),
            u64::from_le_bytes(words[2]),
            u64::from_le_bytes(words[3]),
        ]
    }

    /// Adds the message's next stripe.
    #[inline(always)]
    pub(crate) fn stripe(&mut self, lanes: [u64; 4]) {
        let [first, second, third, fourth] = &mut self.accumulators;
        *first = round(*first, lanes[0]);
        *second = round(*second, lanes[1]);
        *third = round(*third, lanes[2]);
        *fourth = round(*fourth, lanes[3]);
        self.striped = true;
    }

    /// The hash of a message of `length` bytes, whose bytes after the stripes added are `tail`.
    pub(crate) fn finish(self, length: u64, tail: &[u8]) -> u64 {
        let mut hash = if self.striped {
            let [first, second, third, fourth] = self.accumulators;
            let hash = first
                .rotate_left(1)
                .wrapping_add(second.rotate_left(7))
                .wrapping_add(third.rotate_left(12))
                .wrapping_add(fourth.rotate_left(18));
            self.accumulators.iter().fold(hash, |hash, accumulator| {
                (hash ^ round(0, *accumulator))
                    .wrapping_mul(PRIME_1)
                    .wrapping_add(PRIME_4)
            })
        } else {
            self.seed.wrapping_add(PRIME_5)
        };
        hash = hash.wrapping_add(length);
        let (words, mut rest) = tail.as_chunks::<8>();
        for word in words {
            hash = (hash ^ round(0, u64::from_le_bytes(*word)))
                .rotate_left(27)
                .wrapping_mul(PRIME_1)
                .wrapping_add(PRIME_4);
        }
        if let Some((word, after)) = rest.split_first_chunk::<4>() {
            hash = (hash ^ u64::from(u32::from_le_bytes(*word)).wrapping_mul(PRIME_1))
                .rotate_left(23)
                .wrapping_mul(PRIME_2)
                .wrapping_add(PRIME_3);
            rest = after;
        }
        for byte in rest {
            hash = (hash ^ u64::from(*byte).wrapping_mul(PRIME_5))
                .rotate_left(11)
                .wrapping_mul(PRIME_1);
        }
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(PRIME_2);
        hash ^= hash >> 29;
        hash = hash.wrapping_mul(PRIME_3);
        hash ^ (hash >> 32)
    }
}

/// XXH64 of `bytes` with `seed`.
pub(crate) fn xxh64(bytes: &[u8], seed: u64) -> u64 {
    let (stripes, tail) = bytes.as_chunks::<32>();
    let mut hash = Xxh64::new(seed);
    for stripe in stripes {
        hash.stripe(Xxh64::lanes(stripe));
    }
    hash.finish(bytes.len() as u64, tail)
}

/// The 32-bit checksum of `bytes`: their XXH64, folded into the 32 bits that a stored checksum
/// field holds.
pub(crate) fn checksum32(bytes: &[u8]) -> u32 {
    fold(xxh64(bytes, 0))
}

/// Folds a 64-bit hash into 32 bits, keeping a contribution from every bit.
pub(crate) fn fold(hash: u64) -> u32 {
    (hash ^ (hash >> 32)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_a_message_supplied_in_parts() {
        let bytes = b"The quick brown fox jumps over the lazy dog";
        for split in 0..=bytes.len() {
            let (left, right) = bytes.split_at(split);
            assert_eq!(
                !crc32_update(crc32_update(u32::MAX, left), right),
                crc32(bytes)
            );
        }
    }

    #[test]
    fn continues_from_any_state_over_any_alignment() {
        let bytes = (0..1_000u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect::<Vec<_>>();
        for split in (0..=bytes.len()).step_by(7) {
            let (left, right) = bytes.split_at(split);
            assert_eq!(
                !crc32_update(crc32_update(u32::MAX, left), right),
                tableless_crc32(&bytes)
            );
        }
    }

    #[test]
    fn matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    // Preserve the previous calculation as an independent storage-compatibility oracle.
    fn tableless_crc32(bytes: &[u8]) -> u32 {
        let mut checksum = u32::MAX;
        for byte in bytes {
            checksum ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (checksum & 1).wrapping_neg();
                checksum = (checksum >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !checksum
    }

    #[test]
    fn preserves_checksums_for_byte_values_pages_and_overflow_payloads() {
        for byte in 0..=255u8 {
            assert_eq!(crc32(&[byte]), tableless_crc32(&[byte]));
        }
        let mut state = 0x1234_5678u32;
        let bytes: Vec<u8> = (0..1_048_576)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        for length in (0..=512).chain([1024, 4095, 4096, 4097, 65536, 1_048_576]) {
            assert_eq!(crc32(&bytes[..length]), tableless_crc32(&bytes[..length]));
        }
        for byte in [0, 255] {
            let page = [byte; 4096];
            assert_eq!(crc32(&page), tableless_crc32(&page));
        }
    }

    #[test]
    fn passes_zero_runs_of_every_length_and_alignment() {
        let mut state = 0x9e37_79b9u32;
        let mut noise = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8 | 1
        };
        for run in (0..=48).chain([255, 256, 257, 511, 512, 4000, 4096]) {
            for before in 0..20 {
                let mut bytes = (0..before).map(|_| noise()).collect::<Vec<_>>();
                bytes.resize(before + run, 0);
                bytes.extend((0..19).map(|_| noise()));
                assert_eq!(
                    crc32(&bytes),
                    tableless_crc32(&bytes),
                    "{run} after {before}"
                );
            }
        }
    }

    /// The bytes the XXH64 tests hash: a fixed sequence with no runs or repeats to hide a slip.
    fn sample() -> Vec<u8> {
        (0..100u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect()
    }

    #[test]
    fn xxh64_matches_the_reference_implementation() {
        // From xxhsum 0.8.3 and its library, over the empty message and every path through the
        // stripes, eight-byte words, a four-byte word, and single bytes.
        assert_eq!(xxh64(b"", 0), 0xef46_db37_51d8_e999);
        assert_eq!(xxh64(b"abc", 0), 0x44bc_2cf5_ad77_0999);
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition", 0),
            0xfbce_a83c_8a37_8bf1
        );
        assert_eq!(xxh64(b"xxhash", 20), 0x48b3_5aa9_8dc0_4f56);
        let bytes = sample();
        for (length, expected) in [
            (1, 0xe934_a84a_db05_2768),
            (3, 0xa9cf_36b4_1f9e_7d09),
            (4, 0x435f_59a3_3b7e_b3d1),
            (5, 0x75ef_30ae_ac84_70ba),
            (8, 0x538c_ac3b_18f9_ef8e),
            (12, 0x52b6_afe4_26c6_92ff),
            (15, 0x2411_84d4_482f_f811),
            (31, 0x4071_dd13_10fa_5da9),
            (32, 0x13ee_8a64_346f_0691),
            (33, 0xf756_1949_9e2e_2e99),
            (36, 0xcd7b_59a4_431d_8f19),
            (40, 0xf410_4b61_6a9d_9f92),
            (47, 0xd42c_869a_a669_1d68),
            (63, 0x63a8_9bd4_f10c_9d1e),
            (64, 0xfb24_d94d_e825_912f),
            (65, 0xb853_7309_01e1_daee),
            (71, 0x2fc6_ab8f_d672_bcf0),
            (96, 0xbaac_cebd_67c9_f947),
            (99, 0xd3bf_f86f_0707_605d),
        ] {
            assert_eq!(xxh64(&bytes[..length], 0), expected, "{length} bytes");
        }
        for (length, seed, expected) in [
            (0, 7, 0x95f0_626f_6f0a_4409),
            (6, 7, 0x3da8_e5f0_903a_0167),
            (31, 7, 0xe2fe_f709_4809_09fd),
            (32, 7, 0xa26f_d535_fa47_e6d1),
            (40, 7, 0x8e16_7f75_a0db_57c8),
            (99, 7, 0x01ab_92ba_cf92_4542),
            (0, 0x9e37_79b9_7f4a_7c15, 0xc434_9fc9_3c01_0000),
            (6, 0x9e37_79b9_7f4a_7c15, 0x22b2_9dcd_3fef_ed70),
            (31, 0x9e37_79b9_7f4a_7c15, 0xb804_1a54_25fa_b416),
            (32, 0x9e37_79b9_7f4a_7c15, 0x8cb4_af26_4843_08e7),
            (40, 0x9e37_79b9_7f4a_7c15, 0xc6b9_51c4_82c4_a3da),
            (99, 0x9e37_79b9_7f4a_7c15, 0x7602_9c03_26eb_d252),
        ] {
            assert_eq!(xxh64(&bytes[..length], seed), expected, "{length} bytes");
        }
    }

    #[test]
    fn xxh64_reads_a_message_given_as_stripes_and_a_tail() {
        let bytes = sample();
        for length in 0..bytes.len() {
            let (stripes, tail) = bytes[..length].as_chunks::<32>();
            let mut hash = Xxh64::new(7);
            for stripe in stripes {
                hash.stripe(Xxh64::lanes(stripe));
            }
            assert_eq!(hash.finish(length as u64, tail), xxh64(&bytes[..length], 7));
        }
    }

    #[test]
    fn checksum32_folds_both_halves_of_the_hash() {
        assert_eq!(fold(0x0000_0001_0000_0000), 1);
        assert_eq!(fold(0x8000_0000_0000_0001), 0x8000_0001);
        assert_eq!(checksum32(b"abc"), 0x44bc_2cf5 ^ 0xad77_0999);
    }
}
