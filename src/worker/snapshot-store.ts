import {
  encodeJournalHeader,
  encodeJournalRecord,
  type JournalScan,
  scanJournal,
} from './journal-codec.js';

const RECORD_MAGIC = new Uint8Array([
  0x54, 0x47, 0x52, 0x53, 0x4f, 0x50, 0x46, 0x31,
]);
const RECORD_VERSION = 2;
const RECORD_HEADER_BYTES_V1 = 28;
const RECORD_HEADER_BYTES = 36;
const MAX_GENERATION = 0xffff_ffff_ffff_ffffn;

export const MAX_SNAPSHOT_BYTES = 16 * 1024 * 1024;
const MAX_JOURNAL_BYTES = 32 * 1024 * 1024;

export type SnapshotSlot = 0 | 1;

export interface SnapshotCandidate {
  slot: SnapshotSlot;
  generation: bigint;
  checkpointSequence?: bigint;
  snapshot: Uint8Array;
  journal?: JournalScan;
  journalError?: StorageError;
}

export interface SnapshotStore {
  readonly hadData: boolean;
  candidates(): readonly SnapshotCandidate[];
  select(candidate: SnapshotCandidate): void;
  commit(snapshot: Uint8Array): void;
  append?(payload: Uint8Array): bigint;
  checkpoint?(snapshot: Uint8Array): void;
  journalRecordCount?(): number;
  journalByteLength?(): number;
  close(): void;
}

export type SnapshotStoreFactory = (
  databaseName: string,
) => Promise<SnapshotStore>;

interface SyncAccessHandleLike {
  close(): void;
  flush(): void;
  getSize(): number;
  read(buffer: Uint8Array, options?: {at?: number}): number;
  truncate(newSize: number): void;
  write(buffer: Uint8Array, options?: {at?: number}): number;
}

interface FileHandleLike {
  createSyncAccessHandle?: () => Promise<SyncAccessHandleLike>;
}

interface DirectoryHandleLike {
  getDirectoryHandle(
    name: string,
    options: {create: boolean},
  ): Promise<DirectoryHandleLike>;
  getFileHandle(
    name: string,
    options: {create: boolean},
  ): Promise<FileHandleLike>;
}

interface OpfsProvider {
  getDirectory(): Promise<DirectoryHandleLike>;
}

export class StorageError extends Error {
  readonly code: string;
  readonly retryable: boolean;

  constructor(code: string, message: string, retryable = false) {
    super(message);
    this.name = 'StorageError';
    this.code = code;
    this.retryable = retryable;
  }
}

export async function createOpfsSnapshotStore(
  databaseName: string,
  provider?: OpfsProvider,
): Promise<SnapshotStore> {
  assertDatabaseName(databaseName);
  const selectedProvider = provider ?? defaultOpfsProvider();

  let root: DirectoryHandleLike;
  try {
    root = await selectedProvider.getDirectory();
  } catch (error) {
    throw storageError(
      error,
      'OPFS_UNAVAILABLE',
      'TinyGres could not open the origin private file system',
    );
  }

  try {
    const tinygresDirectory = await root.getDirectoryHandle('tinygres-v1', {
      create: true,
    });
    const databaseDirectory = await tinygresDirectory.getDirectoryHandle(
      `db-${databaseName}`,
      {create: true},
    );
    const fileHandles = await Promise.all(
      [
        'snapshot-a.bin',
        'snapshot-b.bin',
        'journal-a.bin',
        'journal-b.bin',
      ].map((name) =>
        databaseDirectory.getFileHandle(name, {create: true}),
      ),
    );
    if (
      fileHandles.some(
        (file) => typeof file.createSyncAccessHandle !== 'function',
      )
    ) {
      throw new StorageError(
        'OPFS_UNAVAILABLE',
        'TinyGres OPFS storage requires synchronous access handles in a dedicated Worker',
      );
    }

    const accessHandles: SyncAccessHandleLike[] = [];
    try {
      // Opening slot A first also acts as the exclusive database-wide lock.
      for (const file of fileHandles) {
        accessHandles.push(await file.createSyncAccessHandle!());
      }
    } catch (error) {
      closeAll(accessHandles);
      throw storageError(
        error,
        'OPFS_UNAVAILABLE',
        'TinyGres could not acquire synchronous OPFS access',
      );
    }

    try {
      return new OpfsSnapshotStore(
        [accessHandles[0]!, accessHandles[1]!],
        [accessHandles[2]!, accessHandles[3]!],
      );
    } catch (error) {
      closeAll(accessHandles);
      throw error;
    }
  } catch (error) {
    throw storageError(
      error,
      'OPFS_UNAVAILABLE',
      'TinyGres could not initialize its OPFS database directory',
    );
  }
}

