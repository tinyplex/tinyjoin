import {describe, expect, it} from 'vitest';

import {
  StorageError,
  createOpfsSnapshotStore,
} from '../../src/worker/snapshot-store.ts';

class MemoryFile {
  bytes = new Uint8Array();
  locked = false;
  maxWrite = Number.POSITIVE_INFINITY;
  flushFailures = 0;
  flushCalls = 0;
  readonly failFlushCalls = new Set<number>();

  async createSyncAccessHandle(): Promise<MemoryAccessHandle> {
    if (this.locked) {
      throw new DOMException('already open', 'NoModificationAllowedError');
    }
    this.locked = true;
    return new MemoryAccessHandle(this);
  }
}

class MemoryAccessHandle {
  readonly #file: MemoryFile;
  #closed = false;

  constructor(file: MemoryFile) {
    this.#file = file;
  }

  close(): void {
    if (!this.#closed) {
      this.#closed = true;
      this.#file.locked = false;
    }
  }

  flush(): void {
    this.#assertOpen();
    this.#file.flushCalls += 1;
    if (
      this.#file.failFlushCalls.has(this.#file.flushCalls) ||
      this.#file.flushFailures > 0
    ) {
      if (this.#file.flushFailures > 0) {
        this.#file.flushFailures -= 1;
      }
      throw new DOMException('quota exhausted', 'QuotaExceededError');
    }
  }

  getSize(): number {
    this.#assertOpen();
    return this.#file.bytes.byteLength;
  }

