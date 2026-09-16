import {Worker} from 'node:worker_threads';
import {isRecord} from '../common.js';
import type {WorkerLike} from '../client/client.js';

/**
 * Starts the worker thread, inheriting the parent's flags where Node allows it.
 *
 * Node accepts only a subset of command-line flags for a worker, and rejects the whole list if
 * any one of them is per-process only. Test runners and profilers routinely set such flags, so a
 * rejected list falls back to no inherited flags rather than failing to open a database at all.
 */
const startWorker = (): Worker => {
  const url = new URL('./worker-entry.js', import.meta.url);
  // --input-type applies to the parent's stdin/eval source, not this file.
  const execArgv = process.execArgv.filter(
    (argument, index, arguments_) =>
      argument !== '--input-type' &&
      !argument.startsWith('--input-type=') &&
      arguments_[index - 1] !== '--input-type',
  );
  try {
    return new Worker(url, {name: 'tinyjoin', execArgv});
  } catch (error) {
    if (
      !isRecord(error) ||
      error.code !== 'ERR_WORKER_INVALID_EXEC_ARGV'
    ) {
      throw error;
    }
    return new Worker(url, {name: 'tinyjoin', execArgv: []});
  }
};

/** Adapts Node's worker events to the existing client transport. */
export const createNodeWorker = (): WorkerLike => {
  const worker = startWorker();
  const events = new EventTarget();
  let terminated = false;
  const fail = (message: string): void => {
    events.dispatchEvent(Object.assign(new Event('error'), {message}));
  };
  worker.on('message', (data: unknown) => {
    events.dispatchEvent(new MessageEvent('message', {data}));
  });
  worker.on('messageerror', () => {
    events.dispatchEvent(new MessageEvent('messageerror'));
  });
  worker.on('error', (error) =>
    fail(error instanceof Error ? error.message : String(error)),
  );
  // A thread can exit without throwing. Reject pending requests in that case
  // too; otherwise an initialization or query could wait forever.
  worker.on('exit', (code) => {
    if (!terminated) fail(`The TinyJoin worker exited unexpectedly (${code})`);
  });
  return {
    postMessage: (message) => worker.postMessage(message),
    addEventListener: (type, listener) =>
      events.addEventListener(type, listener as EventListener),
    removeEventListener: (type, listener) =>
      events.removeEventListener(type, listener as EventListener),
    terminate: () => {
      if (!terminated) {
        terminated = true;
        void worker.terminate();
      }
    },
  };
};
