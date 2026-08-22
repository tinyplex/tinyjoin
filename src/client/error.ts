import type {SerializedError} from '../protocol.js';

export class ClientError extends Error {
  readonly code: string;
  readonly details: SerializedError['details'];
  readonly retryable: boolean;

  constructor(error: SerializedError) {
    super(error.message);
    this.name = 'ClientError';
    this.code = error.code;
    this.details =
      error.details === undefined ? undefined : structuredClone(error.details);
    this.retryable = error.retryable ?? false;
  }
}
