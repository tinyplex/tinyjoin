import {describe, expect, it} from 'vitest';

import {
  MAX_DATABASE_BYTES,
  MAX_PAGES,
  createMemoryPageDevice,
  createOpfsPageDevice,
  PAGE_SIZE,
  type PageDevice,
  type SyncPageAccessHandle,
} from '../../src/worker/page-device.ts';

class FakeSyncHandle implements SyncPageAccessHandle {
  size = 0;
  maxRead = Number.POSITIVE_INFINITY;
  maxWrite = Number.POSITIVE_INFINITY;
  readCalls = 0;
  writeCalls = 0;
  flushCalls = 0;
  closeCalls = 0;
  failReadCall: number | undefined;
  failWriteCall: number | undefined;
  readError: unknown = new Error('read failed');
  writeError: unknown = new Error('write failed');
  flushError: unknown;
  closeError: unknown;
  sizeError: unknown;
  truncateError: unknown;
  truncateCalls = 0;
  sizeCalls = 0;
  lastReadBuffer: Uint8Array | undefined;
  lastWriteBuffer: Uint8Array | undefined;
  readonly bytes = new Map<number, number>();

  close(): void {
    this.closeCalls += 1;
    if (this.closeError !== undefined) {
      throw this.closeError;
    }
  }

  flush(): void {
    this.flushCalls += 1;
    if (this.flushError !== undefined) {
      throw this.flushError;
    }
  }

  getSize(): number {
    this.sizeCalls += 1;
    if (this.sizeError !== undefined) {
      throw this.sizeError;
    }
    return this.size;
  }

  read(buffer: Uint8Array, options?: {at?: number}): number {
    this.readCalls += 1;
    this.lastReadBuffer = buffer;
    if (this.readCalls === this.failReadCall) {
      throw this.readError;
    }
    const at = options?.at ?? 0;
    const count = Math.max(
      0,
      Math.min(buffer.byteLength, this.maxRead, this.size - at),
    );
    for (let index = 0; index < count; index += 1) {
      buffer[index] = this.bytes.get(at + index) ?? 0;
    }
    return count;
  }

  truncate(newSize: number): void {
    this.truncateCalls += 1;
    if (this.truncateError !== undefined) {
      throw this.truncateError;
    }
    for (const offset of this.bytes.keys()) {
      if (offset >= newSize) {
        this.bytes.delete(offset);
      }
    }
    this.size = newSize;
  }

  write(buffer: Uint8Array, options?: {at?: number}): number {
    this.writeCalls += 1;
    this.lastWriteBuffer = buffer;
    if (this.writeCalls === this.failWriteCall) {
      throw this.writeError;
    }
    const at = options?.at ?? 0;
    const count = Math.min(buffer.byteLength, this.maxWrite);
    for (let index = 0; index < count; index += 1) {
      this.bytes.set(at + index, buffer[index]!);
    }
    this.size = Math.max(this.size, at + count);
    return count;
  }

  byte(at: number): number {
    return this.bytes.get(at) ?? 0;
  }
}

function page(fill: number): Uint8Array {
  return new Uint8Array(PAGE_SIZE).fill(fill);
}

function pages(...fills: number[]): Uint8Array {
  const bytes = new Uint8Array(fills.length * PAGE_SIZE);
  fills.forEach((fill, index) => bytes.fill(fill, index * PAGE_SIZE));
  return bytes;
}

function pageFills(device: PageDevice): number[] {
  const target = page(0);
  return Array.from({length: device.pageCount()}, (_, id) => {
    device.readPage(id, 0, target);
    return target[0]!;
  });
}

