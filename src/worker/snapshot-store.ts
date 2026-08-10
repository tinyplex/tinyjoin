const RECORD_MAGIC = new Uint8Array([
  0x54, 0x47, 0x52, 0x53, 0x4f, 0x50, 0x46, 0x31,
]);
const RECORD_VERSION = 1;
const RECORD_HEADER_BYTES = 28;
const MAX_GENERATION = 0xffff_ffff_ffff_ffffn;

export const MAX_SNAPSHOT_BYTES = 16 * 1024 * 1024;

export type SnapshotSlot = 0 | 1;

export interface SnapshotCandidate {
  slot: SnapshotSlot;
  generation: bigint;
  snapshot: Uint8Array;
}

export interface SnapshotStore {
  readonly hadData: boolean;
  candidates(): readonly SnapshotCandidate[];
  select(candidate: SnapshotCandidate): void;
  commit(snapshot: Uint8Array): void;
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
      ['snapshot-a.bin', 'snapshot-b.bin'].map((name) =>
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
      return new OpfsSnapshotStore([
        accessHandles[0]!,
        accessHandles[1]!,
      ]);
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
  readonly #candidates: SnapshotCandidate[];
  #activeSlot: SnapshotSlot | undefined;
  #latestGeneration = 0n;
  #closed = false;

  constructor(
    handles: readonly [SyncAccessHandleLike, SyncAccessHandleLike],
  ) {
    this.#handles = handles;
    const sizes = handles.map((handle) => readSize(handle));
    this.hadData = sizes.some((size) => size > 0);
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
    this.#activeSlot = candidate.slot;
    // Import copies the selected state into WASM. Do not retain both complete
    // startup snapshots for the rest of the Worker lifetime.
    this.#candidates.length = 0;
  }

  commit(snapshot: Uint8Array): void {
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
    const record = encodeRecord(generation, snapshot);
    record.fill(0, 0, RECORD_MAGIC.byteLength);
    const handle = this.#handles[targetSlot];
    let markerWritten = false;

    try {
      handle.truncate(0);
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
  }

  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    closeAll(this.#handles);
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
      closeAll(this.#handles);
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

function decodeRecord(
  handle: SyncAccessHandleLike,
  size: number,
  slot: SnapshotSlot,
): SnapshotCandidate | undefined {
  if (size === 0) {
    return undefined;
  }
  if (
    size < RECORD_HEADER_BYTES ||
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
  const generation = view.getBigUint64(12, true);
  const snapshotLength = view.getUint32(20, true);
  if (
    generation === 0n ||
    snapshotLength > MAX_SNAPSHOT_BYTES ||
    snapshotLength !== size - RECORD_HEADER_BYTES
  ) {
    return undefined;
  }
  const expectedChecksum = view.getUint32(24, true);
  if (
    expectedChecksum !==
    crc32([record.subarray(8, 24), record.subarray(RECORD_HEADER_BYTES)])
  ) {
    return undefined;
  }
  if (view.getUint32(8, true) !== RECORD_VERSION) {
    throw new StorageError(
      'STORAGE_VERSION_UNSUPPORTED',
      'The TinyGres OPFS snapshot uses an unsupported storage format',
    );
  }
  return {
    slot,
    generation,
    snapshot: record.slice(RECORD_HEADER_BYTES),
  };
}

function encodeRecord(
  generation: bigint,
  snapshot: Uint8Array,
): Uint8Array {
  const record = new Uint8Array(RECORD_HEADER_BYTES + snapshot.byteLength);
  record.set(RECORD_MAGIC);
  const view = new DataView(record.buffer);
  view.setUint32(8, RECORD_VERSION, true);
  view.setBigUint64(12, generation, true);
  view.setUint32(20, snapshot.byteLength, true);
  record.set(snapshot, RECORD_HEADER_BYTES);
  view.setUint32(
    24,
    crc32([record.subarray(8, 24), record.subarray(RECORD_HEADER_BYTES)]),
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
