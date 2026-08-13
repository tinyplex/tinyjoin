const JOURNAL_MAGIC = new Uint8Array([
  0x54, 0x47, 0x52, 0x57, 0x41, 0x4c, 0x00, 0x01,
]);
const JOURNAL_VERSION = 1;
const JOURNAL_FLAGS = 0;
const JOURNAL_HEADER_BYTES = 28;

const RECORD_MAGIC = new Uint8Array([0x54, 0x47, 0x54, 0x58]);
const RECORD_COMMIT_MAGIC = new Uint8Array([0x54, 0x47, 0x4f, 0x4b]);
const LEGACY_MUTATIONS_RECORD_VERSION = 1;
const PREPARED_COMMIT_RECORD_VERSION = 2;
const RECORD_FLAGS = 0;
const RECORD_HEADER_BYTES = 24;
const RECORD_FOOTER_BYTES = 8;
const MAX_SEQUENCE = 0xffff_ffff_ffff_ffffn;

// A single transaction may contain a source table replacement. Keep a hard
// allocation bound until persistence moves to pages or streaming records.
export const MAX_JOURNAL_RECORD_BYTES = 16 * 1024 * 1024;

export interface JournalRecord {
  sequence: bigint;
  kind: JournalRecordKind;
  payload: Uint8Array;
}

export type JournalRecordKind = 'legacy-mutations' | 'prepared-commit';

export interface JournalScan {
  baseSequence: bigint;
  records: readonly JournalRecord[];
  /** Prefix safe to retain when `tail` is `torn`. */
  validBytes: number;
  tail: 'clean' | 'torn';
}

export class JournalFormatError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = 'JournalFormatError';
    this.code = code;
  }
}

export function encodeJournalHeader(baseSequence: bigint): Uint8Array {
  assertSequence(baseSequence);
  const header = new Uint8Array(JOURNAL_HEADER_BYTES);
  header.set(JOURNAL_MAGIC);
  const view = new DataView(header.buffer);
  view.setUint32(8, JOURNAL_VERSION, true);
  view.setUint32(12, JOURNAL_FLAGS, true);
  view.setBigUint64(16, baseSequence, true);
  view.setUint32(24, crc32(header.subarray(0, 24)), true);
  return header;
}

export function encodeJournalRecord(
  sequence: bigint,
  payload: Uint8Array,
  kind: JournalRecordKind = 'prepared-commit',
): Uint8Array {
  assertSequence(sequence);
  if (sequence === 0n) {
    throw new JournalFormatError(
      'STORAGE_SEQUENCE_INVALID',
      'A journal record sequence must be greater than zero',
    );
  }
  if (payload.byteLength > MAX_JOURNAL_RECORD_BYTES) {
    throw new JournalFormatError(
      'STORAGE_JOURNAL_RECORD_TOO_LARGE',
      `A journal transaction cannot exceed ${MAX_JOURNAL_RECORD_BYTES} bytes`,
    );
  }

  const encoded = new Uint8Array(
    RECORD_HEADER_BYTES + payload.byteLength + RECORD_FOOTER_BYTES,
  );
  encoded.set(RECORD_MAGIC);
  const view = new DataView(encoded.buffer);
  view.setUint16(4, recordVersion(kind), true);
  view.setUint16(6, RECORD_FLAGS, true);
  view.setBigUint64(8, sequence, true);
  view.setUint32(16, payload.byteLength, true);
  view.setUint32(20, crc32(encoded.subarray(0, 20)), true);
  encoded.set(payload, RECORD_HEADER_BYTES);

  const footerOffset = RECORD_HEADER_BYTES + payload.byteLength;
  view.setUint32(footerOffset, crc32(encoded.subarray(0, footerOffset)), true);
  encoded.set(RECORD_COMMIT_MAGIC, footerOffset + 4);
  return encoded;
}

/**
 * Validates a complete journal image without interpreting transaction payloads.
 * Only an incomplete final record is recoverably torn; a fully framed record
 * with a bad checksum is corruption and must not be silently discarded.
 */
