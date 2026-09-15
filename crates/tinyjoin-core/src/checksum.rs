// The standard IEEE CRC-32 checksum with a compile-time, 1-KiB lookup table.
// Keep the polynomial, initial state, and final complement compatible with stored pages.
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

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut checksum = u32::MAX;
    for byte in bytes {
        checksum = (checksum >> 8) ^ TABLE[((checksum ^ u32::from(*byte)) & 255) as usize];
    }
    !checksum
}

#[cfg(test)]
mod tests {
    use super::crc32;

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
