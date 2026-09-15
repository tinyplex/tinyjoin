import {Worker} from 'node:worker_threads';
import type {WorkerLike} from '../client/client.js';

/** Adapts Node's worker events to the existing client transport. */
export const createNodeWorker = (): WorkerLike => {
  const worker = new Worker(new URL('./worker-entry.js', import.meta.url), {
    name: 'tinyjoin',
    // --input-type applies to the parent's stdin/eval source, not this file.
    execArgv: process.execArgv.filter(
      (argument, index, arguments_) =>
        argument !== '--input-type' &&
        !argument.startsWith('--input-type=') &&
        arguments_[index - 1] !== '--input-type',
    ),
  });
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