export function scanJournal(bytes: Uint8Array): JournalScan {
  if (bytes.byteLength < JOURNAL_HEADER_BYTES) {
    throw new JournalFormatError(
      'STORAGE_JOURNAL_HEADER_TORN',
      'The TinyGres journal header is incomplete',
    );
  }
  if (!matches(bytes, 0, JOURNAL_MAGIC)) {
    throw new JournalFormatError(
      'STORAGE_JOURNAL_CORRUPT',
      'The TinyGres journal has an invalid file signature',
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (view.getUint32(24, true) !== crc32(bytes.subarray(0, 24))) {
    throw new JournalFormatError(
      'STORAGE_JOURNAL_CORRUPT',
      'The TinyGres journal header checksum does not match',
    );
  }
  if (view.getUint32(8, true) !== JOURNAL_VERSION) {
    throw new JournalFormatError(
      'STORAGE_VERSION_UNSUPPORTED',
      'The TinyGres journal uses an unsupported format version',
    );
  }
  if (view.getUint32(12, true) !== JOURNAL_FLAGS) {
    throw new JournalFormatError(
      'STORAGE_VERSION_UNSUPPORTED',
      'The TinyGres journal uses unsupported format flags',
    );
  }

  const baseSequence = view.getBigUint64(16, true);
  const records: JournalRecord[] = [];
  let expectedSequence = baseSequence;
  let offset = JOURNAL_HEADER_BYTES;

  while (offset < bytes.byteLength) {
    const remaining = bytes.byteLength - offset;
    if (remaining < RECORD_HEADER_BYTES) {
      return {baseSequence, records, validBytes: offset, tail: 'torn'};
    }
    if (!matches(bytes, offset, RECORD_MAGIC)) {
      throw corruptRecord('file signature', expectedSequence + 1n);
    }

    const header = bytes.subarray(offset, offset + RECORD_HEADER_BYTES);
    const headerView = new DataView(
      header.buffer,
      header.byteOffset,
      header.byteLength,
    );
    if (headerView.getUint32(20, true) !== crc32(header.subarray(0, 20))) {
      throw corruptRecord('header checksum', expectedSequence + 1n);
    }

    const payloadLength = headerView.getUint32(16, true);
    if (payloadLength > MAX_JOURNAL_RECORD_BYTES) {
      throw corruptRecord('payload length', expectedSequence + 1n);
    }
    const recordBytes =
      RECORD_HEADER_BYTES + payloadLength + RECORD_FOOTER_BYTES;
    if (remaining < recordBytes) {
      return {baseSequence, records, validBytes: offset, tail: 'torn'};
    }

    const sequence = headerView.getBigUint64(8, true);
    if (sequence !== expectedSequence + 1n) {
      throw corruptRecord('sequence', expectedSequence + 1n);
    }
    const footerOffset = offset + RECORD_HEADER_BYTES + payloadLength;
    if (!matches(bytes, footerOffset + 4, RECORD_COMMIT_MAGIC)) {
      throw corruptRecord('commit marker', sequence);
    }
    if (
      view.getUint32(footerOffset, true) !==
      crc32(bytes.subarray(offset, footerOffset))
    ) {
      throw corruptRecord('record checksum', sequence);
    }
    const kind = recordKind(headerView.getUint16(4, true));
    if (kind === undefined) {
      throw new JournalFormatError(
        'STORAGE_VERSION_UNSUPPORTED',
        `TinyGres journal transaction ${sequence} uses an unsupported format version`,
      );
    }
    if (headerView.getUint16(6, true) !== RECORD_FLAGS) {
      throw new JournalFormatError(
        'STORAGE_VERSION_UNSUPPORTED',
        `TinyGres journal transaction ${sequence} uses unsupported format flags`,
      );
    }

    records.push({
      sequence,
      kind,
      payload: bytes.slice(offset + RECORD_HEADER_BYTES, footerOffset),
    });
    expectedSequence = sequence;
    offset += recordBytes;
  }

  return {baseSequence, records, validBytes: offset, tail: 'clean'};
}

function recordVersion(kind: JournalRecordKind): number {
  return kind === 'legacy-mutations'
    ? LEGACY_MUTATIONS_RECORD_VERSION
    : PREPARED_COMMIT_RECORD_VERSION;
}

function recordKind(version: number): JournalRecordKind | undefined {
  if (version === LEGACY_MUTATIONS_RECORD_VERSION) {
    return 'legacy-mutations';
  }
  if (version === PREPARED_COMMIT_RECORD_VERSION) {
    return 'prepared-commit';
  }
  return undefined;
}

function assertSequence(sequence: bigint): void {
  if (sequence < 0n || sequence > MAX_SEQUENCE) {
    throw new JournalFormatError(
      'STORAGE_SEQUENCE_INVALID',
      'A journal sequence must be an unsigned 64-bit integer',
    );
  }
}

function corruptRecord(field: string, sequence: bigint): JournalFormatError {
  return new JournalFormatError(
    'STORAGE_JOURNAL_CORRUPT',
    `TinyGres journal transaction ${sequence} has an invalid ${field}`,
  );
}

function matches(
  bytes: Uint8Array,
  offset: number,
  expected: Uint8Array,
): boolean {
  return expected.every((byte, index) => bytes[offset + index] === byte);
}

const CRC32_TABLE = new Uint32Array(256).map((_, index) => {
  let value = index;
  for (let bit = 0; bit < 8; bit += 1) {
    value = (value & 1) === 1 ? 0xedb8_8320 ^ (value >>> 1) : value >>> 1;
  }
  return value >>> 0;
});

function crc32(bytes: Uint8Array): number {
  let value = 0xffff_ffff;
  for (const byte of bytes) {
    value = CRC32_TABLE[(value ^ byte) & 0xff]! ^ (value >>> 8);
  }
  return (value ^ 0xffff_ffff) >>> 0;
}
