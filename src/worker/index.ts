import {startCoordinatedWorker} from './coordinator.js';

/** Starts TinyJoin in the current dedicated Worker. */
export const startWorker = (): void => {
  startCoordinatedWorker();
};
