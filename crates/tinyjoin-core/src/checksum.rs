// The standard IEEE CRC-32 checksum, sixteen bytes at a time.
// Keep the polynomial, initial state, and final complement compatible with stored pages.
use std::sync::OnceLock;

/// The checksum of each byte value, built at compile time.
const TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut checksum = index as u32;
        let mut bit = 0;
        while bit < 8 {
            checksum = (checksum >> 1) ^ (0xedb8_8320 & (checksum & 1).wrapping_neg());
            bit += 1;
        }
        table[index] = checksum;
        index += 1;
    }
    table
};

/// Tables for "slicing by 16": entry `n` of table `k` is the checksum of byte value `n` followed
/// by `k` zero bytes, so sixteen independent lookups advance the checksum by sixteen bytes, where
/// one lookup per byte would each wait for the last. They are built on first use, since as
/// constants they would add 16 KiB to the engine's download.
static TABLES: OnceLock<Box<[[u32; 256]; 16]>> = OnceLock::new();

fn tables() -> &'static [[u32; 256]; 16] {
    TABLES.get_or_init(|| {
        let mut tables = Box::new([[0; 256]; 16]);
        tables[0] = TABLE;
        for slice in 1..16 {
            for byte in 0..256 {
                let previous = tables[slice - 1][byte];
                tables[slice][byte] = (previous >> 8) ^ TABLE[(previous & 255) as usize];
            }
        }
        tables
    })
}

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    !crc32_update(u32::MAX, bytes)
}

/// Continues a CRC-32 over further bytes. Start from `u32::MAX` and complement the result, as
/// [`crc32`] does, to checksum a message supplied in parts.
pub(crate) fn crc32_update(mut checksum: u32, bytes: &[u8]) -> u32 {
    let tables = tables();
    let (chunks, remainder) = bytes.as_chunks::<16>();
    for chunk in chunks {
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
        checksum = (checksum >> 8) ^ TABLE[((checksum ^ u32::from(*byte)) & 255) as usize];
    }
    checksum
}

#[cfg(test)]
mod tests {
    use super::{crc32, crc32_update};

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
}
