import {startWorker as startWorkerHost} from './host.js';

/** Starts TinyJoin in the current dedicated Worker. */
export function startWorker(): void {
  startWorkerHost();
}
