import {
  type PageAuthorityClassification,
  type PageAuthorityMarker,
  PAGE_AUTHORITY_RECORD_BYTES,
  classifyPageAuthorityRecord,
  pageAuthorityCommitMagic,
  stagePageAuthorityMarker,
} from './page-authority.js';
import {
  MAX_DATABASE_BYTES,
  OpfsPageDevice,
  PAGE_SIZE,
  type PageDevice,
  type SyncPageAccessHandle,
} from './page-device.js';
import {assertDatabaseName, StorageError} from './storage-error.js';

export const PAGE_DATABASE_FILE_NAME = 'database-v1.pages';
export const FIRST_DATA_PAGE_ID = 8;
export const MIN_PAGE_DATABASE_BYTES = FIRST_DATA_PAGE_ID * PAGE_SIZE;

export interface OpfsPageStorageFileHandle {
  createSyncAccessHandle?: () => Promise<SyncPageAccessHandle>;
}

export interface OpfsPageStorageDirectoryHandle {
  getDirectoryHandle(
    name: string,
    options: {create: boolean},
  ): Promise<OpfsPageStorageDirectoryHandle>;
  getFileHandle(
    name: string,
    options: {create: boolean},
  ): Promise<OpfsPageStorageFileHandle>;
}

export interface OpfsPageStorageProvider {
  getDirectory(): Promise<OpfsPageStorageDirectoryHandle>;
}

export interface OpfsPageStorageSession {
  readonly authority: PageAuthorityClassification;
  readonly pageDevice: PageDevice | undefined;
  createStagedPageDevice(): Promise<PageDevice>;
  publishPageAuthority(marker: PageAuthorityMarker): void;
  close(): void;
}

/**
 * Opens the cross-version OPFS authority lock and, when already published,
 * the page database it protects. Legacy and interrupted staged states retain
 * only the slot-A lock until migration explicitly creates a page device.
 */
export async function createOpfsPageStorageSession(
  databaseName: string,
  provider?: OpfsPageStorageProvider,
): Promise<OpfsPageStorageSession> {
  assertDatabaseName(databaseName);
  const selectedProvider = provider ?? defaultOpfsProvider();
  const databaseDirectory = await openDatabaseDirectory(
    databaseName,
    selectedProvider,
  );

  let authorityHandle: SyncPageAccessHandle | undefined;
  try {
    const authorityFile = await databaseDirectory.getFileHandle(
      'snapshot-a.bin',
      {create: true},
    );
    authorityHandle = await openSyncHandle(
      authorityFile,
      'TinyGres could not acquire its cross-version OPFS database lock',
    );
    const authority = classifyPageAuthorityRecord(
      readAuthorityRecord(authorityHandle),
    );
    const session = new OpfsPageStorageSessionImpl(
      databaseDirectory,
      authorityHandle,
      authority,
    );
    authorityHandle = undefined;

    if (authority.kind === 'page') {
      await session.openCommittedPageDevice();
    }
    return session;
  } catch (error) {
    if (authorityHandle !== undefined) {
      closePreserving(error, authorityHandle);
    }
    throw storageError(
      error,
      'OPFS_UNAVAILABLE',
      'TinyGres could not open its OPFS page storage',
    );
  }
}

class OpfsPageStorageSessionImpl implements OpfsPageStorageSession {
  readonly #databaseDirectory: OpfsPageStorageDirectoryHandle;
  readonly #authorityHandle: SyncPageAccessHandle;
  #authority: PageAuthorityClassification;
  #pageDevice: SessionPageDevice | undefined;
  #closed = false;

  constructor(
    databaseDirectory: OpfsPageStorageDirectoryHandle,
    authorityHandle: SyncPageAccessHandle,
    authority: PageAuthorityClassification,
  ) {
    this.#databaseDirectory = databaseDirectory;
    this.#authorityHandle = authorityHandle;
    this.#authority = immutableAuthority(authority);
  }

  get authority(): PageAuthorityClassification {
    return this.#authority;
  }

  get pageDevice(): PageDevice | undefined {
    return this.#pageDevice;
  }