  read(buffer: Uint8Array, options?: {at?: number}): number {
    this.#assertOpen();
    const at = options?.at ?? 0;
    const count = Math.min(buffer.byteLength, this.#file.bytes.byteLength - at);
    if (count <= 0) {
      return 0;
    }
    buffer.set(this.#file.bytes.subarray(at, at + count));
    return count;
  }

  truncate(newSize: number): void {
    this.#assertOpen();
    const next = new Uint8Array(newSize);
    next.set(this.#file.bytes.subarray(0, newSize));
    this.#file.bytes = next;
  }

  write(buffer: Uint8Array, options?: {at?: number}): number {
    this.#assertOpen();
    const at = options?.at ?? 0;
    const count = Math.min(buffer.byteLength, this.#file.maxWrite);
    const requiredSize = Math.max(this.#file.bytes.byteLength, at + count);
    if (requiredSize !== this.#file.bytes.byteLength) {
      const next = new Uint8Array(requiredSize);
      next.set(this.#file.bytes);
      this.#file.bytes = next;
    }
    this.#file.bytes.set(buffer.subarray(0, count), at);
    return count;
  }

  #assertOpen(): void {
    if (this.#closed) {
      throw new DOMException('closed', 'InvalidStateError');
    }
  }
}

class MemoryDirectory {
  readonly directories = new Map<string, MemoryDirectory>();
  readonly files = new Map<string, MemoryFile>();

  async getDirectoryHandle(
    name: string,
    _options: {create: boolean},
  ): Promise<MemoryDirectory> {
    let directory = this.directories.get(name);
    if (!directory) {
      directory = new MemoryDirectory();
      this.directories.set(name, directory);
    }
    return directory;
  }

  async getFileHandle(
    name: string,
    _options: {create: boolean},
  ): Promise<MemoryFile> {
    let file = this.files.get(name);
    if (!file) {
      file = new MemoryFile();
      this.files.set(name, file);
    }
    return file;
  }
}

function memoryOpfs() {
  const root = new MemoryDirectory();
  return {
    root,
    provider: {getDirectory: async () => root},
    async database(name: string): Promise<MemoryDirectory> {
      const tinygres = await root.getDirectoryHandle('tinygres-v1', {
        create: true,
      });
      return tinygres.getDirectoryHandle(`db-${name}`, {create: true});
    },
  };
}

describe('OPFS snapshot store', () => {
  it('orders complete A/B snapshots by an independent generation', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('restart-test', opfs.provider);
    store.commit(new Uint8Array([1]));
    store.commit(new Uint8Array([2]));
    store.close();

    const reopened = await createOpfsSnapshotStore(
      'restart-test',
      opfs.provider,
    );
    expect(
      reopened.candidates().map(({generation, snapshot}) => ({
        generation,
        snapshot: [...snapshot],
      })),
    ).toEqual([
      {generation: 2n, snapshot: [2]},
      {generation: 1n, snapshot: [1]},
    ]);
    reopened.close();
  });

  it('loops over short writes and flushes an exact record', async () => {
    const opfs = memoryOpfs();
    const database = await opfs.database('short-writes');
    const firstSlot = await database.getFileHandle('snapshot-a.bin', {
      create: true,
    });
    firstSlot.maxWrite = 3;

    const store = await createOpfsSnapshotStore(
      'short-writes',
      opfs.provider,
    );
    store.commit(new Uint8Array([1, 2, 3, 4, 5]));
    store.close();

    const reopened = await createOpfsSnapshotStore(
      'short-writes',
      opfs.provider,
    );
    expect([...reopened.candidates()[0]!.snapshot]).toEqual([1, 2, 3, 4, 5]);
    reopened.close();
  });

  it('rejects a zero-length write without publishing the target slot', async () => {
    const opfs = memoryOpfs();
    const database = await opfs.database('zero-write');
    const firstSlot = await database.getFileHandle('snapshot-a.bin', {
      create: true,
    });
    firstSlot.maxWrite = 0;
    const store = await createOpfsSnapshotStore('zero-write', opfs.provider);

    expect(() => store.commit(new Uint8Array([1]))).toThrowError(
      expect.objectContaining({code: 'STORAGE_WRITE_FAILED'}),
    );
    expect(firstSlot.bytes).toHaveLength(0);
    store.close();
  });

  it('keeps the previous generation after a flush failure', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('flush-failure', opfs.provider);
    store.commit(new Uint8Array([1]));
    const database = await opfs.database('flush-failure');
    const secondSlot = await database.getFileHandle('snapshot-b.bin', {
      create: true,
    });
    secondSlot.flushFailures = 1;

    expect(() => store.commit(new Uint8Array([2]))).toThrowError(
      expect.objectContaining({code: 'STORAGE_QUOTA_EXCEEDED'}),
    );
    store.close();

    const reopened = await createOpfsSnapshotStore(
      'flush-failure',
      opfs.provider,
    );
    expect(reopened.candidates()).toHaveLength(1);
    expect([...reopened.candidates()[0]!.snapshot]).toEqual([1]);
    reopened.close();
  });

  it('invalidates a commit whose final marker flush fails', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore(
      'marker-flush-failure',
      opfs.provider,
    );
    store.commit(new Uint8Array([1]));
    const database = await opfs.database('marker-flush-failure');
    const secondSlot = await database.getFileHandle('snapshot-b.bin', {
      create: true,
    });
    secondSlot.failFlushCalls.add(3);

    expect(() => store.commit(new Uint8Array([2]))).toThrowError(
      expect.objectContaining({code: 'STORAGE_QUOTA_EXCEEDED'}),
    );
    expect(secondSlot.bytes).toHaveLength(0);
    store.close();

    const reopened = await createOpfsSnapshotStore(
      'marker-flush-failure',
      opfs.provider,
    );
    expect(reopened.candidates().map(({snapshot}) => [...snapshot])).toEqual([
      [1],
    ]);
    reopened.close();
  });

  it('poisons the store when final-marker cleanup cannot be flushed', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore(
      'unknown-commit',
      opfs.provider,
    );
    store.commit(new Uint8Array([1]));
    const database = await opfs.database('unknown-commit');
    const secondSlot = await database.getFileHandle('snapshot-b.bin', {
      create: true,
    });
    secondSlot.failFlushCalls.add(3);
    secondSlot.failFlushCalls.add(4);

    expect(() => store.commit(new Uint8Array([2]))).toThrowError(
      expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
    );
    expect(() => store.commit(new Uint8Array([3]))).toThrowError(
      expect.objectContaining({code: 'STORAGE_CLOSED'}),
    );

    const reopened = await createOpfsSnapshotStore(
      'unknown-commit',
      opfs.provider,
    );
    expect(reopened.candidates().map(({snapshot}) => [...snapshot])).toEqual([
      [1],
    ]);
    reopened.close();
  });

  it('falls back from a checksum-invalid newer slot', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('torn-write', opfs.provider);
    store.commit(new Uint8Array([1]));
    store.commit(new Uint8Array([2]));
    store.close();
    const database = await opfs.database('torn-write');
    const newest = await database.getFileHandle('snapshot-b.bin', {
      create: true,
    });
    const lastByte = newest.bytes.length - 1;
    newest.bytes[lastByte] = (newest.bytes[lastByte] ?? 0) ^ 0xff;

    const reopened = await createOpfsSnapshotStore(
      'torn-write',
      opfs.provider,
    );
    expect(reopened.hadData).toBe(true);
    expect(reopened.candidates()).toHaveLength(1);
    expect([...reopened.candidates()[0]!.snapshot]).toEqual([1]);
    reopened.close();
  });

  it('falls back for every truncated prefix of the newer record', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore(
      'truncation-prefixes',
      opfs.provider,
    );
    store.commit(new Uint8Array([1]));
    store.commit(new Uint8Array([2, 3, 4, 5]));
    store.close();
    const database = await opfs.database('truncation-prefixes');
    const newest = await database.getFileHandle('snapshot-b.bin', {
      create: true,
    });
    const complete = newest.bytes.slice();

    for (let length = 0; length < complete.byteLength; length += 1) {
      newest.bytes = complete.slice(0, length);
      const reopened = await createOpfsSnapshotStore(
        'truncation-prefixes',
        opfs.provider,
      );
      expect(
        reopened.candidates().map(({snapshot}) => [...snapshot]),
        `prefix length ${length}`,
      ).toEqual([[1]]);
      reopened.close();
    }
  });

