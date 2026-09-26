import {objFreeze} from '../common.js';
import {workerTerminated, type WorkerRpc} from '../client/rpc.js';
import {
  PROTOCOL_VERSION,
  type WorkerEvent,
  type WorkerRequest,
} from '../protocol.js';
import {startWorker} from './host.js';

/**
 * Connects the coordinator to an engine host in the same Worker. A request is
 * a call rather than a message, and neither side validates what the other
 * built: every request from outside the Worker was validated as it arrived.
 */
export const createLocalRpc = (): WorkerRpc => {
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
  return objFreeze<WorkerRpc>({
    request: (method, params) =>
      (disposed
        ? Promise.reject(workerTerminated())
        : host.request({
            v: PROTOCOL_VERSION,
            id: nextId++,
            method,
            params,
          } as WorkerRequest)) as never,
    onEvent: (listener) => {
      listeners.add(listener);
    },
    dispose: () => {
      disposed = true;
      listeners.clear();
    },
  });
};
