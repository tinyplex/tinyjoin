use serde::{Serialize, de::DeserializeOwned};

use crate::{EngineError, Result};

const MAGIC: &[u8; 8] = b"TGRSNAP\0";
const FORMAT_VERSION: u16 = 1;
const FLAGS: u16 = 0;
const HEADER_LENGTH: usize = 20;

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(value).map_err(|error| {
        EngineError::new(
            "SNAPSHOT_SERIALIZATION_FAILED",
            format!("Could not encode database snapshot: {error}"),
        )
    })?;
    let payload_length = u32::try_from(payload.len()).map_err(|_| {
        EngineError::new(
            "SNAPSHOT_TOO_LARGE",
            "Database snapshot exceeds the 4 GiB format limit",
        )
    })?;
    let encoded_length = HEADER_LENGTH.checked_add(payload.len()).ok_or_else(|| {
        EngineError::new(
            "SNAPSHOT_TOO_LARGE",
            "Database snapshot exceeds this runtime's address space",
        )
    })?;

    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.extend_from_slice(MAGIC);
    encoded.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    encoded.extend_from_slice(&FLAGS.to_le_bytes());
    encoded.extend_from_slice(&payload_length.to_le_bytes());
    encoded.extend_from_slice(&crc32(&payload).to_le_bytes());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() < HEADER_LENGTH {
        return Err(EngineError::invalid_snapshot(
            "Snapshot is shorter than its header",
        ));
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err(EngineError::invalid_snapshot(
            "Snapshot has an invalid file signature",
        ));
    }

    let version = read_u16(bytes, 8);
    if version != FORMAT_VERSION {
        return Err(EngineError::unsupported_snapshot(format!(
            "Snapshot format version {version} is not supported"
        )));
    }

    let flags = read_u16(bytes, 10);
    if flags != FLAGS {
        return Err(EngineError::unsupported_snapshot(format!(
            "Snapshot uses unsupported format flags 0x{flags:04x}"
        )));
    }

    let payload_length = read_u32(bytes, 12) as usize;
    let expected_length = HEADER_LENGTH.checked_add(payload_length).ok_or_else(|| {
        EngineError::invalid_snapshot("Snapshot payload length overflows this runtime")
    })?;
    if bytes.len() != expected_length {
        return Err(EngineError::invalid_snapshot(format!(
            "Snapshot length is {}, but its header declares {expected_length}",
            bytes.len()
        )));
    }

    let payload = &bytes[HEADER_LENGTH..];
    let expected_checksum = read_u32(bytes, 16);
    let actual_checksum = crc32(payload);
    if actual_checksum != expected_checksum {
        return Err(EngineError::invalid_snapshot(
            "Snapshot payload checksum does not match",
        ));
    }

    serde_json::from_slice(payload).map_err(|error| {
        EngineError::invalid_snapshot(format!("Snapshot payload is not valid: {error}"))
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

// A compact tableless implementation of the standard IEEE CRC-32 checksum.
fn crc32(bytes: &[u8]) -> u32 {
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

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Payload {
        value: String,
    }

    fn encoded() -> Vec<u8> {
        encode(&Payload {
            value: "kept".to_owned(),
        })
        .unwrap()
    }

    #[test]
    fn snapshot_envelope_round_trips() {
        let bytes = encoded();
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(decode::<Payload>(&bytes).unwrap().value, "kept");
    }

    #[test]
    fn envelope_rejects_invalid_header_fields() {
        let cases: [(usize, u8, &str); 4] = [
            (0, b'X', "INVALID_SNAPSHOT"),
            (8, 2, "UNSUPPORTED_SNAPSHOT"),
            (10, 1, "UNSUPPORTED_SNAPSHOT"),
            (12, 0, "INVALID_SNAPSHOT"),
        ];

        for (offset, replacement, expected_code) in cases {
            let mut bytes = encoded();
            bytes[offset] = replacement;
            assert_eq!(
                decode::<Payload>(&bytes).unwrap_err().code,
                expected_code,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn envelope_rejects_every_truncation_and_payload_corruption() {
        let complete = encoded();
        for length in 0..complete.len() {
            assert_eq!(
                decode::<Payload>(&complete[..length]).unwrap_err().code,
                "INVALID_SNAPSHOT",
                "truncated to {length} bytes"
            );
        }

        let mut corrupted = complete;
        *corrupted.last_mut().unwrap() ^= 1;
        assert_eq!(
            decode::<Payload>(&corrupted).unwrap_err().code,
            "INVALID_SNAPSHOT"
        );
    }

    #[test]
    fn envelope_rejects_invalid_json_with_a_valid_checksum() {
        let mut bytes = encoded();
        let payload = b"not-json";
        bytes.truncate(HEADER_LENGTH);
        bytes[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&crc32(payload).to_le_bytes());
        bytes.extend_from_slice(payload);

        assert_eq!(
            decode::<Payload>(&bytes).unwrap_err().code,
            "INVALID_SNAPSHOT"
        );
    }
}