  it('fails fast on an unsupported wrapper version', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('future-format', opfs.provider);
    store.commit(new Uint8Array([1]));
    store.close();
    const database = await opfs.database('future-format');
    const firstSlot = await database.getFileHandle('snapshot-a.bin', {
      create: true,
    });
    rewriteRecordVersion(firstSlot, 99);

    await expect(
      createOpfsSnapshotStore('future-format', opfs.provider),
    ).rejects.toMatchObject({code: 'STORAGE_VERSION_UNSUPPORTED'});
  });

  it('treats a version-bit corruption as a torn newer record', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore(
      'corrupt-version-byte',
      opfs.provider,
    );
    store.commit(new Uint8Array([1]));
    store.commit(new Uint8Array([2]));
    store.close();
    const database = await opfs.database('corrupt-version-byte');
    const newest = await database.getFileHandle('snapshot-b.bin', {
      create: true,
    });
    newest.bytes[8] = (newest.bytes[8] ?? 0) ^ 0x01;

    const reopened = await createOpfsSnapshotStore(
      'corrupt-version-byte',
      opfs.provider,
    );
    expect(reopened.candidates().map(({snapshot}) => [...snapshot])).toEqual([
      [1],
    ]);
    reopened.close();
  });

  it('migrates a legacy snapshot into a paired journal checkpoint', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('legacy-pair', opfs.provider);
    store.commit(new Uint8Array([1, 2, 3]));
    store.close();
    const database = await opfs.database('legacy-pair');
    const firstSlot = await database.getFileHandle('snapshot-a.bin', {
      create: true,
    });
    firstSlot.bytes = new Uint8Array(
      encodeLegacySnapshot(1n, new Uint8Array([1, 2, 3])),
    );
    const firstJournal = await database.getFileHandle('journal-a.bin', {
      create: true,
    });
    firstJournal.bytes = new Uint8Array();

    const reopened = await createOpfsSnapshotStore('legacy-pair', opfs.provider);
    const legacy = reopened.candidates()[0]!;
    expect(legacy.checkpointSequence).toBe(0n);
    reopened.select(legacy);
    reopened.close();

    const migrated = await createOpfsSnapshotStore('legacy-pair', opfs.provider);
    expect(migrated.candidates()[0]).toMatchObject({
      checkpointSequence: 0n,
      snapshot: new Uint8Array([1, 2, 3]),
    });
    expect(migrated.candidates()[0]!.journal?.baseSequence).toBe(0n);
    migrated.close();
  });

  it('locks one database name while allowing independent names', async () => {
    const opfs = memoryOpfs();
    const first = await createOpfsSnapshotStore('locked', opfs.provider);

    await expect(
      createOpfsSnapshotStore('locked', opfs.provider),
    ).rejects.toMatchObject({code: 'STORAGE_LOCKED'});
    const independent = await createOpfsSnapshotStore(
      'independent',
      opfs.provider,
    );
    independent.close();

    first.close();
    const reopened = await createOpfsSnapshotStore('locked', opfs.provider);
    reopened.close();
  });

  it('rejects unsafe or ambiguous database names', async () => {
    const opfs = memoryOpfs();
    for (const name of ['', '../escape', 'white space', 'x'.repeat(65)]) {
      await expect(
        createOpfsSnapshotStore(name, opfs.provider),
      ).rejects.toBeInstanceOf(StorageError);
    }
  });

  it('appends committed journal records and resumes their sequence', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('journal-restart', opfs.provider);
    store.checkpoint!(new Uint8Array([0]));
    expect(store.append!(new Uint8Array([10]))).toBe(1n);
    expect(store.append!(new Uint8Array([20]))).toBe(2n);
    store.close();

    const reopened = await createOpfsSnapshotStore(
      'journal-restart',
      opfs.provider,
    );
    const candidate = reopened.candidates()[0]!;
    expect(candidate.journal?.baseSequence).toBe(0n);
    expect(
      candidate.journal?.records.map(({sequence, payload}) => ({
        sequence,
        payload: [...payload],
      })),
    ).toEqual([
      {sequence: 1n, payload: [10]},
      {sequence: 2n, payload: [20]},
    ]);
    reopened.select(candidate);
    expect(reopened.append!(new Uint8Array([30]))).toBe(3n);
    reopened.close();
  });

  it('compacts into an independently committed checkpoint and journal pair', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('journal-compact', opfs.provider);
    store.checkpoint!(new Uint8Array([0]));
    store.append!(new Uint8Array([10]));
    store.checkpoint!(new Uint8Array([1]));
    store.close();

    const reopened = await createOpfsSnapshotStore(
      'journal-compact',
      opfs.provider,
    );
    expect(
      reopened.candidates().map((candidate) => ({
        snapshot: [...candidate.snapshot],
        base: candidate.journal?.baseSequence,
        records: candidate.journal?.records.length,
      })),
    ).toEqual([
      {snapshot: [1], base: 1n, records: 0},
      {snapshot: [0], base: 0n, records: 1},
    ]);
    reopened.close();
  });

  it('rejects a journal paired with the wrong checkpoint sequence', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('journal-mismatch', opfs.provider);
    store.checkpoint!(new Uint8Array([0]));
    store.append!(new Uint8Array([1]));
    store.checkpoint!(new Uint8Array([1]));
    store.close();
    const database = await opfs.database('journal-mismatch');
    const oldJournal = await database.getFileHandle('journal-a.bin', {
      create: true,
    });
    const newJournal = await database.getFileHandle('journal-b.bin', {
      create: true,
    });
    newJournal.bytes = oldJournal.bytes.slice();

    const reopened = await createOpfsSnapshotStore(
      'journal-mismatch',
      opfs.provider,
    );
    expect(() => reopened.select(reopened.candidates()[0]!)).toThrowError(
      expect.objectContaining({code: 'STORAGE_JOURNAL_CORRUPT'}),
    );
    reopened.close();
  });

  it('repairs an incomplete final journal record without losing its checkpoint', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('journal-torn', opfs.provider);
    store.checkpoint!(new Uint8Array([1]));
    store.append!(new Uint8Array([2, 3, 4]));
    store.close();
    const database = await opfs.database('journal-torn');
    const journal = await database.getFileHandle('journal-a.bin', {create: true});
    journal.bytes = journal.bytes.slice(0, journal.bytes.length - 2);

    const reopened = await createOpfsSnapshotStore(
      'journal-torn',
      opfs.provider,
    );
    const candidate = reopened.candidates()[0]!;
    expect([...candidate.snapshot]).toEqual([1]);
    expect(candidate.journal?.records).toEqual([]);
    expect(journal.bytes).toHaveLength(candidate.journal?.validBytes ?? -1);
    reopened.close();
  });

  it('leaves the legacy snapshot intact when migration cannot flush', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore('legacy-failure', opfs.provider);
    store.commit(new Uint8Array([7]));
    store.close();
    const database = await opfs.database('legacy-failure');
    const legacy = await database.getFileHandle('snapshot-a.bin', {create: true});
    legacy.bytes = new Uint8Array(encodeLegacySnapshot(1n, new Uint8Array([7])));
    const oldJournal = await database.getFileHandle('journal-a.bin', {
      create: true,
    });
    oldJournal.bytes = new Uint8Array();
    const newJournal = await database.getFileHandle('journal-b.bin', {
      create: true,
    });
    newJournal.flushFailures = 1;

    const reopening = await createOpfsSnapshotStore(
      'legacy-failure',
      opfs.provider,
    );
    expect(() => reopening.select(reopening.candidates()[0]!)).toThrowError(
      expect.objectContaining({code: 'STORAGE_QUOTA_EXCEEDED'}),
    );
    reopening.close();

    const recovered = await createOpfsSnapshotStore(
      'legacy-failure',
      opfs.provider,
    );
    expect([...recovered.candidates()[0]!.snapshot]).toEqual([7]);
    recovered.close();
  });

  it('rolls back a journal whose final marker cannot flush', async () => {
    const opfs = memoryOpfs();
    const store = await createOpfsSnapshotStore(
      'journal-marker-failure',
      opfs.provider,
    );
    store.checkpoint!(new Uint8Array([1]));
    const database = await opfs.database('journal-marker-failure');
    const journal = await database.getFileHandle('journal-a.bin', {create: true});
    journal.failFlushCalls.add(3);

    expect(() => store.append!(new Uint8Array([2]))).toThrowError(
      expect.objectContaining({code: 'STORAGE_QUOTA_EXCEEDED'}),
    );
    store.close();

    const reopened = await createOpfsSnapshotStore(
      'journal-marker-failure',
      opfs.provider,
    );
    expect(reopened.candidates()[0]!.journal?.records).toEqual([]);
    reopened.close();
  });
});