  /**
   * Creates or resets the non-authoritative page file for a legacy migration.
   * The returned device also owns this session: closing it releases the page
   * handle first and the cross-version slot-A lock second.
   */
  async createStagedPageDevice(): Promise<PageDevice> {
    this.#assertOpen();
    this.#assertAuthorityUnpublished();
    if (this.#pageDevice !== undefined) {
      throw new StorageError(
        'STORAGE_PAGE_DEVICE_ALREADY_OPEN',
        'The staged TinyGres page device is already open',
      );
    }

    let pageHandle: SyncPageAccessHandle | undefined;
    let transferred = false;
    try {
      const pageFile = await this.#databaseDirectory.getFileHandle(
        PAGE_DATABASE_FILE_NAME,
        {create: true},
      );
      pageHandle = await openSyncHandle(
        pageFile,
        'TinyGres could not acquire its staged OPFS page file',
      );
      try {
        pageHandle.truncate(0);
        pageHandle.flush();
      } catch (error) {
        throw storageError(
          error,
          'STORAGE_WRITE_FAILED',
          'TinyGres could not reset its staged OPFS page file',
        );
      }
      transferred = true;
      const inner = new OpfsPageDevice(pageHandle);
      pageHandle = undefined;
      this.#pageDevice = new SessionPageDevice(inner, () => this.close());
      return this.#pageDevice;
    } catch (error) {
      if (!transferred && pageHandle !== undefined) {
        closePreserving(error, pageHandle);
      }
      throw storageError(
        error,
        'OPFS_UNAVAILABLE',
        'TinyGres could not create its staged OPFS page device',
      );
    }
  }

  /**
   * Publishes the page database by replacing legacy slot A. Before the final
   * magic write, every on-disk state still permits legacy recovery. Once that
   * write is attempted, failure is necessarily an unknown commit outcome and
   * the whole session is poisoned and closed. Callers must first make the
   * retained legacy slot-B fallback durable; this primitive deliberately does
   * not create or validate that migration prerequisite.
   */
  publishPageAuthority(marker: PageAuthorityMarker): void {
    this.#assertOpen();
    this.#assertAuthorityUnpublished();
    const pageDevice = this.#pageDevice;
    if (pageDevice === undefined) {
      throw new StorageError(
        'STORAGE_PAGE_DEVICE_REQUIRED',
        'A staged TinyGres page device must be initialized before publication',
      );
    }
    // Read caller-controlled marker properties once, then validate and allocate
    // every publication byte before touching slot A. The canonical authority
    // object is already available if the durable final write succeeds.
    const publishedAuthority = immutableAuthority({
      kind: 'page',
      marker: {
        appliedJournalSequence: marker.appliedJournalSequence,
        databaseRevision: marker.databaseRevision,
      },
    });
    if (publishedAuthority.kind !== 'page') {
      throw new Error('The canonical page authority must remain a page marker');
    }
    const staged = stagePageAuthorityMarker(publishedAuthority.marker);
    const commitMagic = pageAuthorityCommitMagic();

    // Marker accessors are outside the trusted session state and can re-enter
    // this object. Recheck every publication guard after the last such access.
    this.#assertOpen();
    this.#assertAuthorityUnpublished();
    if (this.#pageDevice !== pageDevice) {
      throw new StorageError(
        'STORAGE_INVALID_STATE',
        'The staged TinyGres page device changed during authority publication',
      );
    }
    if (pageDevice.pageCount() < FIRST_DATA_PAGE_ID) {
      throw new StorageError(
        'STORAGE_CORRUPT',
        `The staged TinyGres page file must contain at least ${FIRST_DATA_PAGE_ID} metadata pages`,
      );
    }

    // The validated page generation must be durable before slot A can make it
    // the sole authority. A failed flush is still a known pre-publication
    // outcome: slot A has not been touched and the caller may safely retry.
    try {
      pageDevice.flush();
    } catch (error) {
      throw storageError(
        error,
        'STORAGE_WRITE_FAILED',
        'TinyGres could not flush its staged page database before publication',
      );
    }

    try {
      this.#authorityHandle.truncate(0);
      this.#authorityHandle.flush();
      writeExactly(this.#authorityHandle, 0, staged);
      this.#authorityHandle.truncate(PAGE_AUTHORITY_RECORD_BYTES);
      verifyExactRecord(this.#authorityHandle, staged);
      this.#authorityHandle.flush();
    } catch (error) {
      throw storageError(
        error,
        'STORAGE_WRITE_FAILED',
        'TinyGres could not stage its OPFS page authority marker',
      );
    }

    try {
      writeExactly(this.#authorityHandle, 0, commitMagic);
      this.#authorityHandle.flush();
    } catch (error) {
      const mapped = storageError(
        error,
        'STORAGE_WRITE_FAILED',
        'TinyGres could not finish its OPFS page authority marker',
      );
      try {
        this.close();
      } catch {
        // The uncertain publication outcome is the first and decisive error.
      }
      throw new StorageError(
        'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        `TinyGres could not determine whether page authority was published: ${mapped.message}`,
      );
    }

    this.#authority = publishedAuthority;
  }

  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;

    let firstError: unknown;
    if (this.#pageDevice !== undefined) {
      try {
        this.#pageDevice.closeInner();
      } catch (error) {
        firstError = error;
      }
    }
    try {
      this.#authorityHandle.close();
    } catch (error) {
      firstError ??= storageError(
        error,
        'STORAGE_CLOSE_FAILED',
        'TinyGres could not release its cross-version OPFS database lock',
      );
    }
    if (firstError !== undefined) {
      throw firstError;
    }
  }

  /** @internal Opens a previously committed page database without repairing it. */
  async openCommittedPageDevice(): Promise<void> {
    this.#assertOpen();
    if (this.#authority.kind !== 'page' || this.#pageDevice !== undefined) {
      throw new StorageError(
        'STORAGE_INVALID_STATE',
        'The TinyGres page database cannot be opened in this authority state',
      );
    }

    let pageHandle: SyncPageAccessHandle | undefined;
    let transferred = false;
    try {
      let pageFile: OpfsPageStorageFileHandle;
      try {
        pageFile = await this.#databaseDirectory.getFileHandle(
          PAGE_DATABASE_FILE_NAME,
          {create: false},
        );
      } catch (error) {
        if (errorName(error) === 'NotFoundError') {
          throw new StorageError(
            'STORAGE_CORRUPT',
            'The authoritative TinyGres page file is missing',
          );
        }
        throw error;
      }
      pageHandle = await openSyncHandle(
        pageFile,
        'TinyGres could not acquire its authoritative OPFS page file',
      );
      validateCommittedPageFile(pageHandle);

      // Alignment has already been proven while slot A is exclusively held,
      // so OpfsPageDevice has no torn tail to repair after authority commits.
      transferred = true;
      const inner = new OpfsPageDevice(pageHandle);
      pageHandle = undefined;
      this.#pageDevice = new SessionPageDevice(inner, () => this.close());
    } catch (error) {
      if (!transferred && pageHandle !== undefined) {
        closePreserving(error, pageHandle);
      }
      try {
        this.close();
      } catch {
        // Preserve the page-open failure rather than a cleanup failure.
      }
      throw storageError(
        error,
        'OPFS_UNAVAILABLE',
        'TinyGres could not open its authoritative OPFS page file',
      );
    }
  }

  #assertOpen(): void {
    if (this.#closed) {
      throw new StorageError(
        'STORAGE_CLOSED',
        'The TinyGres OPFS page storage session is closed',
      );
    }
  }

  #assertAuthorityUnpublished(): void {
    if (this.#authority.kind === 'page') {
      throw new StorageError(
        'STORAGE_AUTHORITY_ALREADY_PUBLISHED',
        'The TinyGres page database is already authoritative',
      );
    }
  }
}

