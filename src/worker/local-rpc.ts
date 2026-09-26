import {objFreeze} from '../common.js';
import {workerTerminated, type WorkerRpc} from '../client/rpc.js';
import {
  PROTOCOL_VERSION,
  type RpcMethod,
  type RpcMethods,
  type WorkerEvent,
  type WorkerRequest,
} from '../protocol.js';
import {startWorker, type Served} from './host.js';

/** A connection to an engine host in the same Worker. */
export interface LocalRpc extends WorkerRpc {
  /**
   * Serves a request at once, as the host's requestNow() does, or returns
   * `undefined`, having done nothing, when it must wait its turn.
   */
  requestNow<Method extends RpcMethod>(
    method: Method,
    params: RpcMethods[Method]['request'],
  ): Served | undefined;
}

/**
 * Connects the coordinator to an engine host in the same Worker. A request is
 * a call rather than a message, and neither side validates what the other
 * built: every request from outside the Worker was validated as it arrived.
 */
export const createLocalRpc = (): LocalRpc => {
  const listeners = new Set<(event: WorkerEvent) => void>();
  let disposed = false;
  let nextId = 1;
  // The host posts only its change events: requests settle directly.
  const host = startWorker({
    scope: {
      postMessage: (message) => {
        for (const listener of listeners) listener(message as WorkerEvent);
      },
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
      close: () => undefined,
    },
  });
  const envelope = (method: RpcMethod, params: unknown): WorkerRequest =>
    ({v: PROTOCOL_VERSION, id: nextId++, method, params}) as WorkerRequest;
  return objFreeze<LocalRpc>({
    request: (method, params) =>
      (disposed
        ? Promise.reject(workerTerminated())
        : host.request(envelope(method, params))) as never,
    requestNow: (method, params) =>
      disposed ? undefined : host.requestNow(envelope(method, params)),
    onEvent: (listener) => {
      listeners.add(listener);
    },
    dispose: () => {
      disposed = true;
      listeners.clear();
    },
  });
};
