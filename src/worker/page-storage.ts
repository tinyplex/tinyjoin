import {
  asCodedError,
  errorDetail,
  errorName,
  isFunction,
  isUndefined,
  objFreeze,
} from '../common.js';
import {
  createOpfsPageDevice,
  type PageDevice,
  type SyncPageAccessHandle,
} from './page-device.js';
import {assertDatabaseName, StorageError} from './storage-error.js';

export const PAGE_DATABASE_FILE_NAME = 'database.pages';
const STORAGE_DIRECTORY_NAME = 'tinyjoin-pages-v1';

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

const OPFS_UNAVAILABLE = 'OPFS_UNAVAILABLE';

// The DOMException names a browser uses when OPFS is present but unusable, as
// opposed to a failure that is worth retrying.
const UNAVAILABLE_NAMES = [
  'InvalidStateError',
  'NotAllowedError',
  'SecurityError',
];

/** Opens the one page file that is the complete persistent database. */
export const createOpfsPageStorageSession = async (
  databaseName: string,
  provider?: OpfsPageStorageProvider,
): Promise<OpfsPageStorageSession> => {
  assertDatabaseName(databaseName);
  const selectedProvider = provider ?? defaultOpfsProvider();
  let handle: SyncPageAccessHandle | undefined;
  let pageDevice: PageDevice | undefined;
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
    pageDevice = createOpfsPageDevice(handle);
    handle = undefined;
    let closed = false;
    const device = pageDevice;
    return objFreeze({
      pageDevice: device,
      close: (): void => {
        if (!closed) {
          closed = true;
          device.close();
        }
      },
    });
  } catch (error) {
    closePreserving(pageDevice ?? handle);
    throw storageError(
      error,
      OPFS_UNAVAILABLE,
      'TinyJoin could not open its OPFS page database',
    );
  }
};

const openSyncHandle = async (
  file: OpfsPageStorageFileHandle,
): Promise<SyncPageAccessHandle> => {
  if (!isFunction(file.createSyncAccessHandle)) {
    throw new StorageError(
      OPFS_UNAVAILABLE,
      'TinyJoin OPFS page storage requires synchronous access handles in a dedicated Worker',
    );
  }
  try {
    return await file.createSyncAccessHandle();
  } catch (error) {
    throw storageError(
      error,
      OPFS_UNAVAILABLE,
      'TinyJoin could not acquire its OPFS page database',
    );
  }
};

const closePreserving = (closeable: {close(): void} | undefined): void => {
  try {
    closeable?.close();
  } catch {
    // Preserve the failure that prevented the database from opening.
  }
};

const defaultOpfsProvider = (): OpfsPageStorageProvider => {
  const storage = globalThis.navigator?.storage as
    | {getDirectory?: () => Promise<OpfsPageStorageDirectoryHandle>}
    | undefined;
  if (!isFunction(storage?.getDirectory)) {
    throw new StorageError(
      OPFS_UNAVAILABLE,
      'Origin private file system storage is unavailable in this runtime',
    );
  }
  return {getDirectory: () => storage.getDirectory!()};
};

const storageError = (
  error: unknown,
  fallbackCode: string,
  fallbackMessage: string,
): StorageError => {
  if (error instanceof StorageError) {
    return error;
  }
  const coded = asCodedError(error);
  if (!isUndefined(coded)) {
    return new StorageError(coded.code, coded.message, coded.retryable ?? false);
  }
  const name = errorName(error);
  if (name === 'NoModificationAllowedError') {
    return new StorageError(
      'STORAGE_LOCKED',
      'Another TinyJoin worker already has this OPFS database open',
      true,
    );
  }
  if (name === 'QuotaExceededError') {
    return new StorageError(
      'STORAGE_QUOTA_EXCEEDED',
      'The browser has no space available for TinyJoin OPFS page storage',
      true,
    );
  }
  const message = `${fallbackMessage}${errorDetail(error)}`;
  return UNAVAILABLE_NAMES.includes(name)
    ? new StorageError(OPFS_UNAVAILABLE, message)
    : new StorageError(fallbackCode, message, true);
};
