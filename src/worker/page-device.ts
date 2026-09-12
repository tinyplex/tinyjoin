import {
  asCodedError,
  errorDetail,
  errorName,
  isCount,
  isCountWithin,
  isInteger,
  isUndefined,
  MAX_U32,
  objFreeze,
} from '../common.js';
import {StorageError} from './storage-error.js';

export const PAGE_SIZE = 4096;
export const MAX_PAGES = 65_536;
export const MAX_DATABASE_BYTES = PAGE_SIZE * MAX_PAGES;

export interface PageDevice {
  pageCount(): number;
  /**
   * Reads directly into `target`. If the read fails, the target's contents are
   * undefined: a synchronous access handle may already have filled a prefix.
   */
  readPage(pageIdLow: number, pageIdHigh: number, target: Uint8Array): number;
  /**
   * Appends a page or replaces one in place. An existing page may only be an
   * inactive copy-on-write page; never overwrite metadata or data reachable
   * from the active superblock.
   */
  writePage(pageIdLow: number, pageIdHigh: number, source: Uint8Array): number;
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
export const createOpfsPageDevice = (
  handle: SyncPageAccessHandle,
): PageDevice => {
  let closed = false;

  // The validated byte length of the page file, which both the page count and
  // the torn-tail repair start from.
  const readByteLength = (): number => {
    let size: number;
    try {
      size = handle.getSize();
    } catch (error) {
      throw pageStorageError(error, READ_FAILED, COULD_NOT_READ_PAGE_COUNT);
    }
    if (!isCount(size)) {
      throw corrupt('has an invalid byte length');
    }
    if (size > MAX_DATABASE_BYTES) {
      throw new StorageError(
        TOO_LARGE,
        `The TinyJoin page file exceeds ${MAX_DATABASE_BYTES} bytes`,
      );
    }
    return size;
  };

  const readPageCount = (): number => {
    const size = readByteLength();
    if (size % PAGE_SIZE !== 0) {
      throw corrupt('is not aligned to its page size');
    }
    return size / PAGE_SIZE;
  };

  const truncateTo = (bytes: number): void => {
    handle.truncate(bytes);
    handle.flush();
  };

  // A crash can leave a partly written trailing page. Dropping it is always
  // safe, because a page only becomes reachable once the superblock names it.
  const repairTornTail = (): void => {
    const size = readByteLength();
    const alignedSize = size - (size % PAGE_SIZE);
    if (alignedSize === size) {
      return;
    }
    try {
      truncateTo(alignedSize);
    } catch (error) {
      throw new StorageError(
        UNKNOWN_OUTCOME,
        pageStorageError(
          error,
          WRITE_FAILED,
          'TinyJoin could not repair a torn trailing database page',
        ).message,
      );
    }
  };

  const rollbackAppend = (originalPageCount: number): void => {
    try {
      truncateTo(originalPageCount * PAGE_SIZE);
    } catch (error) {
      throw new StorageError(
        UNKNOWN_OUTCOME,
        `TinyJoin could not roll back a failed page append${errorDetail(error)}`,
      );
    }
  };

  try {
    repairTornTail();
  } catch (error) {
    // Construction takes ownership of the handle immediately. If validation
    // or repair fails, close that handle without allowing a cleanup failure
    // to replace the error that explains why the database could not open.
    closed = true;
    try {
      handle.close();
    } catch {
      // The original open error is more actionable and must be preserved.
    }
    throw error;
  }

  return objFreeze({
    pageCount: (): number => {
      assertOpen(closed);
      return readPageCount();
    },

    readPage: (low: number, high: number, target: Uint8Array): number => {
      assertOpen(closed);
      const pageId = pageIdFromWords(low, high, false);
      assertExactPage(target, 'The read target');
      if (pageId >= readPageCount()) {
        throw unallocated(pageId);
      }
      transferExactly(
        (view, at) => handle.read(view, {at}),
        pageId * PAGE_SIZE,
        target,
        'read',
      );
      return PAGE_SIZE;
    },

    writePage: (low: number, high: number, source: Uint8Array): number => {
      assertOpen(closed);
      const pageId = pageIdFromWords(low, high, true);
      assertExactPage(source, 'The page source');
      const count = readPageCount();
      if (pageId > count) {
        throw new RangeError(
          `Page ${pageId} cannot be written before page ${count} is allocated`,
        );
      }
      const at = pageId * PAGE_SIZE;
      const appending = pageId === count;
      try {
        if (appending) {
          transferExactly(
            (view, offset) => handle.write(view, {at: offset}),
            at,
            source,
            'write',
          );
        } else {
          writeExistingExactly(handle, at, source);
        }
      } catch (error) {
        if (appending) {
          rollbackAppend(count);
        }
        throw error;
      }
      return PAGE_SIZE;
    },

    flush: (): void => {
      assertOpen(closed);
      try {
        handle.flush();
      } catch (error) {
        throw pageStorageError(
          error,
          WRITE_FAILED,
          'TinyJoin could not flush database pages',
        );
      }
    },

    close: (): void => {
      if (closed) {
        return;
      }
      closed = true;
      try {
        handle.close();
      } catch (error) {
        throw pageStorageError(
          error,
          'STORAGE_CLOSE_FAILED',
          'TinyJoin could not close its page device',
        );
      }
    },
  });
};

/** A dependency-free page device for tests and non-persistent runtimes. */
export const createMemoryPageDevice = (): PageDevice => {
  const pages: Uint8Array[] = [];
  let closed = false;

  return objFreeze({
    pageCount: (): number => {
      assertOpen(closed);
      return pages.length;
    },

    readPage: (low: number, high: number, target: Uint8Array): number => {
      assertOpen(closed);
      const pageId = pageIdFromWords(low, high, false);
      assertExactPage(target, 'The read target');
      const page = pages[pageId];
      if (isUndefined(page)) {
        throw unallocated(pageId);
      }
      target.set(page);
      return PAGE_SIZE;
    },

    writePage: (low: number, high: number, source: Uint8Array): number => {
      assertOpen(closed);
      const pageId = pageIdFromWords(low, high, true);
      assertExactPage(source, 'The page source');
      if (pageId > pages.length) {
        throw new RangeError(
          `Page ${pageId} cannot be written before page ${pages.length} is allocated`,
        );
      }
      pages[pageId] = source.slice();
      return PAGE_SIZE;
    },

    flush: (): void => assertOpen(closed),

    close: (): void => {
      closed = true;
      pages.length = 0;
    },
  });
};

const READ_FAILED = 'STORAGE_READ_FAILED';
const WRITE_FAILED = 'STORAGE_WRITE_FAILED';
const TOO_LARGE = 'STORAGE_DATABASE_TOO_LARGE';
const UNKNOWN_OUTCOME = 'STORAGE_COMMIT_OUTCOME_UNKNOWN';
const COULD_NOT_READ_PAGE_COUNT =
  'TinyJoin could not read the database page count';

const corrupt = (problem: string): StorageError =>
  new StorageError('STORAGE_CORRUPT', `The TinyJoin page file ${problem}`);

const unallocated = (pageId: number): RangeError =>
  new RangeError(`Page ${pageId} has not been allocated`);

const assertOpen = (closed: boolean): void => {
  if (closed) {
    throw new StorageError(
      'STORAGE_CLOSED',
      'The TinyJoin page device is closed',
    );
  }
};

/**
 * Resolves the 64-bit page id WASM passes as two words. `appendable` allows the
 * one-past-the-end id that names a new page, which only a write may use.
 */
const pageIdFromWords = (
  low: number,
  high: number,
  appendable: boolean,
): number => {
  if (!isUint32(low) || !isUint32(high)) {
    throw new RangeError('Page id words must be unsigned 32-bit integers');
  }
  const maximum = appendable ? MAX_PAGES : MAX_PAGES - 1;
  if (high !== 0 || low > maximum) {
    throw new RangeError(`Page id must be between 0 and ${maximum}`);
  }
  if (low === MAX_PAGES) {
    throw new StorageError(
      TOO_LARGE,
      `The TinyJoin database cannot exceed ${MAX_DATABASE_BYTES} bytes`,
    );
  }
  return low;
};

const isUint32 = (value: number): boolean => isCountWithin(value, 0, MAX_U32);

const assertExactPage = (bytes: Uint8Array, label: string): void => {
  if (!(bytes instanceof Uint8Array)) {
    throw new TypeError(`${label} must be a Uint8Array`);
  }
  if (bytes.byteLength !== PAGE_SIZE) {
    throw new RangeError(`${label} must contain exactly ${PAGE_SIZE} bytes`);
  }
};

/**
 * Moves a whole page, one short transfer at a time. Reads and writes are the
 * same loop, and differ only in which way the bytes go: a handle that stops
 * making progress has failed, whichever direction it was going.
 */
const transferExactly = (
  transfer: (view: Uint8Array, at: number) => number,
  at: number,
  buffer: Uint8Array,
  action: 'read' | 'write',
): void => {
  const code = action === 'read' ? READ_FAILED : WRITE_FAILED;
  let offset = 0;
  try {
    while (offset < buffer.byteLength) {
      const moved = transfer(buffer.subarray(offset), at + offset);
      if (
        !isInteger(moved) ||
        moved <= 0 ||
        moved > buffer.byteLength - offset
      ) {
        throw new StorageError(code, shortTransfer(action), true);
      }
      offset += moved;
    }
  } catch (error) {
    throw pageStorageError(error, code, transferFailed(action));
  }
};

/**
 * Completes an in-place write without copying the caller's view. A thrown
 * access-handle write may have changed storage before it threw, and a later
 * non-progressing write follows an observed partial mutation. Those outcomes
 * cannot be reported as retryable without first reopening the database.
 */
const writeExistingExactly = (
  handle: SyncPageAccessHandle,
  at: number,
  source: Uint8Array,
): void => {
  let offset = 0;
  while (offset < source.byteLength) {
    let written: number;
    try {
      written = handle.write(source.subarray(offset), {at: at + offset});
    } catch (error) {
      throw unknownWriteOutcome(error);
    }
    if (
      !isInteger(written) ||
      written < 0 ||
      written > source.byteLength - offset
    ) {
      throw unknownWriteOutcome(
        new Error('The page device returned an invalid write length'),
      );
    }
    if (written === 0) {
      if (offset === 0) {
        throw new StorageError(WRITE_FAILED, shortTransfer('write'), true);
      }
      throw unknownWriteOutcome(
        new Error('A partial database page write stopped making progress'),
      );
    }
    offset += written;
  }
};

const shortTransfer = (action: string): string =>
  `TinyJoin received a short database page ${action} with no progress`;

const transferFailed = (action: string): string =>
  `TinyJoin could not ${action} a database page`;

const unknownWriteOutcome = (error: unknown): StorageError =>
  new StorageError(
    UNKNOWN_OUTCOME,
    'TinyJoin could not safely complete an in-place page write: ' +
      pageStorageError(error, WRITE_FAILED, transferFailed('write')).message,
  );

const pageStorageError = (
  error: unknown,
  fallbackCode: string,
  fallbackMessage: string,
): StorageError => {
  if (error instanceof StorageError) {
    return error;
  }
  const coded = asCodedError(error);
  if (coded !== undefined) {
    return new StorageError(coded.code, coded.message);
  }
  if (errorName(error) === 'QuotaExceededError') {
    return new StorageError(
      'STORAGE_QUOTA_EXCEEDED',
      'The browser has no space available for TinyJoin database pages',
      true,
    );
  }
  return new StorageError(
    fallbackCode,
    `${fallbackMessage}${errorDetail(error)}`,
    true,
  );
};
