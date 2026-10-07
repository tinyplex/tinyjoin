/**
 * Runs a scenario, and returns how many reactions it attached to promises by
 * calling then(), which catch() and finally() call in turn.
 *
 * An `await` attaches its reaction without calling then(), so what is counted
 * is what the code under test chose to attach. The count is of the scenario
 * alone, which must therefore do its work before it returns.
 */
export const reactions = (scenario: () => void): number => {
  const then = Promise.prototype.then;
  let attached = 0;
  Promise.prototype.then = function (
    this: Promise<unknown>,
    ...handlers: unknown[]
  ): Promise<unknown> {
    attached++;
    return Reflect.apply(then, this, handlers) as Promise<unknown>;
  } as typeof then;
  try {
    scenario();
  } finally {
    Promise.prototype.then = then;
  }
  return attached;
};