export function assertDatabaseName(databaseName: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(databaseName)) {
    throw new StorageError(
      'INVALID_STORAGE_NAME',
      'An OPFS database name must be 1-64 ASCII letters, numbers, dots, underscores, or hyphens, and start with a letter or number',
    );
  }
}

class OpfsSnapshotStore implements SnapshotStore {
  readonly hadData: boolean;
  readonly #handles: readonly [SyncAccessHandleLike, SyncAccessHandleLike];
  readonly #journalHandles: readonly [
    SyncAccessHandleLike,
    SyncAccessHandleLike,
  ];
  readonly #candidates: SnapshotCandidate[];
  #activeSlot: SnapshotSlot | undefined;
  #latestGeneration = 0n;
  #latestSequence = 0n;
  #journalRecords = 0;
  #journalBytes = 0;
  #closed = false;

  constructor(
    handles: readonly [SyncAccessHandleLike, SyncAccessHandleLike],
    journalHandles: readonly [SyncAccessHandleLike, SyncAccessHandleLike],
  ) {
    this.#handles = handles;
    this.#journalHandles = journalHandles;
    const sizes = handles.map((handle) => readSize(handle));
    const journalSizes = journalHandles.map((handle) => readSize(handle));
    this.hadData = [...sizes, ...journalSizes].some((size) => size > 0);
    this.#candidates = handles
      .map((handle, slot) =>
        decodeRecord(handle, sizes[slot]!, slot as SnapshotSlot),
      )
      .filter(
        (candidate): candidate is SnapshotCandidate =>
          candidate !== undefined,
      )
      .sort((left, right) =>
        left.generation === right.generation
          ? right.slot - left.slot
          : left.generation > right.generation
            ? -1
            : 1,
      );
    for (const candidate of this.#candidates) {
      const size = journalSizes[candidate.slot]!;
      if (size === 0) {
        if ((candidate.checkpointSequence ?? 0n) !== 0n) {
          candidate.journalError = new StorageError(
            'STORAGE_JOURNAL_CORRUPT',
            'A TinyGres checkpoint is missing its paired OPFS journal',
          );
        }
        continue;
      }
      if (size > MAX_JOURNAL_BYTES) {
        candidate.journalError = new StorageError(
          'STORAGE_JOURNAL_CORRUPT',
          'The TinyGres OPFS journal exceeds its maximum size',
        );
        continue;
      }
      try {
        const bytes = readAll(
          journalHandles[candidate.slot],
          size,
          'journal',
        );
        const journal = scanJournal(bytes);
        if (journal.baseSequence !== (candidate.checkpointSequence ?? 0n)) {
          throw new StorageError(
            'STORAGE_JOURNAL_CORRUPT',
            'A TinyGres checkpoint does not match its paired OPFS journal',
          );
        }
        if (journal.tail === 'torn') {
          const handle = journalHandles[candidate.slot];
          handle.truncate(journal.validBytes);
          handle.flush();
        }
        candidate.journal = journal;
      } catch (error) {
        candidate.journalError = storageError(
          error,
          'STORAGE_JOURNAL_CORRUPT',
          'TinyGres could not validate its OPFS journal',
        );
      }
    }
    this.#latestGeneration = this.#candidates.reduce(
      (latest, candidate) =>
        candidate.generation > latest ? candidate.generation : latest,
      0n,
    );
  }

  candidates(): readonly SnapshotCandidate[] {
    this.#assertOpen();
    return this.#candidates;
  }

  select(candidate: SnapshotCandidate): void {
    this.#assertOpen();
    if (!this.#candidates.includes(candidate)) {
      throw new StorageError(
        'STORAGE_CORRUPT',
        'TinyGres could not select an unknown OPFS snapshot candidate',
      );
    }
    if (candidate.journalError) {
      throw candidate.journalError;
    }
    this.#activeSlot = candidate.slot;
    if (candidate.journal) {
      this.#latestSequence =
        candidate.journal.records.at(-1)?.sequence ??
        candidate.journal.baseSequence;
      this.#journalRecords = candidate.journal.records.length;
      this.#journalBytes = candidate.journal.validBytes;
    } else {
      this.#latestSequence = 0n;
      this.#journalRecords = 0;
      this.#journalBytes = encodeJournalHeader(0n).byteLength;
      // Migrate an old snapshot by publishing a complete paired slot while
      // leaving the legacy slot untouched as the crash fallback.
      this.checkpoint(candidate.snapshot);
    }
    // Import copies the selected state into WASM. Do not retain both complete
    // startup snapshots for the rest of the Worker lifetime.
    this.#candidates.length = 0;
  }

  commit(snapshot: Uint8Array): void {
    this.checkpoint(snapshot);
  }

  checkpoint(snapshot: Uint8Array): void {
    this.#assertOpen();
    if (snapshot.byteLength > MAX_SNAPSHOT_BYTES) {
      throw new StorageError(
        'STORAGE_SNAPSHOT_TOO_LARGE',
        `TinyGres snapshots cannot exceed ${MAX_SNAPSHOT_BYTES} bytes`,
      );
    }
    if (this.#latestGeneration === MAX_GENERATION) {
      throw new StorageError(
        'STORAGE_GENERATION_OVERFLOW',
        'The TinyGres OPFS snapshot generation overflowed',
      );
    }

    const generation = this.#latestGeneration + 1n;
    const targetSlot: SnapshotSlot =
      this.#activeSlot === undefined || this.#activeSlot === 1 ? 0 : 1;
    const record = encodeRecord(generation, this.#latestSequence, snapshot);
    record.fill(0, 0, RECORD_MAGIC.byteLength);
    const handle = this.#handles[targetSlot];
    const journalHandle = this.#journalHandles[targetSlot];
    let markerWritten = false;

    try {
      // Invalidate the old target snapshot before replacing its paired
      // journal. A crash can therefore select either the untouched active
      // pair or the fully published new pair, never a mismatched pair.
      invalidateRecordOrThrow(handle);
      replaceFile(journalHandle, encodeJournalHeader(this.#latestSequence));
      writeAll(handle, record, 0);
      handle.truncate(record.byteLength);
      if (handle.getSize() !== record.byteLength) {
        throw new StorageError(
          'STORAGE_WRITE_FAILED',
          'TinyGres could not write a complete OPFS snapshot',
          true,
        );
      }
      handle.flush();

      // The independently flushed magic is the record's commit marker. A
      // crash before this point leaves the target slot invalid.
      writeAll(handle, RECORD_MAGIC, 0);
      markerWritten = true;
      handle.flush();
    } catch (error) {
      const invalidated = invalidateRecord(handle);
      if (markerWritten && !invalidated) {
        this.#poison();
        throw new StorageError(
          'STORAGE_COMMIT_OUTCOME_UNKNOWN',
          'TinyGres could not determine whether the final OPFS commit marker was durable',
        );
      }
      throw storageError(
        error,
        'STORAGE_WRITE_FAILED',
        'TinyGres could not durably flush its OPFS snapshot',
      );
    }

    this.#activeSlot = targetSlot;
    this.#latestGeneration = generation;
    this.#journalRecords = 0;
    this.#journalBytes = encodeJournalHeader(0n).byteLength;
  }

  append(payload: Uint8Array): bigint {
    this.#assertOpen();
    if (this.#activeSlot === undefined) {
      throw new StorageError(
        'STORAGE_NOT_INITIALIZED',
        'TinyGres must checkpoint an initial state before appending its journal',
      );
    }
    if (this.#latestSequence === MAX_GENERATION) {
      throw new StorageError(
        'STORAGE_SEQUENCE_OVERFLOW',
        'The TinyGres OPFS journal sequence overflowed',
      );
    }
    const sequence = this.#latestSequence + 1n;
    const record = encodeJournalRecord(sequence, payload);
    const handle = this.#journalHandles[this.#activeSlot];
    const originalSize = readSize(handle);
    const withoutMarker = record.subarray(0, record.byteLength - 4);
    let markerWritten = false;
    try {
      writeAll(handle, withoutMarker, originalSize);
      handle.truncate(originalSize + withoutMarker.byteLength);
      handle.flush();
      writeAll(
        handle,
        record.subarray(record.byteLength - 4),
        originalSize + withoutMarker.byteLength,
      );
      markerWritten = true;
      handle.truncate(originalSize + record.byteLength);
      handle.flush();
    } catch (error) {
      const rolledBack = truncateAndFlush(handle, originalSize);
      if (!rolledBack) {
        this.#poison();
        if (markerWritten) {
          throw new StorageError(
            'STORAGE_COMMIT_OUTCOME_UNKNOWN',
            'TinyGres could not determine whether the final journal commit marker was durable',
          );
        }
        throw new StorageError(
          'STORAGE_WRITE_FAILED',
          'TinyGres could not remove a torn journal append',
          true,
        );
      }
      throw storageError(
        error,
        'STORAGE_WRITE_FAILED',
        'TinyGres could not durably append its OPFS journal',
      );
    }
    this.#latestSequence = sequence;
    this.#journalRecords += 1;
    this.#journalBytes = originalSize + record.byteLength;
    return sequence;
  }

  journalRecordCount(): number {
    this.#assertOpen();
    return this.#journalRecords;
  }

  journalByteLength(): number {
    this.#assertOpen();
    return this.#journalBytes;
  }

  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    closeAll([...this.#handles, ...this.#journalHandles]);
  }

  #assertOpen(): void {
    if (this.#closed) {
      throw new StorageError(
        'STORAGE_CLOSED',
        'The TinyGres OPFS snapshot store is closed',
      );
    }
  }

  #poison(): void {
    this.#closed = true;
    try {
      closeAll([...this.#handles, ...this.#journalHandles]);
    } catch {
      // The outcome is already unknown; still attempt to release both locks.
    }
  }
}

function defaultOpfsProvider(): OpfsProvider {
  const navigatorValue = globalThis.navigator as
    | {storage?: {getDirectory?: () => Promise<unknown>}}
    | undefined;
  const storage = navigatorValue?.storage;
  const getDirectory = storage?.getDirectory;
  if (typeof getDirectory !== 'function') {
    throw new StorageError(
      'OPFS_UNAVAILABLE',
      'TinyGres OPFS storage requires a secure browser context with origin private file system support',
    );
  }
  return {
    async getDirectory() {
      return (await getDirectory.call(storage)) as DirectoryHandleLike;
    },
  };
}

function readSize(handle: SyncAccessHandleLike): number {
  let size: number;
  try {
    size = handle.getSize();
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_READ_FAILED',
      'TinyGres could not inspect an OPFS snapshot',
    );
  }
  if (!Number.isSafeInteger(size) || size < 0) {
    throw new StorageError(
      'STORAGE_CORRUPT',
      'TinyGres found an invalid OPFS snapshot size',
    );
  }
  return size;
}

function readAll(
  handle: SyncAccessHandleLike,
  size: number,
  description: string,
): Uint8Array {
  const bytes = new Uint8Array(size);
  let offset = 0;
  try {
    while (offset < size) {
      const read = handle.read(bytes.subarray(offset), {at: offset});
      if (!Number.isSafeInteger(read) || read <= 0 || read > size - offset) {
        throw new StorageError(
          'STORAGE_READ_FAILED',
          `TinyGres encountered a short OPFS ${description} read`,
        );
      }
      offset += read;
    }
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_READ_FAILED',
      `TinyGres could not read its OPFS ${description}`,
    );
  }
  return bytes;
}

function decodeRecord(
  handle: SyncAccessHandleLike,
  size: number,
  slot: SnapshotSlot,
): SnapshotCandidate | undefined {
  if (size === 0) {
    return undefined;
  }
  if (
    size < RECORD_HEADER_BYTES_V1 ||
    size > RECORD_HEADER_BYTES + MAX_SNAPSHOT_BYTES
  ) {
    return undefined;
  }

  const record = new Uint8Array(size);
  let offset = 0;
  try {
    while (offset < size) {
      const read = handle.read(record.subarray(offset), {at: offset});
      if (
        !Number.isSafeInteger(read) ||
        read <= 0 ||
        read > size - offset
      ) {
        return undefined;
      }
      offset += read;
    }
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_READ_FAILED',
      'TinyGres could not read an OPFS snapshot',
    );
  }

  if (!RECORD_MAGIC.every((byte, index) => record[index] === byte)) {
    return undefined;
  }
  const view = new DataView(record.buffer, record.byteOffset, record.byteLength);
  const version = view.getUint32(8, true);
  const headerBytes =
    version === 1
      ? RECORD_HEADER_BYTES_V1
      : version === RECORD_VERSION
        ? RECORD_HEADER_BYTES
        : undefined;
  if (headerBytes === undefined) {
    const recognizedEnvelope = [RECORD_HEADER_BYTES, RECORD_HEADER_BYTES_V1].some(
      (candidateHeaderBytes) =>
        size >= candidateHeaderBytes &&
        view.getUint32(candidateHeaderBytes - 4, true) ===
          crc32([
            record.subarray(8, candidateHeaderBytes - 4),
            record.subarray(candidateHeaderBytes),
          ]),
    );
    if (recognizedEnvelope) {
      throw new StorageError(
        'STORAGE_VERSION_UNSUPPORTED',
        'The TinyGres OPFS snapshot uses an unsupported storage format',
      );
    }
    return undefined;
  }
  const generation = view.getBigUint64(12, true);
  const snapshotLength = view.getUint32(20, true);
  if (
    generation === 0n ||
    snapshotLength > MAX_SNAPSHOT_BYTES ||
    snapshotLength !== size - headerBytes
  ) {
    return undefined;
  }
  const checksumOffset = version === 1 ? 24 : 32;
  if (
    view.getUint32(checksumOffset, true) !==
    crc32([
      record.subarray(8, checksumOffset),
      record.subarray(headerBytes),
    ])
  ) {
    return undefined;
  }
  return {
    slot,
    generation,
    checkpointSequence: version === 1 ? 0n : view.getBigUint64(24, true),
    snapshot: record.slice(headerBytes),
  };
}