function immutableAuthority(
  authority: PageAuthorityClassification,
): PageAuthorityClassification {
  if (authority.kind === 'page') {
    return Object.freeze({
      kind: 'page',
      marker: Object.freeze({
        appliedJournalSequence: authority.marker.appliedJournalSequence,
        databaseRevision: authority.marker.databaseRevision,
      }),
    });
  }
  return Object.freeze({kind: authority.kind});
}

class SessionPageDevice implements PageDevice {
  readonly #inner: PageDevice;
  readonly #closeSession: () => void;

  constructor(inner: PageDevice, closeSession: () => void) {
    this.#inner = inner;
    this.#closeSession = closeSession;
  }

  pageCount(): number {
    return this.#inner.pageCount();
  }

  readPage(
    pageIdLow: number,
    pageIdHigh: number,
    target: Uint8Array,
  ): number {
    return this.#inner.readPage(pageIdLow, pageIdHigh, target);
  }

  writePage(
    pageIdLow: number,
    pageIdHigh: number,
    source: Uint8Array,
  ): number {
    return this.#inner.writePage(pageIdLow, pageIdHigh, source);
  }

  flush(): void {
    this.#inner.flush();
  }

  close(): void {
    this.#closeSession();
  }

  closeInner(): void {
    this.#inner.close();
  }
}

async function openDatabaseDirectory(
  databaseName: string,
  provider: OpfsPageStorageProvider,
): Promise<OpfsPageStorageDirectoryHandle> {
  try {
    const root = await provider.getDirectory();
    const tinygresDirectory = await root.getDirectoryHandle('tinygres-v1', {
      create: true,
    });
    return await tinygresDirectory.getDirectoryHandle(`db-${databaseName}`, {
      create: true,
    });
  } catch (error) {
    throw storageError(
      error,
      'OPFS_UNAVAILABLE',
      'TinyGres could not initialize its OPFS database directory',
    );
  }
}

