import {describe, expect, it} from 'vitest';

import {
  type OpfsPageStorageDirectoryHandle,
  type OpfsPageStorageProvider,
  FIRST_DATA_PAGE_ID,
  MIN_PAGE_DATABASE_BYTES,
  PAGE_DATABASE_FILE_NAME,
  createOpfsPageStorageSession,
} from '../../src/worker/page-storage.ts';
import {
  classifyPageAuthorityRecord,
  encodePageAuthorityMarker,
  stagePageAuthorityMarker,
} from '../../src/worker/page-authority.ts';
import {
  MAX_DATABASE_BYTES,
  PAGE_SIZE,
  type SyncPageAccessHandle,
} from '../../src/worker/page-device.ts';
import {StorageError} from '../../src/worker/storage-error.ts';

const marker = {
  appliedJournalSequence: 41n,
  databaseRevision: 17n,
};

class MemoryFile {
  bytes = new Uint8Array();
  reportedSize: number | undefined;
  locked = false;
  openCalls = 0;
  closeCalls = 0;
  flushCalls = 0;
  readCalls = 0;
  readonly readByteLengths: number[] = [];
  writeCalls = 0;
  truncateCalls = 0;
  maxRead = Number.POSITIVE_INFINITY;
  maxWrite = Number.POSITIVE_INFINITY;
  closeError: unknown;
  sizeError: unknown;
  truncateError: unknown;
  readonly failFlushCalls = new Map<number, unknown>();
  readonly failReadCalls = new Map<number, unknown>();
  readonly failWriteCalls = new Map<number, unknown>();
  readonly events: string[];
  readonly name: string;

  constructor(name: string, events: string[]) {
    this.name = name;
    this.events = events;
  }

  async createSyncAccessHandle(): Promise<MemoryAccessHandle> {
    this.events.push(`open:${this.name}`);
    this.openCalls += 1;
    if (this.locked) {
      throw new DOMException('already open', 'NoModificationAllowedError');
    }
    this.locked = true;
    return new MemoryAccessHandle(this);
  }
}

class MemoryAccessHandle implements SyncPageAccessHandle {
  readonly #file: MemoryFile;
  #closed = false;

