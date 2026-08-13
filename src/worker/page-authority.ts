import {PAGE_SIZE, MAX_PAGES} from './page-device.js';
import {StorageError} from './storage-error.js';

const SNAPSHOT_MAGIC = new Uint8Array([
  0x54, 0x47, 0x52, 0x53, 0x4f, 0x50, 0x46, 0x31,
]);
const PAGE_PAYLOAD_MAGIC = new Uint8Array([
  0x54, 0x47, 0x52, 0x50, 0x41, 0x47, 0x45, 0x00,
]);
const TOMBSTONE_VERSION = 0x8000_0001;
const TOMBSTONE_GENERATION = 1n;
const TOMBSTONE_FORMAT = 1;
const PAGE_FORMAT = 1;
const HEADER_BYTES = 36;
const PAYLOAD_BYTES = 32;
const CHECKSUM_OFFSET = 32;
const MAX_U64 = 0xffff_ffff_ffff_ffffn;
const MAX_DATABASE_REVISION = 9_007_199_254_740_991n;

export const PAGE_AUTHORITY_RECORD_BYTES = HEADER_BYTES + PAYLOAD_BYTES;

export interface PageAuthorityMarker {
  appliedJournalSequence: bigint;
  databaseRevision: bigint;
}

export type PageAuthorityClassification =
  | {kind: 'legacy'}
  | {kind: 'staged'}
  | {kind: 'page'; marker: PageAuthorityMarker};

/**
 * Encodes the cross-version authority marker stored in legacy snapshot slot A.
 *
 * The outer shape deliberately remains a checksum-valid snapshot-v2 envelope
 * with an unsupported version. Released legacy workers therefore fail with
 * `STORAGE_VERSION_UNSUPPORTED` instead of opening the retained slot B.
 */
export function encodePageAuthorityMarker(
  marker: PageAuthorityMarker,
): Uint8Array {
  assertU64(marker.appliedJournalSequence, 'journal sequence');
  assertDatabaseRevision(marker.databaseRevision);

  const record = new Uint8Array(PAGE_AUTHORITY_RECORD_BYTES);
  record.set(SNAPSHOT_MAGIC);
  const view = new DataView(record.buffer);
  view.setUint32(8, TOMBSTONE_VERSION, true);
  view.setBigUint64(12, TOMBSTONE_GENERATION, true);
  view.setUint32(20, PAYLOAD_BYTES, true);
  view.setBigUint64(24, marker.appliedJournalSequence, true);

  record.set(PAGE_PAYLOAD_MAGIC, HEADER_BYTES);
  view.setUint32(HEADER_BYTES + 8, TOMBSTONE_FORMAT, true);
  view.setUint32(HEADER_BYTES + 12, PAGE_FORMAT, true);
  view.setUint32(HEADER_BYTES + 16, PAGE_SIZE, true);
  view.setUint32(HEADER_BYTES + 20, MAX_PAGES, true);
  view.setBigUint64(HEADER_BYTES + 24, marker.databaseRevision, true);
  view.setUint32(
    CHECKSUM_OFFSET,
    crc32([
      record.subarray(8, CHECKSUM_OFFSET),
      record.subarray(HEADER_BYTES),
    ]),
    true,
  );
  return record;
}

/**
 * Classifies snapshot slot A without ever falling back across a committed page
 * marker. A body carrying the tombstone version but lacking the complete magic
 * is an interrupted pre-marker publication and must recover from legacy slot B.
 */
export function classifyPageAuthorityRecord(
  bytes: Uint8Array,
): PageAuthorityClassification {
  const version =
    bytes.byteLength >= 12
      ? new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(
          8,
          true,
        )
      : undefined;
  if (version !== TOMBSTONE_VERSION) {
    // The payload signature is deliberately redundant with the outer version.
    // If either survives, a damaged committed marker must never be mistaken for
    // legacy authority and allowed to roll back to the retained slot B.
    if (matches(bytes, HEADER_BYTES, PAGE_PAYLOAD_MAGIC)) {
      throw corruptMarker('has a damaged authority envelope');
    }
    return {kind: 'legacy'};
  }
  if (!matches(bytes, 0, SNAPSHOT_MAGIC)) {
    if (isCommitMagicPrefix(bytes)) {
      return {kind: 'staged'};
    }
    throw corruptMarker('has a damaged commit marker');
  }
  return {kind: 'page', marker: decodeCommittedMarker(bytes)};
}

