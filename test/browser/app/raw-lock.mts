import {createOpfsPageStorageSession} from '../../../src/worker/page-storage.js';

// Bypass election to verify the native sync handle remains the final guard
// against an old release or custom engine that does not coordinate.
self.addEventListener('message', (event: MessageEvent<string>) => {
  void createOpfsPageStorageSession(event.data).then(
    (session) => {
      session.close();
      self.postMessage('');
    },
    (error: unknown) =>
      self.postMessage(
        typeof error === 'object' && error !== null && 'code' in error
          ? String(error.code)
          : String(error),
      ),
  );
});
