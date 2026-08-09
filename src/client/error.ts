import type {SerializedTinygresError} from '../protocol.js';

export class TinygresError extends Error {
  readonly code: string;
  readonly details: SerializedTinygresError['details'];
  readonly retryable: boolean;

  constructor(error: SerializedTinygresError) {
    super(error.message);
    this.name = 'TinygresError';
    this.code = error.code;
    this.details = error.details;
    this.retryable = error.retryable ?? false;
  }

  static fromUnknown(
    error: unknown,
    code = 'UNKNOWN_ERROR',
  ): TinygresError {
    if (error instanceof TinygresError) {
      return error;
    }
    if (error instanceof Error) {
      return new TinygresError({code, message: error.message});
    }
    return new TinygresError({code, message: String(error)});
  }
}
