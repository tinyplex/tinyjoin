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

/** The names an OPFS database may take, which are also directory names. */
export const DATABASE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

export const assertDatabaseName = (databaseName: string): void => {
  if (!DATABASE_NAME.test(databaseName)) {
    throw new StorageError(
      'INVALID_STORAGE_NAME',
      'An OPFS database name must be 1-64 ASCII letters, numbers, dots, underscores, or hyphens, and start with a letter or number',
    );
  }
};
