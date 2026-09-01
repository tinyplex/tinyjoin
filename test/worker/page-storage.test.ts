import {describe, expect, it} from 'vitest';

import {
  createOpfsPageStorageSession,
  PAGE_DATABASE_FILE_NAME,
  type OpfsPageStorageDirectoryHandle,
  type OpfsPageStorageProvider,
} from '../../src/worker/page-storage.ts';
import {
  MAX_DATABASE_BYTES,
  PAGE_SIZE,
  type SyncPageAccessHandle,
} from '../../src/worker/page-device.ts';

class MemoryFile {
  bytes = new Uint8Array();
  reportedSize: number | undefined;
  locked = false;
  openCalls = 0;
  closeCalls = 0;
  flushCalls = 0;
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
    this.#file.locked = false;
    this.#file.events.push(`close:${this.#file.name}`);
  }

  flush(): void {
    this.#assertOpen();
    this.#file.flushCalls += 1;
  }

  getSize(): number {
    this.#assertOpen();
    return this.#file.reportedSize ?? this.#file.bytes.byteLength;
  }

  read(buffer: Uint8Array, options?: {at?: number}): number {
    this.#assertOpen();
    const at = options?.at ?? 0;
    const count = Math.min(
      buffer.byteLength,
      Math.max(0, this.#file.bytes.byteLength - at),
    );
    buffer.set(this.#file.bytes.subarray(at, at + count));
    return count;
  }

  truncate(newSize: number): void {
    this.#assertOpen();
    const bytes = new Uint8Array(newSize);
    bytes.set(this.#file.bytes.subarray(0, newSize));
    this.#file.bytes = bytes;
    this.#file.reportedSize = undefined;
  }

  write(buffer: Uint8Array, options?: {at?: number}): number {
    this.#assertOpen();
    const at = options?.at ?? 0;
    const required = Math.max(this.#file.bytes.byteLength, at + buffer.byteLength);
    if (required !== this.#file.bytes.byteLength) {
      const bytes = new Uint8Array(required);
      bytes.set(this.#file.bytes);
      this.#file.bytes = bytes;
    }
    this.#file.bytes.set(buffer, at);
    return buffer.byteLength;
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
        throw new DOMException('missing', 'NotFoundError');
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
        throw new DOMException('missing', 'NotFoundError');
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
    getDirectory: async () => root,
  };
  const database = async (name: string): Promise<MemoryDirectory> => {
    const tinyjoin = await root.getDirectoryHandle('tinyjoin-pages-v1', {
      create: true,
    });
    return tinyjoin.getDirectoryHandle(`db-${name}`, {create: true});
  };
  return {database, events, provider, root};
}

describe('page-only OPFS storage', () => {
  it('opens exactly one page file whose access handle is the database lock', async () => {
    const opfs = memoryOpfs();
    const first = await createOpfsPageStorageSession('ships', opfs.provider);

    expect(opfs.events).toEqual([
      'directory:tinyjoin-pages-v1:true',
      'directory:db-ships:true',
      `file:${PAGE_DATABASE_FILE_NAME}:true`,
      `open:${PAGE_DATABASE_FILE_NAME}`,
    ]);
    const directory = await opfs.database('ships');
    expect([...directory.files]).toHaveLength(1);
    const file = directory.files.get(PAGE_DATABASE_FILE_NAME)!;
    expect(file.locked).toBe(true);
    await expect(
      createOpfsPageStorageSession('ships', opfs.provider),
    ).rejects.toMatchObject({code: 'STORAGE_LOCKED', retryable: true});

    first.close();
    first.close();
    expect(file.closeCalls).toBe(1);
    const reopened = await createOpfsPageStorageSession('ships', opfs.provider);
    reopened.close();
    expect(file.closeCalls).toBe(2);
  });

  it('allows different database names to hold independent locks', async () => {
    const opfs = memoryOpfs();
    const first = await createOpfsPageStorageSession('first', opfs.provider);
    const second = await createOpfsPageStorageSession('second', opfs.provider);
    first.close();
    second.close();
    expect((await opfs.database('first')).files).toHaveProperty('size', 1);
    expect((await opfs.database('second')).files).toHaveProperty('size', 1);
  });

  it('repairs a torn trailing page while opening the sole file', async () => {
    const opfs = memoryOpfs();
    const directory = await opfs.database('torn');
    const file = await directory.getFileHandle(PAGE_DATABASE_FILE_NAME, {
      create: true,
    });
    file.bytes = new Uint8Array(PAGE_SIZE + 17);

    const session = await createOpfsPageStorageSession('torn', opfs.provider);
    expect(session.pageDevice.pageCount()).toBe(1);
    expect(file.bytes).toHaveLength(PAGE_SIZE);
    expect(file.flushCalls).toBe(1);
    session.close();
  });

  it('closes the lock when page validation rejects an oversized file', async () => {
    const opfs = memoryOpfs();
    const directory = await opfs.database('oversized');
    const file = await directory.getFileHandle(PAGE_DATABASE_FILE_NAME, {
      create: true,
    });
    file.reportedSize = MAX_DATABASE_BYTES + PAGE_SIZE;

    await expect(
      createOpfsPageStorageSession('oversized', opfs.provider),
    ).rejects.toMatchObject({code: 'STORAGE_DATABASE_TOO_LARGE'});
    expect(file.locked).toBe(false);
    expect(file.closeCalls).toBe(1);
  });

  it('requires dedicated-Worker synchronous access handles', async () => {
    const directory = new MemoryDirectory([]);
    directory.files.set(
      PAGE_DATABASE_FILE_NAME,
      {} as MemoryFile,
    );
    const databaseDirectory: OpfsPageStorageDirectoryHandle = {
      getDirectoryHandle: async () => databaseDirectory,
      getFileHandle: async () => ({}),
    };
    const provider: OpfsPageStorageProvider = {
      getDirectory: async () => databaseDirectory,
    };
    await expect(
      createOpfsPageStorageSession('window-thread', provider),
    ).rejects.toMatchObject({code: 'OPFS_UNAVAILABLE'});
  });

  it('validates the database name before touching OPFS', async () => {
    let calls = 0;
    const provider: OpfsPageStorageProvider = {
      getDirectory: async () => {
        calls += 1;
        return new MemoryDirectory([]);
      },
    };
    await expect(
      createOpfsPageStorageSession('../escape', provider),
    ).rejects.toMatchObject({code: 'INVALID_STORAGE_NAME'});
    expect(calls).toBe(0);
  });
});
