import {StorageError} from './snapshot-store.js';

export const PAGE_SIZE = 4096;
export const MAX_PAGES = 65_536;
export const MAX_DATABASE_BYTES = PAGE_SIZE * MAX_PAGES;

export interface PageDevice {
  pageCount(): number;
  /**
   * Reads directly into `target`. If the read fails, the target's contents are
   * undefined: a synchronous access handle may already have filled a prefix.
   */
  readPage(
    pageIdLow: number,
    pageIdHigh: number,
    target: Uint8Array,
  ): number;
  /**
   * Appends a page or replaces one in place. An existing page may only be an
   * inactive copy-on-write page; never overwrite metadata or data reachable
   * from the active superblock.
   */
  writePage(
    pageIdLow: number,
    pageIdHigh: number,
    source: Uint8Array,
  ): number;
  flush(): void;
  close(): void;
}

/**
 * The synchronous subset used by the worker page device. Implementations must
 * consume read/write views before returning and must not retain them.
 */
export interface SyncPageAccessHandle {
  close(): void;
  flush(): void;
  getSize(): number;
  read(buffer: Uint8Array, options?: {at?: number}): number;
  truncate(newSize: number): void;
  write(buffer: Uint8Array, options?: {at?: number}): number;
}

/**
 * A bounded random-access page device over an OPFS synchronous access handle.
 *
 * Full-page writes may replace an existing inactive copy-on-write page or
 * append exactly one page. In-place writes must never target metadata or data
 * reachable from the active superblock. This keeps the file dense and page
 * aligned, and prevents a bad page id from allocating a large sparse file.
 */
export class OpfsPageDevice implements PageDevice {
  readonly #handle: SyncPageAccessHandle;
  #closed = false;

  constructor(handle: SyncPageAccessHandle) {
    this.#handle = handle;
    try {
      this.#repairTornTail();
    } catch (error) {
      // Construction takes ownership of the handle immediately. If validation
      // or repair fails, close that handle without allowing a cleanup failure
      // to replace the error that explains why the database could not open.
      this.#closed = true;
      try {
        handle.close();
      } catch {
        // The original open error is more actionable and must be preserved.
      }
      throw error;
    }
  }

  pageCount(): number {
    this.#assertOpen();
    return this.#readPageCount();
  }

  readPage(
    pageIdLow: number,
    pageIdHigh: number,
    target: Uint8Array,
  ): number {
    this.#assertOpen();
    const pageId = pageIdFromWords(pageIdLow, pageIdHigh);
    assertExactPage(target, 'The read target');
    if (pageId >= this.#readPageCount()) {
      throw new RangeError(`Page ${pageId} has not been allocated`);
    }

    readExactly(this.#handle, pageOffset(pageId), target);
    return PAGE_SIZE;
  }

  writePage(
    pageIdLow: number,
    pageIdHigh: number,
    source: Uint8Array,
  ): number {
    this.#assertOpen();
    const pageId = pageIdForWriteFromWords(pageIdLow, pageIdHigh);
    assertExactPage(source, 'The page source');
    const count = this.#readPageCount();
    if (pageId > count) {
      throw new RangeError(
        `Page ${pageId} cannot be written before page ${count} is allocated`,
      );
    }

    try {
      if (pageId === count) {
        writeExactly(this.#handle, pageOffset(pageId), source);
      } else {
        writeExistingExactly(this.#handle, pageOffset(pageId), source);
      }
    } catch (error) {
      if (pageId === count) {
        this.#rollbackAppend(count);
      }
      throw error;
    }
    return PAGE_SIZE;
  }

  flush(): void {
    this.#assertOpen();
    try {
      this.#handle.flush();
    } catch (error) {
      throw pageStorageError(
        error,
        'STORAGE_WRITE_FAILED',
        'TinyGres could not flush database pages',
      );
    }
  }

  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    try {
      this.#handle.close();
    } catch (error) {
      throw pageStorageError(
        error,
        'STORAGE_CLOSE_FAILED',
        'TinyGres could not close its page device',
      );
    }
  }

  #assertOpen(): void {
    if (this.#closed) {
      throw new StorageError(
        'STORAGE_CLOSED',
        'The TinyGres page device is closed',
      );
    }
  }

  #readPageCount(): number {
    let size: number;
    try {
      size = this.#handle.getSize();
    } catch (error) {
      throw pageStorageError(
        error,
        'STORAGE_READ_FAILED',
        'TinyGres could not read the database page count',
      );
    }
    if (!Number.isSafeInteger(size) || size < 0) {
      throw new StorageError(
        'STORAGE_CORRUPT',
        'The TinyGres page file has an invalid byte length',
      );
    }
    if (size > MAX_DATABASE_BYTES) {
      throw new StorageError(
        'STORAGE_DATABASE_TOO_LARGE',
        `The TinyGres page file exceeds ${MAX_DATABASE_BYTES} bytes`,
      );
    }
    if (size % PAGE_SIZE !== 0) {
      throw new StorageError(
        'STORAGE_CORRUPT',
        'The TinyGres page file is not aligned to its page size',
      );
    }
    return size / PAGE_SIZE;
  }

