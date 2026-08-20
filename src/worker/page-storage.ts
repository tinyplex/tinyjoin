import {
  OpfsPageDevice,
  type PageDevice,
  type SyncPageAccessHandle,
} from './page-device.js';
import {assertDatabaseName, StorageError} from './storage-error.js';

export const PAGE_DATABASE_FILE_NAME = 'database.pages';
const STORAGE_DIRECTORY_NAME = 'tinygres-pages-v1';

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

/**
 * A page device whose synchronous access handle is also the database lock.
 * Keeping this handle open for the engine lifetime prevents a second Worker
 * from opening the same database while allowing different names concurrently.
 */
export interface OpfsPageStorageSession {
  readonly pageDevice: PageDevice;
  close(): void;
}

/** Opens the one page file that is the complete persistent database. */
export async function createOpfsPageStorageSession(
  databaseName: string,
  provider?: OpfsPageStorageProvider,
): Promise<OpfsPageStorageSession> {
  assertDatabaseName(databaseName);
  const selectedProvider = provider ?? defaultOpfsProvider();
  let handle: SyncPageAccessHandle | undefined;
  let device: OpfsPageDevice | undefined;
  try {
    const root = await selectedProvider.getDirectory();
    const storageDirectory = await root.getDirectoryHandle(
      STORAGE_DIRECTORY_NAME,
      {create: true},
    );
    const databaseDirectory = await storageDirectory.getDirectoryHandle(
      `db-${databaseName}`,
      {create: true},
    );
    const file = await databaseDirectory.getFileHandle(
      PAGE_DATABASE_FILE_NAME,
      {create: true},
    );
    handle = await openSyncHandle(file);
    device = new OpfsPageDevice(handle);
    handle = undefined;
    return new OpfsPageStorageSessionImpl(device);
  } catch (error) {
    if (device !== undefined) {
      closePreserving(device);
    } else if (handle !== undefined) {
      closePreserving(handle);
    }
    throw storageError(
      error,
      'OPFS_UNAVAILABLE',
      'TinyGres could not open its OPFS page database',
    );
  }
}

class OpfsPageStorageSessionImpl implements OpfsPageStorageSession {
  readonly pageDevice: PageDevice;
  #closed = false;

  constructor(pageDevice: PageDevice) {
    this.pageDevice = pageDevice;
  }

  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.pageDevice.close();
  }
}

async function openSyncHandle(
  file: OpfsPageStorageFileHandle,
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
    throw storageError(
      error,
      'OPFS_UNAVAILABLE',
      'TinyGres could not acquire its OPFS page database',
    );
  }
}

function closePreserving(closeable: {close(): void}): void {
  try {
    closeable.close();
  } catch {
    // Preserve the failure that prevented the database from opening.
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
    return new StorageError(
      error.code,
      error.message,
      'retryable' in error && typeof error.retryable === 'boolean'
        ? error.retryable
        : false,
    );
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
