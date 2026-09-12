import {isUndefined} from '../common.js';
import type {SerializedError} from '../protocol.js';

export class ClientError extends Error {
  readonly code: string;
  readonly details: SerializedError['details'];
  readonly retryable: boolean;

  constructor(error: SerializedError) {
    super(error.message);
    this.name = 'ClientError';
    this.code = error.code;
    this.details = isUndefined(error.details)
      ? undefined
      : structuredClone(error.details);
    this.retryable = error.retryable ?? false;
  }
}

/** The common case: a coded failure TinyJoin raised itself. */
export const clientError = (code: string, message: string): ClientError =>
  new ClientError({code, message});