  constructor(file: MemoryFile) {
    this.#file = file;
  }

  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#file.closeCalls += 1;
    this.#file.events.push(`close:${this.#file.name}`);
    this.#file.locked = false;
    if (this.#file.closeError !== undefined) {
      throw this.#file.closeError;
    }
  }

  flush(): void {
    this.#assertOpen();
    this.#file.flushCalls += 1;
    this.#file.events.push(
      `flush:${this.#file.name}:${this.#file.flushCalls}`,
    );
    const error = this.#file.failFlushCalls.get(this.#file.flushCalls);
    if (error !== undefined) {
      throw error;
    }
  }

  getSize(): number {
    this.#assertOpen();
    if (this.#file.sizeError !== undefined) {
      throw this.#file.sizeError;
    }
    return this.#file.reportedSize ?? this.#file.bytes.byteLength;
  }

  read(buffer: Uint8Array, options?: {at?: number}): number {
    this.#assertOpen();
    this.#file.readCalls += 1;
    this.#file.readByteLengths.push(buffer.byteLength);
    const error = this.#file.failReadCalls.get(this.#file.readCalls);
    if (error !== undefined) {
      throw error;
    }
    const at = options?.at ?? 0;
    const available = Math.max(0, this.#file.bytes.byteLength - at);
    const count = Math.min(buffer.byteLength, available, this.#file.maxRead);
    buffer.set(this.#file.bytes.subarray(at, at + count));
    return count;
  }

  truncate(newSize: number): void {
    this.#assertOpen();
    this.#file.truncateCalls += 1;
    this.#file.events.push(`truncate:${this.#file.name}:${newSize}`);
    if (this.#file.truncateError !== undefined) {
      throw this.#file.truncateError;
    }
    const next = new Uint8Array(newSize);
    next.set(this.#file.bytes.subarray(0, newSize));
    this.#file.bytes = next;
    this.#file.reportedSize = undefined;
  }

  write(buffer: Uint8Array, options?: {at?: number}): number {
    this.#assertOpen();
    this.#file.writeCalls += 1;
    this.#file.events.push(
      `write:${this.#file.name}:${this.#file.writeCalls}`,
    );
    const error = this.#file.failWriteCalls.get(this.#file.writeCalls);
    if (error !== undefined) {
      throw error;
    }
    const at = options?.at ?? 0;
    const count = Math.min(buffer.byteLength, this.#file.maxWrite);
    const required = Math.max(this.#file.bytes.byteLength, at + count);
    if (required !== this.#file.bytes.byteLength) {
      const next = new Uint8Array(required);
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

class MemoryDirectory implements OpfsPageStorageDirectoryHandle {
  readonly directories = new Map<string, MemoryDirectory>();
  readonly files = new Map<string, MemoryFile>();
  readonly events: string[];

  constructor(events: string[]) {
    this.events = events;
  }

  async getDirectoryHandle(
    name: string,
    options: {create: boolean},
  ): Promise<MemoryDirectory> {
    this.events.push(`directory:${name}:${options.create}`);
    let directory = this.directories.get(name);
    if (directory === undefined) {
      if (!options.create) {
        throw new DOMException('missing directory', 'NotFoundError');
      }
      directory = new MemoryDirectory(this.events);
      this.directories.set(name, directory);
    }
    return directory;
  }

  async getFileHandle(
    name: string,
    options: {create: boolean},
  ): Promise<MemoryFile> {
    this.events.push(`file:${name}:${options.create}`);
    let file = this.files.get(name);
    if (file === undefined) {
      if (!options.create) {
        throw new DOMException('missing file', 'NotFoundError');
      }
      file = new MemoryFile(name, this.events);
      this.files.set(name, file);
    }
    return file;
  }
}

function memoryOpfs() {
  const events: string[] = [];
  const root = new MemoryDirectory(events);
  const provider: OpfsPageStorageProvider = {
    getDirectory: async () => {
      events.push('root');
      return root;
    },
  };
  return {
    events,
    provider,
    async database(name: string): Promise<MemoryDirectory> {
      const tinygres = await root.getDirectoryHandle('tinygres-v1', {
        create: true,
      });
      return tinygres.getDirectoryHandle(`db-${name}`, {create: true});
    },
  };
}

async function seedFile(
  opfs: ReturnType<typeof memoryOpfs>,
  databaseName: string,
  fileName: string,
  bytes: Uint8Array,
): Promise<MemoryFile> {
  const database = await opfs.database(databaseName);
  const file = await database.getFileHandle(fileName, {create: true});
  file.bytes = bytes.slice();
  return file;
}

async function seedCommitted(
  opfs: ReturnType<typeof memoryOpfs>,
  databaseName: string,
  pageBytes = new Uint8Array(MIN_PAGE_DATABASE_BYTES),
): Promise<{authority: MemoryFile; page: MemoryFile}> {
  const authority = await seedFile(
    opfs,
    databaseName,
    'snapshot-a.bin',
    encodePageAuthorityMarker(marker),
  );
  const page = await seedFile(
    opfs,
    databaseName,
    PAGE_DATABASE_FILE_NAME,
    pageBytes,
  );
  return {authority, page};
}

async function stagedSession(
  opfs: ReturnType<typeof memoryOpfs>,
  databaseName: string,
) {
  const session = await createOpfsPageStorageSession(
    databaseName,
    opfs.provider,
  );
  const device = await session.createStagedPageDevice();
  const page = new Uint8Array(PAGE_SIZE);
  for (let pageId = 0; pageId < FIRST_DATA_PAGE_ID; pageId += 1) {
    device.writePage(pageId, 0, page);
  }
  device.flush();
  const database = await opfs.database(databaseName);
  return {
    session,
    device,
    authority: await database.getFileHandle('snapshot-a.bin', {create: false}),
    page: await database.getFileHandle(PAGE_DATABASE_FILE_NAME, {create: false}),
  };
}

describe('OPFS page storage authority', () => {
  it('acquires slot A first and holds it across new/new worker contention', async () => {
    const opfs = memoryOpfs();
    const first = await createOpfsPageStorageSession('locking', opfs.provider);

    expect(opfs.events.indexOf('open:snapshot-a.bin')).toBeGreaterThan(-1);
    expect(opfs.events).not.toContain(`open:${PAGE_DATABASE_FILE_NAME}`);
    await expect(
      createOpfsPageStorageSession('locking', opfs.provider),
    ).rejects.toMatchObject({code: 'STORAGE_LOCKED', retryable: true});

    first.close();
    const second = await createOpfsPageStorageSession(
      'locking',
      opfs.provider,
    );
    second.close();
  });

  it('classifies legacy and staged authority without creating a page file', async () => {
    const opfs = memoryOpfs();
    const legacyFile = await seedFile(
      opfs,
      'legacy',
      'snapshot-a.bin',
      new Uint8Array(16 * 1024 * 1024).fill(7),
    );
    await seedFile(
      opfs,
      'staged',
      'snapshot-a.bin',
      stagePageAuthorityMarker(marker),
    );

    const legacy = await createOpfsPageStorageSession(
      'legacy',
      opfs.provider,
    );
    expect(legacy.authority).toEqual({kind: 'legacy'});
    expect(legacy.pageDevice).toBeUndefined();
    expect(legacyFile.readCalls).toBe(1);
    expect(legacyFile.readByteLengths).toEqual([69]);
    const legacyDatabase = await opfs.database('legacy');
    expect(legacyDatabase.files.has(PAGE_DATABASE_FILE_NAME)).toBe(false);
    legacy.close();

    const staged = await createOpfsPageStorageSession(
      'staged',
      opfs.provider,
    );
    expect(staged.authority).toEqual({kind: 'staged'});
    expect(staged.pageDevice).toBeUndefined();
    const stagedDatabase = await opfs.database('staged');
    expect(stagedDatabase.files.has(PAGE_DATABASE_FILE_NAME)).toBe(false);
    staged.close();
  });

  it('preserves authority read errors and closes A after classification fails', async () => {
    const opfs = memoryOpfs();
    const authority = await seedFile(
      opfs,
      'read-failure',
      'snapshot-a.bin',
      encodePageAuthorityMarker(marker),
    );
    authority.failReadCalls.set(1, new StorageError(
      'STORAGE_READ_FAILED',
      'exact authority read failure',
      false,
    ));

    await expect(
      createOpfsPageStorageSession('read-failure', opfs.provider),
    ).rejects.toMatchObject({
      code: 'STORAGE_READ_FAILED',
      message: 'exact authority read failure',
      retryable: false,
    });
    expect(authority.closeCalls).toBe(1);
  });

  it('opens committed page authority only after locking A and never creates a missing page', async () => {
    const opfs = memoryOpfs();
    await seedCommitted(opfs, 'committed');
    const session = await createOpfsPageStorageSession(
      'committed',
      opfs.provider,
    );

    expect(session.authority).toEqual({kind: 'page', marker});
    expect(session.pageDevice?.pageCount()).toBe(FIRST_DATA_PAGE_ID);
    expect(opfs.events.indexOf('open:snapshot-a.bin')).toBeLessThan(
      opfs.events.indexOf(`open:${PAGE_DATABASE_FILE_NAME}`),
    );
    session.close();

    const missing = memoryOpfs();
    const authority = await seedFile(
      missing,
      'missing-page',
      'snapshot-a.bin',
      encodePageAuthorityMarker(marker),
    );
    const database = await missing.database('missing-page');
    await expect(
      createOpfsPageStorageSession('missing-page', missing.provider),
    ).rejects.toMatchObject({code: 'STORAGE_CORRUPT', retryable: false});
    expect(database.files.has(PAGE_DATABASE_FILE_NAME)).toBe(false);
    expect(
      missing.events.filter(
        (event) => event === `file:${PAGE_DATABASE_FILE_NAME}:false`,
      ),
    ).toHaveLength(1);
    expect(authority.closeCalls).toBe(1);
  });

  it.each([
    ['short', MIN_PAGE_DATABASE_BYTES - PAGE_SIZE, 'STORAGE_CORRUPT'],
    ['unaligned', MIN_PAGE_DATABASE_BYTES + 1, 'STORAGE_CORRUPT'],
    [
      'oversized',
      MAX_DATABASE_BYTES + PAGE_SIZE,
      'STORAGE_DATABASE_TOO_LARGE',
    ],
  ])(
    'fails closed for a %s committed page file without repairing it',
    async (suffix, reportedSize, code) => {
      const opfs = memoryOpfs();
      const {authority, page} = await seedCommitted(
        opfs,
        `invalid-${suffix}`,
      );
      page.reportedSize = reportedSize;
      const originalBytes = page.bytes.slice();

      await expect(
        createOpfsPageStorageSession(`invalid-${suffix}`, opfs.provider),
      ).rejects.toMatchObject({code});
      expect(page.truncateCalls).toBe(0);
      expect(page.flushCalls).toBe(0);
      expect(page.bytes).toEqual(originalBytes);
      expect(page.closeCalls).toBe(1);
      expect(authority.closeCalls).toBe(1);
      expect(opfs.events.indexOf(`close:${PAGE_DATABASE_FILE_NAME}`)).toBeLessThan(
        opfs.events.indexOf('close:snapshot-a.bin'),
      );
    },
  );

  it('creates and resets a staged page file without changing legacy authority', async () => {
    const opfs = memoryOpfs();
    const existing = await seedFile(
      opfs,
      'reset',
      PAGE_DATABASE_FILE_NAME,
      new Uint8Array(PAGE_SIZE * 3).fill(7),
    );
    const session = await createOpfsPageStorageSession(
      'reset',
      opfs.provider,
    );
    const device = await session.createStagedPageDevice();

    expect(existing.bytes).toHaveLength(0);
    expect(existing.truncateCalls).toBe(1);
    expect(existing.flushCalls).toBe(1);
    expect(device.pageCount()).toBe(0);
    expect(session.authority).toEqual({kind: 'legacy'});
    expect(() => session.publishPageAuthority(marker)).toThrow(
      expect.objectContaining({code: 'STORAGE_CORRUPT'}),
    );
    session.close();
  });

  it('closes page then A exactly once and preserves the first close error', async () => {
    const opfs = memoryOpfs();
    const {authority, page} = await seedCommitted(opfs, 'close-order');
    page.closeError = new Error('page close failed first');
    authority.closeError = new Error('A close failed second');
    const session = await createOpfsPageStorageSession(
      'close-order',
      opfs.provider,
    );

    expect(() => session.pageDevice?.close()).toThrow(
      expect.objectContaining({message: expect.stringContaining('page close failed first')}),
    );
    session.close();
    session.pageDevice?.close();
    expect(page.closeCalls).toBe(1);
    expect(authority.closeCalls).toBe(1);
    expect(opfs.events.indexOf(`close:${PAGE_DATABASE_FILE_NAME}`)).toBeLessThan(
      opfs.events.indexOf('close:snapshot-a.bin'),
    );
  });

  it('publishes exact staged and committed records across every flush cut', async () => {
    for (const [failedFlush, expectedKind, expectedCode] of [
      [1, 'legacy', 'STORAGE_QUOTA_EXCEEDED'],
      [2, 'staged', 'STORAGE_QUOTA_EXCEEDED'],
      [3, 'page', 'STORAGE_COMMIT_OUTCOME_UNKNOWN'],
    ] as const) {
      const opfs = memoryOpfs();
      const {session, authority} = await stagedSession(
        opfs,
        `flush-cut-${failedFlush}`,
      );
      authority.failFlushCalls.set(
        failedFlush,
        new DOMException('no room', 'QuotaExceededError'),
      );

      expect(() => session.publishPageAuthority(marker)).toThrow(
        expect.objectContaining({
          code: expectedCode,
          retryable: failedFlush < 3,
        }),
      );
      if (failedFlush < 3) {
        session.close();
      }
      expect(classifyPageAuthorityRecord(authority.bytes).kind).toBe(
        expectedKind,
      );
    }

    const opfs = memoryOpfs();
    const {session, authority} = await stagedSession(opfs, 'publish-success');
    session.publishPageAuthority(marker);
    expect(authority.bytes).toEqual(encodePageAuthorityMarker(marker));
    expect(session.authority).toEqual({kind: 'page', marker});
    session.close();

    const reopened = await createOpfsPageStorageSession(
      'publish-success',
      opfs.provider,
    );
    expect(reopened.authority).toEqual({kind: 'page', marker});
    expect(reopened.pageDevice?.pageCount()).toBe(FIRST_DATA_PAGE_ID);
    reopened.close();
  });

  it('keeps pre-magic failures retryable with legacy fallback intact', async () => {
    const opfs = memoryOpfs();
    const {session, authority} = await stagedSession(
      opfs,
      'pre-magic-retry',
    );
    authority.maxWrite = 0;

    expect(() => session.publishPageAuthority(marker)).toThrow(
      expect.objectContaining({code: 'STORAGE_WRITE_FAILED', retryable: true}),
    );
    expect(session.authority).toEqual({kind: 'legacy'});
    expect(classifyPageAuthorityRecord(authority.bytes)).toEqual({
      kind: 'legacy',
    });
    authority.maxWrite = Number.POSITIVE_INFINITY;
    session.publishPageAuthority(marker);
    expect(session.authority).toEqual({kind: 'page', marker});
    session.close();
  });

  it('flushes staged pages before touching authority and permits a safe retry', async () => {
    const opfs = memoryOpfs();
    const {session, authority, page} = await stagedSession(
      opfs,
      'page-flush-before-authority',
    );
    const originalAuthority = authority.bytes.slice();
    page.failFlushCalls.set(
      3,
      new DOMException('page quota exhausted', 'QuotaExceededError'),
    );

    expect(() => session.publishPageAuthority(marker)).toThrow(
      expect.objectContaining({
        code: 'STORAGE_QUOTA_EXCEEDED',
        retryable: true,
      }),
    );
    expect(authority.bytes).toEqual(originalAuthority);
    expect(authority.truncateCalls).toBe(0);
    expect(authority.writeCalls).toBe(0);
    expect(authority.flushCalls).toBe(0);
    expect(session.authority).toEqual({kind: 'legacy'});

    page.failFlushCalls.delete(3);
    session.publishPageAuthority(marker);
    expect(session.authority).toEqual({kind: 'page', marker});
    session.close();
  });

  it('poisons and closes after any attempted final magic failure', async () => {
    const opfs = memoryOpfs();
    const {session, authority, page} = await stagedSession(
      opfs,
      'magic-failure',
    );
    authority.failWriteCalls.set(2, new Error('magic write failed'));

    expect(() => session.publishPageAuthority(marker)).toThrow(
      expect.objectContaining({
        code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        retryable: false,
      }),
    );
    expect(classifyPageAuthorityRecord(authority.bytes)).toEqual({
      kind: 'staged',
    });
    expect(page.closeCalls).toBe(1);
    expect(authority.closeCalls).toBe(1);
    expect(() => session.publishPageAuthority(marker)).toThrow(
      expect.objectContaining({code: 'STORAGE_CLOSED'}),
    );
  });

  it('treats a partial final magic write as an unknown closed outcome', async () => {
    const opfs = memoryOpfs();
    const {session, authority, page} = await stagedSession(
      opfs,
      'partial-magic-failure',
    );
    authority.maxWrite = 3;
    authority.failWriteCalls.set(25, new Error('magic continuation failed'));

    expect(() => session.publishPageAuthority(marker)).toThrow(
      expect.objectContaining({
        code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        retryable: false,
      }),
    );
    expect(classifyPageAuthorityRecord(authority.bytes)).toEqual({
      kind: 'staged',
    });
    expect(authority.bytes.subarray(0, 3)).toEqual(
      new Uint8Array([0x54, 0x47, 0x52]),
    );
    expect(page.closeCalls).toBe(1);
    expect(authority.closeCalls).toBe(1);
  });

  it('canonicalizes published authority before the durable final write', async () => {
    const opfs = memoryOpfs();
    const {session, device} = await stagedSession(opfs, 'canonical-authority');
    let journalReads = 0;
    let revisionReads = 0;
    const accessorMarker = {
      get appliedJournalSequence() {
        journalReads += 1;
        if (journalReads > 1) {
          throw new Error('journal sequence was read after publication');
        }
        device.writePage(
          FIRST_DATA_PAGE_ID,
          0,
          new Uint8Array(PAGE_SIZE).fill(7),
        );
        return marker.appliedJournalSequence;
      },
      get databaseRevision() {
        revisionReads += 1;
        if (revisionReads > 1) {
          throw new Error('database revision was read after publication');
        }
        return marker.databaseRevision;
      },
    };

    session.publishPageAuthority(accessorMarker);
    expect(journalReads).toBe(1);
    expect(revisionReads).toBe(1);
    expect(session.authority).toEqual({kind: 'page', marker});
    const pageWrite = opfs.events.lastIndexOf(
      `write:${PAGE_DATABASE_FILE_NAME}:9`,
    );
    const pageFlush = opfs.events.lastIndexOf(
      `flush:${PAGE_DATABASE_FILE_NAME}:3`,
    );
    const authorityTruncate = opfs.events.lastIndexOf(
      'truncate:snapshot-a.bin:0',
    );
    expect(pageWrite).toBeLessThan(pageFlush);
    expect(pageFlush).toBeLessThan(authorityTruncate);
    expect(() => {
      (session.authority as {kind: string}).kind = 'legacy';
    }).toThrow(TypeError);
    expect(() => session.createStagedPageDevice()).rejects.toMatchObject({
      code: 'STORAGE_AUTHORITY_ALREADY_PUBLISHED',
    });
    session.close();
  });

  it('rechecks session state after marker accessors before touching authority', async () => {
    const opfs = memoryOpfs();
    const {session, authority} = await stagedSession(
      opfs,
      'marker-reentry-close',
    );
    const originalAuthority = authority.bytes.slice();
    const closingMarker = {
      get appliedJournalSequence() {
        session.close();
        return marker.appliedJournalSequence;
      },
      databaseRevision: marker.databaseRevision,
    };

    expect(() => session.publishPageAuthority(closingMarker)).toThrow(
      expect.objectContaining({code: 'STORAGE_CLOSED'}),
    );
    expect(authority.bytes).toEqual(originalAuthority);
    expect(authority.truncateCalls).toBe(0);
    expect(authority.writeCalls).toBe(0);
  });

  it('never resets or republishes an existing page authority', async () => {
    const opfs = memoryOpfs();
    const {authority, page} = await seedCommitted(opfs, 'no-republish');
    const originalAuthority = authority.bytes.slice();
    const originalPage = page.bytes.slice();
    const session = await createOpfsPageStorageSession(
      'no-republish',
      opfs.provider,
    );

    await expect(session.createStagedPageDevice()).rejects.toMatchObject({
      code: 'STORAGE_AUTHORITY_ALREADY_PUBLISHED',
    });
    expect(() => session.publishPageAuthority(marker)).toThrow(
      expect.objectContaining({code: 'STORAGE_AUTHORITY_ALREADY_PUBLISHED'}),
    );
    expect(authority.bytes).toEqual(originalAuthority);
    expect(page.bytes).toEqual(originalPage);
    expect(page.truncateCalls).toBe(0);
    session.close();
  });
});