describe('page device bounds', () => {
  it('fixes 4 KiB pages and a 256 MiB database ceiling', () => {
    expect(PAGE_SIZE).toBe(4096);
    expect(MAX_PAGES).toBe(65_536);
    expect(MAX_DATABASE_BYTES).toBe(256 * 1024 * 1024);
  });

  it('rejects invalid 64-bit page words before doing I/O', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);
    const target = page(0);

    for (const [low, high] of [
      [-1, 0],
      [0.5, 0],
      [0x1_0000_0000, 0],
      [0, -1],
      [0, 0.5],
      [0, 1],
      [MAX_PAGES, 0],
    ]) {
      expect(() => device.readPage(low!, high!, target)).toThrow(RangeError);
    }
    expect(handle.readCalls).toBe(0);
  });

  it('rejects page gaps and never grows beyond the configured cap', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);

    expect(() => device.writePages(1, 0, page(1))).toThrow(RangeError);
    expect(handle.writeCalls).toBe(0);

    const full = new FakeSyncHandle();
    full.size = MAX_DATABASE_BYTES;
    const fullDevice = createOpfsPageDevice(full);
    expect(fullDevice.pageCount()).toBe(MAX_PAGES);
    expect(fullDevice.writePages(MAX_PAGES - 1, 0, page(2))).toBe(PAGE_SIZE);
    expect(full.size).toBe(MAX_DATABASE_BYTES);
    expect(() => fullDevice.writePages(MAX_PAGES, 0, page(3))).toThrow(
      expect.objectContaining({code: 'STORAGE_DATABASE_TOO_LARGE'}),
    );
    expect(full.size).toBe(MAX_DATABASE_BYTES);

    const memory = createMemoryPageDevice();
    expect(() => memory.writePages(MAX_PAGES, 0, page(3))).toThrow(
      expect.objectContaining({code: 'STORAGE_DATABASE_TOO_LARGE'}),
    );
  });

  it('bounds every page a write names before doing I/O', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);
    const memory = createMemoryPageDevice();
    for (const target of [device, memory]) {
      expect(() => target.writePages(1, 0, pages(1, 2))).toThrow(
        /cannot be written/,
      );
      for (const source of [
        new Uint8Array(0),
        new Uint8Array(PAGE_SIZE + 1),
      ]) {
        expect(() => target.writePages(0, 0, source)).toThrow(
          /whole 4096-byte pages/,
        );
      }
      expect(() => target.writePages(MAX_PAGES - 1, 0, pages(1, 2))).toThrow(
        expect.objectContaining({code: 'STORAGE_DATABASE_TOO_LARGE'}),
      );
      expect(target.pageCount()).toBe(0);
    }
    expect(handle.writeCalls).toBe(0);

    const full = new FakeSyncHandle();
    full.size = MAX_DATABASE_BYTES - PAGE_SIZE;
    const fullDevice = createOpfsPageDevice(full);
    expect(fullDevice.writePages(MAX_PAGES - 2, 0, pages(1, 2))).toBe(
      2 * PAGE_SIZE,
    );
    expect(full.size).toBe(MAX_DATABASE_BYTES);
  });

  it('reads the file length once and follows its own appends', () => {
    const handle = new FakeSyncHandle();
    handle.size = PAGE_SIZE;
    const device = createOpfsPageDevice(handle);
    const opened = handle.sizeCalls;
    expect(device.writePages(1, 0, page(1))).toBe(PAGE_SIZE);
    expect(device.writePages(2, 0, page(2))).toBe(PAGE_SIZE);
    expect(device.writePages(0, 0, page(3))).toBe(PAGE_SIZE);
    device.readPage(2, 0, page(0));
    expect(device.pageCount()).toBe(3);
    expect(handle.size).toBe(3 * PAGE_SIZE);
    expect(() => device.readPage(3, 0, page(0))).toThrow();
    expect(handle.sizeCalls).toBe(opened);
  });

  it('reads exact pages', () => {
    const device = createMemoryPageDevice();
    device.writePages(0, 0, page(0));

    expect(() => device.readPage(0, 0, new Uint8Array(PAGE_SIZE - 1))).toThrow(
      /exactly 4096/,
    );
  });
});

describe('memory page device', () => {
  it('appends dense pages and owns input bytes', () => {
    const device = createMemoryPageDevice();
    const first = page(1);
    expect(device.writePages(0, 0, first)).toBe(PAGE_SIZE);
    first.fill(9);
    expect(device.writePages(1, 0, page(2))).toBe(PAGE_SIZE);
    expect(device.pageCount()).toBe(2);

    const target = page(0);
    expect(device.readPage(0, 0, target)).toBe(PAGE_SIZE);
    expect([...target.subarray(0, 6)]).toEqual([1, 1, 1, 1, 1, 1]);
    target.fill(4);
    const reread = page(0);
    device.readPage(0, 0, reread);
    expect([...reread.subarray(0, 6)]).toEqual([1, 1, 1, 1, 1, 1]);
  });

  it('writes runs of pages in place and past the end, and owns their bytes', () => {
    const device = createMemoryPageDevice();
    const source = pages(1, 2);
    expect(device.writePages(0, 0, source)).toBe(2 * PAGE_SIZE);
    source.fill(9);
    expect(device.writePages(1, 0, pages(3, 4, 5))).toBe(3 * PAGE_SIZE);
    expect(pageFills(device)).toEqual([1, 3, 4, 5]);
  });

  it('rejects unallocated reads, page gaps, and use after close', () => {
    const device = createMemoryPageDevice();
    expect(() => device.readPage(0, 0, page(0))).toThrow(/not been allocated/);
    expect(() => device.writePages(1, 0, page(0))).toThrow(/cannot be written/);
    device.writePages(0, 0, page(1));
    device.flush();
    device.close();
    device.close();
    expect(() => device.pageCount()).toThrow(
      expect.objectContaining({code: 'STORAGE_CLOSED'}),
    );
    expect(() => device.readPage(0, 0, page(0))).toThrow(
      expect.objectContaining({code: 'STORAGE_CLOSED'}),
    );
  });
});