/** Returns an exact pre-marker image whose final eight-byte magic is zero. */
export function stagePageAuthorityMarker(
  marker: PageAuthorityMarker,
): Uint8Array {
  const record = encodePageAuthorityMarker(marker);
  record.fill(0, 0, SNAPSHOT_MAGIC.byteLength);
  return record;
}

/** The only bytes written during the final authority publication step. */
export function pageAuthorityCommitMagic(): Uint8Array {
  return SNAPSHOT_MAGIC.slice();
}

function decodeCommittedMarker(bytes: Uint8Array): PageAuthorityMarker {
  if (bytes.byteLength !== PAGE_AUTHORITY_RECORD_BYTES) {
    throw corruptMarker('has an invalid byte length');
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (
    view.getBigUint64(12, true) !== TOMBSTONE_GENERATION ||
    view.getUint32(20, true) !== PAYLOAD_BYTES
  ) {
    throw corruptMarker('has an invalid envelope');
  }
  if (
    view.getUint32(CHECKSUM_OFFSET, true) !==
    crc32([
      bytes.subarray(8, CHECKSUM_OFFSET),
      bytes.subarray(HEADER_BYTES),
    ])
  ) {
    throw corruptMarker('has an invalid checksum');
  }
  if (!matches(bytes, HEADER_BYTES, PAGE_PAYLOAD_MAGIC)) {
    throw corruptMarker('has an invalid payload signature');
  }
  if (
    view.getUint32(HEADER_BYTES + 8, true) !== TOMBSTONE_FORMAT ||
    view.getUint32(HEADER_BYTES + 12, true) !== PAGE_FORMAT ||
    view.getUint32(HEADER_BYTES + 16, true) !== PAGE_SIZE ||
    view.getUint32(HEADER_BYTES + 20, true) !== MAX_PAGES
  ) {
    throw corruptMarker('uses unsupported page storage parameters');
  }

  const databaseRevision = view.getBigUint64(HEADER_BYTES + 24, true);
  if (databaseRevision > MAX_DATABASE_REVISION) {
    throw corruptMarker('contains an unsupported database revision');
  }
  return {
    appliedJournalSequence: view.getBigUint64(24, true),
    databaseRevision,
  };
}

function assertU64(value: bigint, field: string): void {
  if (value < 0n || value > MAX_U64) {
    throw new RangeError(`The page authority ${field} must be an unsigned 64-bit integer`);
  }
}

function assertDatabaseRevision(value: bigint): void {
  if (value < 0n || value > MAX_DATABASE_REVISION) {
    throw new RangeError(
      `The page authority database revision cannot exceed ${MAX_DATABASE_REVISION}`,
    );
  }
}

function corruptMarker(detail: string): StorageError {
  return new StorageError(
    'STORAGE_CORRUPT',
    `The TinyGres page authority marker ${detail}`,
  );
}

function matches(
  bytes: Uint8Array,
  offset: number,
  expected: Uint8Array,
): boolean {
  return (
    bytes.byteLength >= offset + expected.byteLength &&
    expected.every((byte, index) => bytes[offset + index] === byte)
  );
}

function isCommitMagicPrefix(bytes: Uint8Array): boolean {
  if (bytes.byteLength < SNAPSHOT_MAGIC.byteLength) {
    return false;
  }
  let zeroSuffix = false;
  for (let index = 0; index < SNAPSHOT_MAGIC.byteLength; index += 1) {
    const byte = bytes[index]!;
    if (zeroSuffix) {
      if (byte !== 0) {
        return false;
      }
    } else if (byte !== SNAPSHOT_MAGIC[index]) {
      if (byte !== 0) {
        return false;
      }
      zeroSuffix = true;
    }
  }
  return true;
}

const CRC32_TABLE = new Uint32Array(256).map((_, index) => {
  let value = index;
  for (let bit = 0; bit < 8; bit += 1) {
    value = (value & 1) === 1 ? 0xedb8_8320 ^ (value >>> 1) : value >>> 1;
  }
  return value >>> 0;
});

function crc32(parts: readonly Uint8Array[]): number {
  let value = 0xffff_ffff;
  for (const part of parts) {
    for (const byte of part) {
      value = CRC32_TABLE[(value ^ byte) & 0xff]! ^ (value >>> 8);
    }
  }
  return (value ^ 0xffff_ffff) >>> 0;
}
