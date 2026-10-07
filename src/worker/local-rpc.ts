import {objFreeze} from '../common.js';
import {workerTerminated, type WorkerRpc} from '../client/rpc.js';
import {
  PROTOCOL_VERSION,
  type RpcMethod,
  type RpcMethods,
  type WorkerEvent,
  type WorkerRequest,
} from '../protocol.js';
import {ClientError} from '../client/error.js';
import {
  serializeError,
  startWorker,
  type Served,
  type WorkerController,
} from './host.js';

/**
 * A connection to an engine host in the same Worker. It takes only what its
 * users need of a connection to a Worker: requests, events and disposal.
 */
export interface LocalRpc
  extends Pick<WorkerRpc, 'request' | 'onEvent' | 'dispose'> {
  /**
   * Serves a request at once, as the host's requestNow() does, or returns
   * `undefined`, having done nothing, when it must wait its turn.
   */
  requestNow<Method extends RpcMethod>(
    method: Method,
    params: RpcMethods[Method]['request'],
  ): Served | undefined;
  /**
   * Serves a statement request at once, as the host's statementNow() does: it
   * returns the result, or throws the error a response would carry, as
   * requestNow() returns it, or returns `undefined`, having done nothing, when
   * the statement must wait its turn.
   */
  statementNow: WorkerController['statementNow'];
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
    request: (method, params, read) => {
      if (disposed) {
        return Promise.reject(workerTerminated());
      }
      const served = host.request(envelope(method, params));
      return (
        read ? served.then(read as (value: unknown) => unknown) : served
      ) as never;
    },
    requestNow: (method, params) =>
      disposed ? undefined : host.requestNow(envelope(method, params)),
    statementNow: (request, target, transactionId) => {
      if (disposed) {
        return undefined;
      }
      try {
        return host.statementNow(request, target, transactionId);
      } catch (error) {
        throw new ClientError(serializeError(error));
      }
    },
    onEvent: (listener) => {
      listeners.add(listener);
    },
    dispose: () => {
      disposed = true;
      listeners.clear();
    },
  });
};
