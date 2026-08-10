import type {SerializedError} from '../protocol.js';

export class ClientError extends Error {
  readonly code: string;
  readonly details: SerializedError['details'];
  readonly retryable: boolean;

  constructor(error: SerializedError) {
    super(error.message);
    this.name = 'ClientError';
    this.code = error.code;
    this.details = error.details;
    this.retryable = error.retryable ?? false;
  }

  static fromUnknown(
    error: unknown,
    code = 'UNKNOWN_ERROR',
  ): ClientError {
    if (error instanceof ClientError) {
      return error;
    }
    if (error instanceof Error) {
      return new ClientError({code, message: error.message});
    }
    return new ClientError({code, message: String(error)});
  }
}
