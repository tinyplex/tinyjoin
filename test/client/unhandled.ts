import {onTestFinished} from 'vitest';

/**
 * Runs a scenario, and returns the reasons of the promise rejections it left
 * for nothing to handle.
 *
 * Node reports such a rejection once the microtask queue has drained. The test
 * runner listens for them too, and fails the run for any, so its listeners
 * stand aside while a test raises one on purpose. They are put back when the
 * scenario ends, and again when its test does, in case the scenario never
 * ended.
 */
export const unhandledRejections = async (
  scenario: () => void | Promise<void>,
): Promise<unknown[]> => {
  const listeners = process.listeners('unhandledRejection');
  const unhandled: unknown[] = [];
  const restore = (): void => {
    process.removeAllListeners('unhandledRejection');
    for (const listener of listeners) {
      process.on('unhandledRejection', listener);
    }
  };
  onTestFinished(restore);
  process.removeAllListeners('unhandledRejection');
  process.on('unhandledRejection', (reason) => {
    unhandled.push(reason);
  });
  try {
    await scenario();
    await new Promise((resolve) => setTimeout(resolve, 0));
  } finally {
    restore();
  }
  return unhandled;
};