  #repairTornTail(): void {
    let size: number;
    try {
      size = this.#handle.getSize();
    } catch (error) {
      throw pageStorageError(
        error,
        'STORAGE_READ_FAILED',
        'TinyGres could not read the database page count',
      );
    }
    if (!Number.isSafeInteger(size) || size < 0) {
      throw new StorageError(
        'STORAGE_CORRUPT',
        'The TinyGres page file has an invalid byte length',
      );
    }
    if (size > MAX_DATABASE_BYTES) {
      throw new StorageError(
        'STORAGE_DATABASE_TOO_LARGE',
        `The TinyGres page file exceeds ${MAX_DATABASE_BYTES} bytes`,
      );
    }
    const alignedSize = size - (size % PAGE_SIZE);
    if (alignedSize === size) {
      return;
    }
    try {
      this.#handle.truncate(alignedSize);
      this.#handle.flush();
    } catch (error) {
      const mapped = pageStorageError(
        error,
        'STORAGE_WRITE_FAILED',
        'TinyGres could not repair a torn trailing database page',
      );
      throw new StorageError(
        'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        mapped.message,
      );
    }
  }

  #rollbackAppend(originalPageCount: number): void {
    try {
      this.#handle.truncate(originalPageCount * PAGE_SIZE);
      this.#handle.flush();
    } catch (rollbackError) {
      const detail =
        rollbackError instanceof Error && rollbackError.message
          ? `: ${rollbackError.message}`
          : '';
      throw new StorageError(
        'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        `TinyGres could not roll back a failed page append${detail}`,
      );
    }
  }
}

/** A dependency-free page device for tests and non-persistent runtimes. */
export class MemoryPageDevice implements PageDevice {
  readonly #pages: Uint8Array[] = [];
  #closed = false;

  pageCount(): number {
    this.#assertOpen();
    return this.#pages.length;
  }

  readPage(
    pageIdLow: number,
    pageIdHigh: number,
    target: Uint8Array,
  ): number {
    this.#assertOpen();
    const pageId = pageIdFromWords(pageIdLow, pageIdHigh);
    assertExactPage(target, 'The read target');
    const page = this.#pages[pageId];
    if (page === undefined) {
      throw new RangeError(`Page ${pageId} has not been allocated`);
    }
    target.set(page);
    return PAGE_SIZE;
  }

  writePage(
    pageIdLow: number,
    pageIdHigh: number,
    source: Uint8Array,
  ): number {
    this.#assertOpen();
    const pageId = pageIdForWriteFromWords(pageIdLow, pageIdHigh);
    assertExactPage(source, 'The page source');
    if (pageId > this.#pages.length) {
      throw new RangeError(
        `Page ${pageId} cannot be written before page ${this.#pages.length} is allocated`,
      );
    }
    this.#pages[pageId] = source.slice();
    return PAGE_SIZE;
  }

  flush(): void {
    this.#assertOpen();
  }

  close(): void {
    this.#closed = true;
    this.#pages.length = 0;
  }

  #assertOpen(): void {
    if (this.#closed) {
      throw new StorageError(
        'STORAGE_CLOSED',
        'The TinyGres page device is closed',
      );
    }
  }
}

function pageIdFromWords(low: number, high: number): number {
  if (!isUint32(low) || !isUint32(high)) {
    throw new RangeError('Page id words must be unsigned 32-bit integers');
  }
  if (high !== 0 || low >= MAX_PAGES) {
    throw new RangeError(`Page id must be between 0 and ${MAX_PAGES - 1}`);
  }
  return low;
}

