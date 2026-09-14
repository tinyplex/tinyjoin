import type {WorkerLike} from './client/client.js';

const assertWorkerAvailable = (): void => {
  if (typeof Worker === 'undefined') {
    throw new Error(
      'TinyJoin requires a browser Worker. Importing is SSR-safe, but create the client in the browser or provide a Worker-like implementation.',
    );
  }
};

/** Constructs a Worker from a URL the application supplied. */
export const createUrlWorker = (url: string | URL): WorkerLike => {
  assertWorkerAvailable();
  return new Worker(url, {name: 'tinyjoin', type: 'module'});
};

/**
 * Constructs the Worker that TinyJoin ships.
 *
 * Both the shape and the location of this call matter. An application's bundler
 * recognizes `new Worker(new URL('...', import.meta.url), {type: 'module'})`
 * literally, and only emits a Worker entry when it sees that exact pattern
 * spelled out, so none of it may move into a variable. The specifier is
 * resolved against this module, which lives at the root of the package for
 * that reason: the published client is one bundle at this same depth, so the
 * relative path is correct in the source tree and in the bundle alike.
 */
export const createDefaultWorker = (refreshOnResume = false): WorkerLike => {
  assertWorkerAvailable();
  const worker = new Worker(
    new URL('./worker/default-entry.js', import.meta.url),
    {
      name: 'tinyjoin',
      type: 'module',
    },
  );
  if (!refreshOnResume || typeof document === 'undefined') return worker;
  const refresh = (): void => {
    if (document.visibilityState === 'visible')
      worker.postMessage({tinyjoin: 'resync'});
  };
  window.addEventListener('pageshow', refresh);
  document.addEventListener('resume', refresh);
  document.addEventListener('visibilitychange', refresh);
  return {
    postMessage: (message) => worker.postMessage(message),
    addEventListener: (type, listener) =>
      worker.addEventListener(type, listener as EventListener),
    removeEventListener: (type, listener) =>
      worker.removeEventListener(type, listener as EventListener),
    terminate: () => {
      window.removeEventListener('pageshow', refresh);
      document.removeEventListener('resume', refresh);
      document.removeEventListener('visibilitychange', refresh);
      worker.terminate();
    },
  };
};