function encodeRecord(
  generation: bigint,
  checkpointSequence: bigint,
  snapshot: Uint8Array,
): Uint8Array {
  const record = new Uint8Array(RECORD_HEADER_BYTES + snapshot.byteLength);
  record.set(RECORD_MAGIC);
  const view = new DataView(record.buffer);
  view.setUint32(8, RECORD_VERSION, true);
  view.setBigUint64(12, generation, true);
  view.setUint32(20, snapshot.byteLength, true);
  view.setBigUint64(24, checkpointSequence, true);
  record.set(snapshot, RECORD_HEADER_BYTES);
  view.setUint32(
    32,
    crc32([record.subarray(8, 32), record.subarray(RECORD_HEADER_BYTES)]),
    true,
  );
  return record;
}

function writeAll(
  handle: SyncAccessHandleLike,
  bytes: Uint8Array,
  start: number,
): void {
  let offset = 0;
  while (offset < bytes.byteLength) {
    const written = handle.write(bytes.subarray(offset), {at: start + offset});
    if (
      !Number.isSafeInteger(written) ||
      written <= 0 ||
      written > bytes.byteLength - offset
    ) {
      throw new StorageError(
        'STORAGE_WRITE_FAILED',
        'TinyGres encountered a short OPFS write',
        true,
      );
    }
    offset += written;
  }
}

