import {isUndefined, objFreeze} from '../common.js';
import {
  PROTOCOL_VERSION,
  isRpcResult,
  isRpcResultHeader,
  isWorkerEvent,
  isWorkerResponse,
  type RpcMethod,
  type RpcMethods,
  type WorkerEvent,
  type WorkerRequest,
} from '../protocol.js';
import type {WorkerLike} from './client.js';
import {ClientError, clientError} from './error.js';

export type ResultValidation = 'full' | 'header';

export interface WorkerRpc {
  request<Method extends RpcMethod>(
    method: Method,
    params: RpcMethods[Method]['request'],
  ): Promise<RpcMethods[Method]['response']>;
  onEvent(listener: (event: WorkerEvent) => void): void;
  dispose(error?: ClientError): void;
}

type PendingRequest = {
  method: RpcMethod;
  resolve(value: unknown): void;
  reject(error: unknown): void;
};

const TERMINATED = 'WORKER_TERMINATED';
const TERMINATED_MESSAGE = 'The TinyJoin worker has been closed';
const MISMATCH = 'PROTOCOL_MISMATCH';

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
    const reason = error ?? clientError(TERMINATED, TERMINATED_MESSAGE);
    for (const request of pending.values()) {
      request.reject(reason);
    }
    pending.clear();
    eventListeners.clear();
  };

  const onMessage = (event: MessageEvent<unknown>): void => {
    if (isWorkerEvent(event.data)) {
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
    const isValid =
      resultValidation === 'full' ? isRpcResult : isRpcResultHeader;
    if (!isValid(request.method, event.data.result)) {
      dispose(
        clientError(
          MISMATCH,
          'The TinyJoin worker returned an invalid result for the requested operation',
        ),
      );
      return;
    }
    pending.delete(event.data.id);
    request.resolve(event.data.result);
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
    request: <Method extends RpcMethod>(
      method: Method,
      params: RpcMethods[Method]['request'],
    ): Promise<RpcMethods[Method]['response']> => {
      if (disposed) {
        return Promise.reject(clientError(TERMINATED, TERMINATED_MESSAGE));
      }
      const id = nextId++;
      const message = {v: PROTOCOL_VERSION, id, method, params} as WorkerRequest;
      return new Promise((resolve, reject) => {
        pending.set(id, {method, resolve, reject});
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
      }) as Promise<RpcMethods[Method]['response']>;
    },

    onEvent: (listener: (event: WorkerEvent) => void): void => {
      eventListeners.add(listener);
    },

    dispose,
  });
};
