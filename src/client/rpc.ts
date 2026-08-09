import {
  PROTOCOL_VERSION,
  isWorkerEvent,
  isWorkerResponse,
  type RpcMethod,
  type RpcMethods,
  type WorkerEvent,
  type WorkerRequest,
} from '../protocol.js';
import {TinygresError} from './error.js';

export interface WorkerLike {
  postMessage(message: unknown): void;
  addEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  addEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  addEventListener(type: 'error', listener: (event: ErrorEvent) => void): void;
  removeEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(type: 'error', listener: (event: ErrorEvent) => void): void;
  terminate?: () => void;
}

type PendingRequest = {
  resolve(value: unknown): void;
  reject(error: unknown): void;
};

export class WorkerRpc {
  readonly #worker: WorkerLike;
  readonly #pending = new Map<number, PendingRequest>();
  readonly #eventListeners = new Set<(event: WorkerEvent) => void>();
  #nextId = 1;
  #disposed = false;

  constructor(worker: WorkerLike) {
    this.#worker = worker;
    worker.addEventListener('message', this.#onMessage);
    worker.addEventListener('messageerror', this.#onMessageError);
    worker.addEventListener('error', this.#onError);
  }

  request<Method extends RpcMethod>(
    method: Method,
    params: RpcMethods[Method]['request'],
  ): Promise<RpcMethods[Method]['response']> {
    if (this.#disposed) {
      return Promise.reject(
        new TinygresError({
          code: 'WORKER_TERMINATED',
          message: 'The Tinygres worker has been closed',
        }),
      );
    }

    const id = this.#nextId++;
    const request = {
      v: PROTOCOL_VERSION,
      id,
      method,
      params,
    } as WorkerRequest;

    return new Promise((resolve, reject) => {
      this.#pending.set(id, {resolve, reject});
      try {
        this.#worker.postMessage(request);
      } catch (error) {
        this.#pending.delete(id);
        reject(TinygresError.fromUnknown(error, 'WORKER_POST_FAILED'));
      }
    }) as Promise<RpcMethods[Method]['response']>;
  }

  onEvent(listener: (event: WorkerEvent) => void): () => void {
    this.#eventListeners.add(listener);
    return () => this.#eventListeners.delete(listener);
  }

  dispose(error?: TinygresError): void {
    if (this.#disposed) {
      return;
    }
    this.#disposed = true;
    this.#worker.removeEventListener('message', this.#onMessage);
    this.#worker.removeEventListener('messageerror', this.#onMessageError);
    this.#worker.removeEventListener('error', this.#onError);
    this.#worker.terminate?.();
    const reason =
      error ??
      new TinygresError({
        code: 'WORKER_TERMINATED',
        message: 'The Tinygres worker has been closed',
      });
    for (const pending of this.#pending.values()) {
      pending.reject(reason);
    }
    this.#pending.clear();
    this.#eventListeners.clear();
  }

  readonly #onMessage = (event: MessageEvent<unknown>): void => {
    if (isWorkerEvent(event.data)) {
      for (const listener of this.#eventListeners) {
        listener(event.data);
      }
      return;
    }
    if (!isWorkerResponse(event.data)) {
      this.dispose(
        new TinygresError({
          code: 'PROTOCOL_MISMATCH',
          message: 'The Tinygres worker sent an invalid protocol message',
        }),
      );
      return;
    }

    const pending = this.#pending.get(event.data.id);
    if (!pending) {
      return;
    }
    this.#pending.delete(event.data.id);
    if (event.data.ok) {
      pending.resolve(event.data.result);
    } else {
      pending.reject(new TinygresError(event.data.error));
    }
  };

  readonly #onMessageError = (): void => {
    this.dispose(
      new TinygresError({
        code: 'WORKER_MESSAGE_ERROR',
        message: 'The browser could not deserialize a Tinygres worker message',
      }),
    );
  };

  readonly #onError = (event: ErrorEvent): void => {
    this.dispose(
      new TinygresError({
        code: 'WORKER_ERROR',
        message: event.message || 'The Tinygres worker crashed',
      }),
    );
  };
}
