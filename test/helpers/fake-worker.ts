import type {
  WorkerEvent,
  WorkerRequest,
  WorkerResponse,
} from '../../src/protocol.ts';
import type {WorkerLike} from '../../src/client/client.ts';

export class FakeWorker implements WorkerLike {
  readonly posted: unknown[] = [];
  terminated = false;
  onPost?: (message: WorkerRequest) => void;

  readonly #messageListeners = new Set<
    (event: MessageEvent<unknown>) => void
  >();
  readonly #messageErrorListeners = new Set<
    (event: MessageEvent<unknown>) => void
  >();
  readonly #errorListeners = new Set<(event: ErrorEvent) => void>();

  postMessage(message: unknown): void {
    this.posted.push(message);
    this.onPost?.(message as WorkerRequest);
  }

  addEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  addEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  addEventListener(
    type: 'error',
    listener: (event: ErrorEvent) => void,
  ): void;
  addEventListener(
    type: 'message' | 'messageerror' | 'error',
    listener:
      | ((event: MessageEvent<unknown>) => void)
      | ((event: ErrorEvent) => void),
  ): void {
    if (type === 'message') {
      this.#messageListeners.add(
        listener as (event: MessageEvent<unknown>) => void,
      );
    } else if (type === 'messageerror') {
      this.#messageErrorListeners.add(
        listener as (event: MessageEvent<unknown>) => void,
      );
    } else {
      this.#errorListeners.add(listener as (event: ErrorEvent) => void);
    }
  }

  removeEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(
    type: 'error',
    listener: (event: ErrorEvent) => void,
  ): void;
  removeEventListener(
    type: 'message' | 'messageerror' | 'error',
    listener:
      | ((event: MessageEvent<unknown>) => void)
      | ((event: ErrorEvent) => void),
  ): void {
    if (type === 'message') {
      this.#messageListeners.delete(
        listener as (event: MessageEvent<unknown>) => void,
      );
    } else if (type === 'messageerror') {
      this.#messageErrorListeners.delete(
        listener as (event: MessageEvent<unknown>) => void,
      );
    } else {
      this.#errorListeners.delete(listener as (event: ErrorEvent) => void);
    }
  }

  terminate(): void {
    this.terminated = true;
  }

  respond(message: WorkerResponse | WorkerEvent): void {
    const event = {data: message} as MessageEvent<unknown>;
    for (const listener of this.#messageListeners) {
      listener(event);
    }
  }

  emitInvalidMessage(message: unknown): void {
    const event = {data: message} as MessageEvent<unknown>;
    for (const listener of this.#messageListeners) {
      listener(event);
    }
  }

  emitMessageError(): void {
    const event = {data: undefined} as MessageEvent<unknown>;
    for (const listener of this.#messageErrorListeners) {
      listener(event);
    }
  }

  emitError(message: string): void {
    const event = {message} as ErrorEvent;
    for (const listener of this.#errorListeners) {
      listener(event);
    }
  }
}
