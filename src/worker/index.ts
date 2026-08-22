import {startWorker as startWorkerHost} from './host.js';

/** Starts TinyGres in the current dedicated Worker. */
export function startWorker(): void {
  startWorkerHost();
}