describe('OPFS page device', () => {
  it('appends and overwrites pages through borrowed synchronous views', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);
    const source = page(3);

    expect(device.writePages(0, 0, source)).toBe(PAGE_SIZE);
    expect(handle.lastWriteBuffer?.buffer).toBe(source.buffer);
    source.fill(9);
    expect(device.pageCount()).toBe(1);

    const target = page(0);
    expect(device.readPage(0, 0, target)).toBe(PAGE_SIZE);
    expect(handle.lastReadBuffer?.buffer).toBe(target.buffer);
    expect(target[0]).toBe(3);

  });

  it('writes a run in place and appends the rest, one call each', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);
    const appended = pages(1, 2);
    expect(device.writePages(0, 0, appended)).toBe(2 * PAGE_SIZE);
    expect(handle.writeCalls).toBe(1);
    expect(handle.lastWriteBuffer?.buffer).toBe(appended.buffer);

    const straddling = pages(3, 4, 5);
    expect(device.writePages(1, 0, straddling)).toBe(3 * PAGE_SIZE);
    expect(handle.writeCalls).toBe(3);
    expect(handle.lastWriteBuffer?.buffer).toBe(straddling.buffer);
    expect(device.pageCount()).toBe(4);
    expect(handle.size).toBe(4 * PAGE_SIZE);

    expect(device.writePages(0, 0, pages(6, 7))).toBe(2 * PAGE_SIZE);
    expect(handle.writeCalls).toBe(4);
    expect(pageFills(device)).toEqual([6, 7, 4, 5]);
  });

  it('keeps the in-place part of a run and rolls back its failed append', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);
    device.writePages(0, 0, pages(1, 2));
    handle.failWriteCall = handle.writeCalls + 2;

    expect(() => device.writePages(1, 0, pages(3, 4, 5))).toThrow(
      expect.objectContaining({code: 'STORAGE_WRITE_FAILED', retryable: true}),
    );
    expect(handle.truncateCalls).toBe(1);
    expect(handle.size).toBe(2 * PAGE_SIZE);
    expect(pageFills(device)).toEqual([1, 3]);
  });

  it('completes progressing short reads and writes at exact offsets', () => {
    const handle = new FakeSyncHandle();
    handle.maxWrite = 7;
    handle.maxRead = 11;
    const device = createOpfsPageDevice(handle);
    const source = new Uint8Array(PAGE_SIZE);
    for (let index = 0; index < source.length; index += 1) {
      source[index] = index % 251;
    }

    expect(device.writePages(0, 0, source)).toBe(PAGE_SIZE);
    expect(handle.writeCalls).toBeGreaterThan(1);
    const target = page(0);
    expect(device.readPage(0, 0, target)).toBe(PAGE_SIZE);
    expect(handle.readCalls).toBeGreaterThan(1);
    expect(target).toEqual(source);

  });

  it('turns non-progressing short I/O into stable storage errors', () => {
    const readHandle = new FakeSyncHandle();
    readHandle.size = PAGE_SIZE;
    readHandle.maxRead = 0;
    const readDevice = createOpfsPageDevice(readHandle);
    const target = page(6);
    expect(() => readDevice.readPage(0, 0, target)).toThrow(
      expect.objectContaining({code: 'STORAGE_READ_FAILED', retryable: true}),
    );
    expect(target).toEqual(page(6));

    const writeHandle = new FakeSyncHandle();
    writeHandle.maxWrite = 0;
    const writeDevice = createOpfsPageDevice(writeHandle);
    expect(() => writeDevice.writePages(0, 0, page(1))).toThrow(
      expect.objectContaining({code: 'STORAGE_WRITE_FAILED', retryable: true}),
    );
    expect(writeHandle.size).toBe(0);
    expect(writeHandle.truncateCalls).toBe(1);
    expect(writeHandle.flushCalls).toBe(1);
    expect(writeDevice.pageCount()).toBe(0);
  });

  it('reports a read failure after a direct target was only partly filled', () => {
    const handle = new FakeSyncHandle();
    handle.size = PAGE_SIZE;
    handle.maxRead = 13;
    handle.bytes.set(0, 1);
    handle.failReadCall = 2;
    const target = page(7);

    expect(() => createOpfsPageDevice(handle).readPage(0, 0, target)).toThrow(
      expect.objectContaining({code: 'STORAGE_READ_FAILED'}),
    );
    expect(target[0]).toBe(1);
    expect(target[13]).toBe(7);
  });

  it('marks a partly written existing-page overwrite outcome unknown', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);
    device.writePages(0, 0, page(1));
    handle.maxWrite = 17;
    handle.failWriteCall = handle.writeCalls + 2;

    expect(() => device.writePages(0, 0, page(2))).toThrow(
      expect.objectContaining({
        code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        retryable: false,
      }),
    );
    expect(handle.truncateCalls).toBe(0);
    expect(handle.byte(0)).toBe(2);
    expect(handle.byte(16)).toBe(2);
    expect(handle.byte(17)).toBe(1);
  });

  it('truncates and flushes a partly written append before reporting failure', () => {
    const handle = new FakeSyncHandle();
    const device = createOpfsPageDevice(handle);
    device.writePages(0, 0, page(1));
    handle.maxWrite = 17;
    handle.failWriteCall = handle.writeCalls + 2;

    expect(() => device.writePages(1, 0, page(2))).toThrow(
      expect.objectContaining({code: 'STORAGE_WRITE_FAILED', retryable: true}),
    );
    expect(handle.truncateCalls).toBe(1);
    expect(handle.flushCalls).toBe(1);
    expect(handle.size).toBe(PAGE_SIZE);
    expect(device.pageCount()).toBe(1);
    const target = page(0);
    device.readPage(0, 0, target);
    expect(target).toEqual(page(1));
  });

  it('marks a failed append outcome unknown if rollback cannot complete', () => {
    const handle = new FakeSyncHandle();
    handle.maxWrite = 17;
    handle.failWriteCall = 2;
    handle.truncateError = new Error('truncate failed');
    const device = createOpfsPageDevice(handle);

    expect(() => device.writePages(0, 0, page(2))).toThrow(
      expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
    );
    expect(handle.size).toBe(17);
  });

  it('marks a failed append outcome unknown if rollback cannot flush', () => {
    const handle = new FakeSyncHandle();
    handle.maxWrite = 17;
    handle.failWriteCall = 2;
    handle.flushError = new Error('flush failed');
    const device = createOpfsPageDevice(handle);

    expect(() => device.writePages(0, 0, page(2))).toThrow(
      expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
    );
    expect(handle.truncateCalls).toBe(1);
    expect(handle.flushCalls).toBe(1);
    expect(handle.size).toBe(0);
  });

  it('maps quota failures from writes and flushes', () => {
    const writeHandle = new FakeSyncHandle();
    writeHandle.failWriteCall = 1;
    writeHandle.writeError = new DOMException(
      'out of room',
      'QuotaExceededError',
    );
    expect(() =>
      createOpfsPageDevice(writeHandle).writePages(0, 0, page(1)),
    ).toThrow(
      expect.objectContaining({
        code: 'STORAGE_QUOTA_EXCEEDED',
        retryable: true,
      }),
    );
    expect(writeHandle.truncateCalls).toBe(1);
    expect(writeHandle.flushCalls).toBe(1);

    const partialAppendHandle = new FakeSyncHandle();
    const partialAppendDevice = createOpfsPageDevice(partialAppendHandle);
    partialAppendDevice.writePages(0, 0, page(1));
    partialAppendHandle.maxWrite = 19;
    partialAppendHandle.failWriteCall = partialAppendHandle.writeCalls + 2;
    partialAppendHandle.writeError = new DOMException(
      'out of room',
      'QuotaExceededError',
    );
    expect(() => partialAppendDevice.writePages(1, 0, page(2))).toThrow(
      expect.objectContaining({
        code: 'STORAGE_QUOTA_EXCEEDED',
        retryable: true,
      }),
    );
    expect(partialAppendHandle.size).toBe(PAGE_SIZE);
    expect(partialAppendHandle.truncateCalls).toBe(1);
    expect(partialAppendHandle.flushCalls).toBe(1);

    const overwriteHandle = new FakeSyncHandle();
    const overwriteDevice = createOpfsPageDevice(overwriteHandle);
    overwriteDevice.writePages(0, 0, page(1));
    overwriteHandle.failWriteCall = overwriteHandle.writeCalls + 1;
    overwriteHandle.writeError = new DOMException(
      'out of room',
      'QuotaExceededError',
    );
    expect(() => overwriteDevice.writePages(0, 0, page(2))).toThrow(
      expect.objectContaining({
        code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        retryable: false,
      }),
    );
    expect(overwriteHandle.truncateCalls).toBe(0);

    const flushHandle = new FakeSyncHandle();
    flushHandle.flushError = new DOMException(
      'out of room',
      'QuotaExceededError',
    );
    const device = createOpfsPageDevice(flushHandle);
    expect(() => device.flush()).toThrow(
      expect.objectContaining({code: 'STORAGE_QUOTA_EXCEEDED'}),
    );
  });

  it('repairs a torn trailing page at open and rejects oversized files', () => {
    const torn = new FakeSyncHandle();
    torn.size = PAGE_SIZE + 17;
    for (let offset = PAGE_SIZE; offset < torn.size; offset += 1) {
      torn.bytes.set(offset, 9);
    }
    const repaired = createOpfsPageDevice(torn);
    expect(repaired.pageCount()).toBe(1);
    expect(torn.size).toBe(PAGE_SIZE);
    expect(torn.truncateCalls).toBe(1);
    expect(torn.flushCalls).toBe(1);

    const oversized = new FakeSyncHandle();
    oversized.size = MAX_DATABASE_BYTES + PAGE_SIZE;
    expect(() => createOpfsPageDevice(oversized)).toThrow(
      expect.objectContaining({code: 'STORAGE_DATABASE_TOO_LARGE'}),
    );
    expect(oversized.closeCalls).toBe(1);

    const invalid = new FakeSyncHandle();
    invalid.size = Number.NaN;
    expect(() => createOpfsPageDevice(invalid)).toThrow(
      expect.objectContaining({code: 'STORAGE_CORRUPT'}),
    );
    expect(invalid.closeCalls).toBe(1);
  });

  it('fails closed when a torn-tail repair cannot be made durable', () => {
    const truncateFailure = new FakeSyncHandle();
    truncateFailure.size = PAGE_SIZE + 1;
    truncateFailure.truncateError = new Error('truncate failed');
    expect(() => createOpfsPageDevice(truncateFailure)).toThrow(
      expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
    );
    expect(truncateFailure.closeCalls).toBe(1);

    const flushFailure = new FakeSyncHandle();
    flushFailure.size = PAGE_SIZE + 1;
    flushFailure.flushError = new Error('flush failed');
    expect(() => createOpfsPageDevice(flushFailure)).toThrow(
      expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
    );
    expect(flushFailure.closeCalls).toBe(1);
  });

  it('maps size and close failures, owns handles, and closes only once', () => {
    const sizeHandle = new FakeSyncHandle();
    sizeHandle.sizeError = new Error('broken metadata');
    expect(() => createOpfsPageDevice(sizeHandle)).toThrow(
      expect.objectContaining({code: 'STORAGE_READ_FAILED', retryable: true}),
    );
    expect(sizeHandle.closeCalls).toBe(1);

    const failedOpen = new FakeSyncHandle();
    failedOpen.sizeError = new Error('original size failure');
    failedOpen.closeError = new Error('cleanup close failure');
    expect(() => createOpfsPageDevice(failedOpen)).toThrow(
      expect.objectContaining({
        code: 'STORAGE_READ_FAILED',
        message: expect.stringContaining('original size failure'),
      }),
    );
    expect(failedOpen.closeCalls).toBe(1);

    const closeHandle = new FakeSyncHandle();
    closeHandle.closeError = new Error('close failed');
    const device = createOpfsPageDevice(closeHandle);
    expect(() => device.close()).toThrow(
      expect.objectContaining({code: 'STORAGE_CLOSE_FAILED'}),
    );
    expect(closeHandle.closeCalls).toBe(1);
    device.close();
    expect(closeHandle.closeCalls).toBe(1);
    expect(() => device.pageCount()).toThrow(
      expect.objectContaining({code: 'STORAGE_CLOSED'}),
    );
  });
});