async function openSyncHandle(
  file: OpfsPageStorageFileHandle,
  fallbackMessage: string,
): Promise<SyncPageAccessHandle> {
  if (typeof file.createSyncAccessHandle !== 'function') {
    throw new StorageError(
      'OPFS_UNAVAILABLE',
      'TinyGres OPFS page storage requires synchronous access handles in a dedicated Worker',
    );
  }
  try {
    return await file.createSyncAccessHandle();
  } catch (error) {
    throw storageError(error, 'OPFS_UNAVAILABLE', fallbackMessage);
  }
}

function readAuthorityRecord(handle: SyncPageAccessHandle): Uint8Array {
  let size: number;
  try {
    size = handle.getSize();
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_READ_FAILED',
      'TinyGres could not read its OPFS authority record size',
    );
  }
  if (!Number.isSafeInteger(size) || size < 0) {
    throw new StorageError(
      'STORAGE_CORRUPT',
      'The TinyGres OPFS authority record has an invalid byte length',
    );
  }

  // One extra byte preserves the marker codec's exact-length validation while
  // bounding reads of ordinary legacy snapshots.
  const bytes = new Uint8Array(
    Math.min(size, PAGE_AUTHORITY_RECORD_BYTES + 1),
  );
  readExactly(handle, 0, bytes);
  return bytes;
}

function validateCommittedPageFile(handle: SyncPageAccessHandle): void {
  let size: number;
  try {
    size = handle.getSize();
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_READ_FAILED',
      'TinyGres could not read its authoritative page file size',
    );
  }
  if (!Number.isSafeInteger(size) || size < 0) {
    throw new StorageError(
      'STORAGE_CORRUPT',
      'The authoritative TinyGres page file has an invalid byte length',
    );
  }
  if (size > MAX_DATABASE_BYTES) {
    throw new StorageError(
      'STORAGE_DATABASE_TOO_LARGE',
      `The authoritative TinyGres page file exceeds ${MAX_DATABASE_BYTES} bytes`,
    );
  }
  if (size < MIN_PAGE_DATABASE_BYTES) {
    throw new StorageError(
      'STORAGE_CORRUPT',
      `The authoritative TinyGres page file must contain at least ${FIRST_DATA_PAGE_ID} metadata pages`,
    );
  }
  if (size % PAGE_SIZE !== 0) {
    throw new StorageError(
      'STORAGE_CORRUPT',
      'The authoritative TinyGres page file is not aligned to its page size',
    );
  }
}

function verifyExactRecord(
  handle: SyncPageAccessHandle,
  expected: Uint8Array,
): void {
  let size: number;
  try {
    size = handle.getSize();
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_READ_FAILED',
      'TinyGres could not verify its staged authority record size',
    );
  }
  if (size !== expected.byteLength) {
    throw new StorageError(
      'STORAGE_WRITE_FAILED',
      'TinyGres could not verify the exact staged authority record length',
      true,
    );
  }
  const actual = new Uint8Array(expected.byteLength);
  readExactly(handle, 0, actual);
  if (!actual.every((byte, index) => byte === expected[index])) {
    throw new StorageError(
      'STORAGE_WRITE_FAILED',
      'TinyGres could not verify the staged authority record contents',
      true,
    );
  }
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
          'TinyGres received a short OPFS authority read with no progress',
          true,
        );
      }
      offset += read;
    }
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_READ_FAILED',
      'TinyGres could not read its OPFS authority record',
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
          'TinyGres received a short OPFS authority write with no progress',
          true,
        );
      }
      offset += written;
    }
  } catch (error) {
    throw storageError(
      error,
      'STORAGE_WRITE_FAILED',
      'TinyGres could not write its OPFS authority record',
    );
  }
}

function closePreserving(
  originalError: unknown | undefined,
  handle: SyncPageAccessHandle,
): void {
  try {
    handle.close();
  } catch (closeError) {
    if (originalError === undefined) {
      throw storageError(
        closeError,
        'STORAGE_CLOSE_FAILED',
        'TinyGres could not close an OPFS page storage handle',
      );
    }
  }
}

function defaultOpfsProvider(): OpfsPageStorageProvider {
  const storage = globalThis.navigator?.storage as
    | {getDirectory?: () => Promise<OpfsPageStorageDirectoryHandle>}
    | undefined;
  if (typeof storage?.getDirectory !== 'function') {
    throw new StorageError(
      'OPFS_UNAVAILABLE',
      'Origin private file system storage is unavailable in this runtime',
    );
  }
  return {getDirectory: () => storage.getDirectory!()};
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
    const retryable =
      'retryable' in error && typeof error.retryable === 'boolean'
        ? error.retryable
        : false;
    return new StorageError(error.code, error.message, retryable);
  }
  const name = errorName(error);
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
      'The browser has no space available for TinyGres OPFS page storage',
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

function errorName(error: unknown): string {
  return typeof error === 'object' && error !== null && 'name' in error
    ? String(error.name)
    : '';
}