function replaceFile(handle: SyncAccessHandleLike, bytes: Uint8Array): void {
  handle.truncate(0);
  writeAll(handle, bytes, 0);
  handle.truncate(bytes.byteLength);
  if (handle.getSize() !== bytes.byteLength) {
    throw new StorageError(
      'STORAGE_WRITE_FAILED',
      'TinyGres could not write a complete OPFS file',
      true,
    );
  }
  handle.flush();
}

function invalidateRecordOrThrow(handle: SyncAccessHandleLike): void {
  handle.truncate(0);
  handle.flush();
}

function truncateAndFlush(
  handle: SyncAccessHandleLike,
  size: number,
): boolean {
  try {
    handle.truncate(size);
    handle.flush();
    return true;
  } catch {
    return false;
  }
}

function invalidateRecord(handle: SyncAccessHandleLike): boolean {
  try {
    handle.truncate(0);
    handle.flush();
    return true;
  } catch {
    return false;
  }
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

function closeAll(handles: readonly SyncAccessHandleLike[]): void {
  let firstError: unknown;
  for (const handle of handles) {
    try {
      handle.close();
    } catch (error) {
      firstError ??= error;
    }
  }
  if (firstError !== undefined) {
    throw firstError;
  }
}

function storageError(
  error: unknown,
  fallbackCode: string,
  fallbackMessage: string,
): StorageError {
  if (error instanceof StorageError) {
    return error;
  }
  if (
    typeof error === 'object' &&
    error !== null &&
    'code' in error &&
    typeof error.code === 'string' &&
    'message' in error &&
    typeof error.message === 'string'
  ) {
    return new StorageError(error.code, error.message);
  }
  const name =
    typeof error === 'object' && error !== null && 'name' in error
      ? String(error.name)
      : '';
  const message =
    error instanceof Error && error.message
      ? `${fallbackMessage}: ${error.message}`
      : fallbackMessage;
  if (name === 'NoModificationAllowedError') {
    return new StorageError(
      'STORAGE_LOCKED',
      'Another TinyGres worker already has this OPFS database open',
      true,
    );
  }
  if (name === 'QuotaExceededError') {
    return new StorageError(
      'STORAGE_QUOTA_EXCEEDED',
      'The browser has no space available for the TinyGres OPFS snapshot',
      true,
    );
  }
  if (
    name === 'InvalidStateError' ||
    name === 'NotAllowedError' ||
    name === 'SecurityError'
  ) {
    return new StorageError('OPFS_UNAVAILABLE', message);
  }
  return new StorageError(fallbackCode, message, true);
}
