import type {WorkerLike} from '../client/client.js';
import {createWorkerRpc, type WorkerRpc} from '../client/rpc.js';
import {startWorker, type WorkerScope} from './host.js';

/** Connects the coordinator to the engine host without another Worker or clone. */
export const createLocalRpc = (): WorkerRpc => {
  const listeners = new Set<(event: MessageEvent<unknown>) => void>();
  let receive: ((event: MessageEvent<unknown>) => void) | undefined;
  const scope: WorkerScope = {
    postMessage: (data) => {
      for (const listener of listeners)
        listener({data} as MessageEvent<unknown>);
    },
    addEventListener: (_type, listener) => {
      receive = listener;
    },
    removeEventListener: () => {
      receive = undefined;
    },
    close: () => undefined,
  };
  const worker: WorkerLike = {
    postMessage: (data) => receive?.({data} as MessageEvent<unknown>),
    addEventListener: (type, listener) => {
      if (type === 'message')
        listeners.add(listener as (event: MessageEvent<unknown>) => void);
    },
    removeEventListener: (type, listener) => {
      if (type === 'message')
        listeners.delete(listener as (event: MessageEvent<unknown>) => void);
    },
  };
  startWorker({scope});
  return createWorkerRpc(worker, 'header');
};
