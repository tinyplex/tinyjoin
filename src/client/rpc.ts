import {isRecord, isUndefined, objFreeze, objHasOwn} from '../common.js';
import {
  PROTOCOL_VERSION,
  isRpcResult,
  isRpcResultHeader,
  isWorkerEvent,
  isSqlResultText,
  isWorkerResponse,
  readSqlResult,
  type RpcMethod,
  type RpcMethods,
  type WorkerEvent,
  type WorkerRequest,
} from '../protocol.js';
import type {WorkerLike} from './client.js';
import {ClientError, clientError} from './error.js';

export type ResultValidation = 'full' | 'header';

export interface WorkerRpc {
  /**
   * Sends a request, and settles with its result, as `read` returns it when
   * given. Reading the result as the response arrives, rather than in a
   * promise chained onto this one, saves the caller a turn of the microtask
   * queue between one statement and the next.
   */
  request<Method extends RpcMethod, Result = RpcMethods[Method]['response']>(
    method: Method,
    params: RpcMethods[Method]['request'],
    read?: (response: RpcMethods[Method]['response']) => Result,
  ): Promise<Result>;
  onEvent(listener: (event: WorkerEvent) => void): void;
  dispose(error?: ClientError): void;
}

type PendingRequest = {
  method: RpcMethod;
  read: ((response: never) => unknown) | undefined;
  resolve(value: unknown): void;
  reject(error: unknown): void;
};

const MISMATCH = 'PROTOCOL_MISMATCH';

/** The error for a request made after its connection was disposed. */
export const workerTerminated = (): ClientError =>
  clientError('WORKER_TERMINATED', 'The TinyJoin worker has been closed');

/**
 * Carries the RPC protocol over one Worker.
 *
 * Anything that makes the Worker untrustworthy - an unreadable message, an
 * unexpected result, a crash - disposes the connection and rejects every
 * request still in flight, rather than leaving a caller waiting forever.
 */
export const createWorkerRpc = (
  worker: WorkerLike,
  resultValidation: ResultValidation = 'full',
): WorkerRpc => {
  const pending = new Map<number, PendingRequest>();
  const eventListeners = new Set<(event: WorkerEvent) => void>();
  let nextId = 1;
  let disposed = false;

  const dispose = (error?: ClientError): void => {
    if (disposed) {
      return;
    }
    disposed = true;
    worker.removeEventListener('message', onMessage);
    worker.removeEventListener('messageerror', onMessageError);
    worker.removeEventListener('error', onError);
    worker.terminate?.();
    const reason = error ?? workerTerminated();
    for (const request of pending.values()) {
      request.reject(reason);
    }
    pending.clear();
    eventListeners.clear();
  };

  const onMessage = (event: MessageEvent<unknown>): void => {
    // A response carries an id and an event does not, so a response skips the
    // event check.
    if (
      !(isRecord(event.data) && objHasOwn(event.data, 'id')) &&
      isWorkerEvent(event.data)
    ) {
      for (const listener of eventListeners) {
        listener(event.data);
      }
      return;
    }
    if (!isWorkerResponse(event.data)) {
      dispose(
        clientError(
          MISMATCH,
          'The TinyJoin worker sent an invalid protocol message',
        ),
      );
      return;
    }
    const request = pending.get(event.data.id);
    if (isUndefined(request)) {
      return;
    }
    if (!event.data.ok) {
      pending.delete(event.data.id);
      request.reject(new ClientError(event.data.error));
      return;
    }
    // A statement's result that published nothing arrives as its text, which
    // only the page reads. One that cannot be read fails alone, as the Worker
    // would have failed it: the Worker and its database are unharmed.
    const sent = event.data.result;
    const text =
      (request.method === 'executeSql' ||
        request.method === 'executePrepared') &&
      isSqlResultText(sent)
        ? sent
        : undefined;
    const result = isUndefined(text) ? sent : readSqlResult(text);
    const isValid =
      resultValidation === 'full' ? isRpcResult : isRpcResultHeader;
    if (!isValid(request.method, result)) {
      if (!isUndefined(text)) {
        pending.delete(event.data.id);
        request.reject(
          clientError(
            'BRIDGE_SERIALIZATION_ERROR',
            'WASM returned an invalid structured SQL result',
          ),
        );
        return;
      }
      dispose(
        clientError(
          MISMATCH,
          'The TinyJoin worker returned an invalid result for the requested operation',
        ),
      );
      return;
    }
    pending.delete(event.data.id);
    if (isUndefined(request.read)) {
      request.resolve(result);
      return;
    }
    let read: unknown;
    try {
      read = request.read(result as never);
    } catch (error) {
      request.reject(error);
      return;
    }
    request.resolve(read);
  };

  const onMessageError = (): void =>
    dispose(
      clientError(
        'WORKER_MESSAGE_ERROR',
        'The browser could not deserialize a TinyJoin worker message',
      ),
    );

  const onError = (event: ErrorEvent): void =>
    dispose(
      clientError(
        'WORKER_ERROR',
        event.message || 'The TinyJoin worker crashed',
      ),
    );

  worker.addEventListener('message', onMessage);
  worker.addEventListener('messageerror', onMessageError);
  worker.addEventListener('error', onError);

  return objFreeze({
    request: <
      Method extends RpcMethod,
      Result = RpcMethods[Method]['response'],
    >(
      method: Method,
      params: RpcMethods[Method]['request'],
      read?: (response: RpcMethods[Method]['response']) => Result,
    ): Promise<Result> => {
      if (disposed) {
        return Promise.reject(workerTerminated());
      }
      const id = nextId++;
      const message = {v: PROTOCOL_VERSION, id, method, params} as WorkerRequest;
      return new Promise((resolve, reject) => {
        pending.set(id, {method, read, resolve, reject});
        try {
          worker.postMessage(message);
        } catch (error) {
          pending.delete(id);
          reject(
            error instanceof ClientError
              ? error
              : clientError(
                  'WORKER_POST_FAILED',
                  error instanceof Error ? error.message : String(error),
                ),
          );
        }
      }) as Promise<Result>;
    },

    onEvent: (listener: (event: WorkerEvent) => void): void => {
      eventListeners.add(listener);
    },

    dispose,
  });
};