function rewriteRecordVersion(file: MemoryFile, version: number): void {
  const view = new DataView(
    file.bytes.buffer,
    file.bytes.byteOffset,
    file.bytes.byteLength,
  );
  view.setUint32(8, version, true);
  view.setUint32(
    32,
    testCrc32([file.bytes.subarray(8, 32), file.bytes.subarray(36)]),
    true,
  );
}

function encodeLegacySnapshot(
  generation: bigint,
  snapshot: Uint8Array,
): Uint8Array {
  const bytes = new Uint8Array(28 + snapshot.byteLength);
  bytes.set([0x54, 0x47, 0x52, 0x53, 0x4f, 0x50, 0x46, 0x31]);
  const view = new DataView(bytes.buffer);
  view.setUint32(8, 1, true);
  view.setBigUint64(12, generation, true);
  view.setUint32(20, snapshot.byteLength, true);
  bytes.set(snapshot, 28);
  view.setUint32(
    24,
    testCrc32([bytes.subarray(8, 24), bytes.subarray(28)]),
    true,
  );
  return bytes;
}

function testCrc32(parts: readonly Uint8Array[]): number {
  let value = 0xffff_ffff;
  for (const part of parts) {
    for (const byte of part) {
      value ^= byte;
      for (let bit = 0; bit < 8; bit += 1) {
        value =
          (value & 1) === 1
            ? 0xedb8_8320 ^ (value >>> 1)
            : value >>> 1;
      }
    }
  }
  return (value ^ 0xffff_ffff) >>> 0;
}
