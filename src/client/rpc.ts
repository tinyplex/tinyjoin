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
import {ClientError} from './error.js';

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

export type ResultValidation = 'full' | 'header';

type PendingRequest = {
  method: RpcMethod;
  resolve(value: unknown): void;
  reject(error: unknown): void;
};

export class WorkerRpc {
  readonly #worker: WorkerLike;
  readonly #pending = new Map<number, PendingRequest>();
  readonly #eventListeners = new Set<(event: WorkerEvent) => void>();
  readonly #resultValidation: ResultValidation;
  #nextId = 1;
  #disposed = false;

  constructor(worker: WorkerLike, resultValidation: ResultValidation = 'full') {
    this.#worker = worker;
    this.#resultValidation = resultValidation;
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
        new ClientError({
          code: 'WORKER_TERMINATED',
          message: 'The TinyGres worker has been closed',
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
      this.#pending.set(id, {method, resolve, reject});
      try {
        this.#worker.postMessage(request);
      } catch (error) {
        this.#pending.delete(id);
        reject(ClientError.fromUnknown(error, 'WORKER_POST_FAILED'));
      }
    }) as Promise<RpcMethods[Method]['response']>;
  }

  onEvent(listener: (event: WorkerEvent) => void): () => void {
    this.#eventListeners.add(listener);
    return () => this.#eventListeners.delete(listener);
  }

  dispose(error?: ClientError): void {
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
      new ClientError({
        code: 'WORKER_TERMINATED',
        message: 'The TinyGres worker has been closed',
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
        new ClientError({
          code: 'PROTOCOL_MISMATCH',
          message: 'The TinyGres worker sent an invalid protocol message',
        }),
      );
      return;
    }

    const pending = this.#pending.get(event.data.id);
    if (!pending) {
      return;
    }
    if (event.data.ok) {
      const validResult =
        this.#resultValidation === 'full'
          ? isRpcResult(pending.method, event.data.result)
          : isRpcResultHeader(pending.method, event.data.result);
      if (!validResult) {
        this.dispose(
          new ClientError({
            code: 'PROTOCOL_MISMATCH',
            message:
              'The TinyGres worker returned an invalid result for the requested operation',
          }),
        );
        return;
      }
      this.#pending.delete(event.data.id);
      pending.resolve(event.data.result);
    } else {
      this.#pending.delete(event.data.id);
      pending.reject(new ClientError(event.data.error));
    }
  };

  readonly #onMessageError = (): void => {
    this.dispose(
      new ClientError({
        code: 'WORKER_MESSAGE_ERROR',
        message: 'The browser could not deserialize a TinyGres worker message',
      }),
    );
  };

  readonly #onError = (event: ErrorEvent): void => {
    this.dispose(
      new ClientError({
        code: 'WORKER_ERROR',
        message: event.message || 'The TinyGres worker crashed',
      }),
    );
  };
}