function pageIdForWriteFromWords(low: number, high: number): number {
  if (!isUint32(low) || !isUint32(high)) {
    throw new RangeError('Page id words must be unsigned 32-bit integers');
  }
  if (high !== 0 || low > MAX_PAGES) {
    throw new RangeError(`Page id must be between 0 and ${MAX_PAGES}`);
  }
  if (low === MAX_PAGES) {
    throw new StorageError(
      'STORAGE_DATABASE_TOO_LARGE',
      `The TinyGres database cannot exceed ${MAX_DATABASE_BYTES} bytes`,
    );
  }
  return low;
}

function isUint32(value: number): boolean {
  return Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff;
}

function assertExactPage(bytes: Uint8Array, label: string): void {
  assertUint8Array(bytes, label);
  if (bytes.byteLength !== PAGE_SIZE) {
    throw new RangeError(`${label} must contain exactly ${PAGE_SIZE} bytes`);
  }
}

function assertUint8Array(value: Uint8Array, label: string): void {
  if (!(value instanceof Uint8Array)) {
    throw new TypeError(`${label} must be a Uint8Array`);
  }
}

function pageOffset(pageId: number): number {
  return pageId * PAGE_SIZE;
}

function readExactly(
  handle: SyncPageAccessHandle,
  at: number,
  target: Uint8Array,
): void {
  let offset = 0;
  try {
    while (offset < target.byteLength) {
      const read = handle.read(target.subarray(offset), {at: at + offset});
      if (
        !Number.isInteger(read) ||
        read <= 0 ||
        read > target.byteLength - offset
      ) {
        throw new StorageError(
          'STORAGE_READ_FAILED',
          'TinyGres received a short database page read with no progress',
          true,
        );
      }
      offset += read;
    }
  } catch (error) {
    throw pageStorageError(
      error,
      'STORAGE_READ_FAILED',
      'TinyGres could not read a database page',
    );
  }
}

function writeExactly(
  handle: SyncPageAccessHandle,
  at: number,
  source: Uint8Array,
): void {
  let offset = 0;
  try {
    while (offset < source.byteLength) {
      const written = handle.write(source.subarray(offset), {at: at + offset});
      if (
        !Number.isInteger(written) ||
        written <= 0 ||
        written > source.byteLength - offset
      ) {
        throw new StorageError(
          'STORAGE_WRITE_FAILED',
          'TinyGres received a short database page write with no progress',
          true,
        );
      }
      offset += written;
    }
  } catch (error) {
    throw pageStorageError(
      error,
      'STORAGE_WRITE_FAILED',
      'TinyGres could not write a database page',
    );
  }
}

/**
 * Completes an in-place write without copying the caller's view. A thrown
 * access-handle write may have changed storage before it threw, and a later
 * non-progressing write follows an observed partial mutation. Those outcomes
 * cannot be reported as retryable without first reopening the database.
 */
function writeExistingExactly(
  handle: SyncPageAccessHandle,
  at: number,
  source: Uint8Array,
): void {
  let offset = 0;
  while (offset < source.byteLength) {
    let written: number;
    try {
      written = handle.write(source.subarray(offset), {at: at + offset});
    } catch (error) {
      throw unknownWriteOutcome(error);
    }
    if (
      !Number.isInteger(written) ||
      written < 0 ||
      written > source.byteLength - offset
    ) {
      throw unknownWriteOutcome(
        new Error('The page device returned an invalid write length'),
      );
    }
    if (written === 0) {
      if (offset === 0) {
        throw new StorageError(
          'STORAGE_WRITE_FAILED',
          'TinyGres received a short database page write with no progress',
          true,
        );
      }
      throw unknownWriteOutcome(
        new Error('A partial database page write stopped making progress'),
      );
    }
    offset += written;
  }
}

function unknownWriteOutcome(error: unknown): StorageError {
  const mapped = pageStorageError(
    error,
    'STORAGE_WRITE_FAILED',
    'TinyGres could not write a database page',
  );
  return new StorageError(
    'STORAGE_COMMIT_OUTCOME_UNKNOWN',
    `TinyGres could not safely complete an in-place page write: ${mapped.message}`,
  );
}

function pageStorageError(
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
  if (name === 'QuotaExceededError') {
    return new StorageError(
      'STORAGE_QUOTA_EXCEEDED',
      'The browser has no space available for TinyGres database pages',
      true,
    );
  }
  const message =
    error instanceof Error && error.message
      ? `${fallbackMessage}: ${error.message}`
      : fallbackMessage;
  return new StorageError(fallbackCode, message, true);
}
