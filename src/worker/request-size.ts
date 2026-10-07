import {STATEMENT_PARAMS, type StatementRequest} from '../protocol.js';

/**
 * Estimates retained JSON-like request data without serializing or expanding it.
 * Container identity is charged once; each property/array slot and string value
 * is charged conservatively. Returns limit + 1 as soon as the budget is exceeded.
 */
export const requestBytes = (value: unknown, limit: number): number => {
  if (
    !Number.isSafeInteger(limit) ||
    limit < 0 ||
    limit >= Number.MAX_SAFE_INTEGER
  ) {
    throw new RangeError(
      'A request byte limit must be a non-negative safe integer',
    );
  }
  const overflow = limit + 1;
  const seen = new WeakSet<object>();
  const pending: object[] = [];
  let bytes = 0;

  const add = (amount: number): void => {
    bytes = amount > limit - bytes ? overflow : bytes + amount;
  };
  const visit = (next: unknown): void => {
    if (bytes > limit) return;
    if (
      next === null ||
      next === undefined ||
      typeof next === 'boolean' ||
      typeof next === 'number'
    ) {
      add(8);
    } else if (typeof next === 'string') {
      add(16 + next.length * 2);
    } else if (typeof next === 'object') {
      if (seen.has(next)) return;
      seen.add(next);
      add(32);
      if (bytes <= limit) pending.push(next);
    } else {
      // Functions, symbols, and bigints are outside TinyJoin's request contract.
      bytes = overflow;
    }
  };

  visit(value);
  while (pending.length > 0 && bytes <= limit) {
    const current = pending.pop()!;
    const array = Array.isArray(current);
    if (array) {
      // Charge capacity before walking. Sparse arrays and many references to one
      // child still occupy slots, without allocating a list of all array keys.
      add(current.length * 8);
      if (bytes > limit) break;
    }
    for (const key in current) {
      if (!Object.hasOwn(current, key)) continue;
      const index = array ? Number(key) : -1;
      const arraySlot =
        array &&
        Number.isInteger(index) &&
        index >= 0 &&
        index < current.length &&
        String(index) === key;
      if (!arraySlot) add(24 + key.length * 2);
      if (bytes > limit) break;
      const descriptor = Object.getOwnPropertyDescriptor(current, key);
      if (!descriptor || !('value' in descriptor)) {
        // Never run caller getters while checking the budget.
        bytes = overflow;
        break;
      }
      visit(descriptor.value);
      if (bytes > limit) break;
    }
  }
  return bytes;
};

/**
 * What {@link checkedRequestBytes} charges the request that a statement
 * request stands for, computed from the array as it arrived, so that the
 * request's object need not be built to be measured, nor walked.
 *
 * Everything but the parameters has a fixed charge: the request object with
 * its four keys, its two numbers and its method's name; the parameters object
 * with its `params` key; `sql` and the text or `statementId` and its number;
 * `transactionId` and the id inside a transaction; and `rowMode` and `array`
 * for array rows. The parameters are then charged as their array would be. A
 * parameter that is not a scalar sends them all through the walk, which
 * charges a container that several of them share once, as it would there.
 */
export const statementBytes = (
  request: StatementRequest,
  limit: number,
): number => {
  const length = request.length;
  const target = request[3];
  const transaction = request[4];
  const fixed =
    (typeof target === 'string' ? 324 + target.length * 2 : 342) +
    (transaction === 0 ? 0 : 66 + transaction.length * 2) +
    (request[5] === 1 ? 64 : 0);
  let bytes = fixed + 32 + (length - STATEMENT_PARAMS) * 8;
  for (let index = STATEMENT_PARAMS; index < length; index++) {
    const value = request[index];
    if (typeof value === 'string') bytes += 16 + value.length * 2;
    else if (
      value === null ||
      typeof value === 'boolean' ||
      typeof value === 'number'
    )
      bytes += 8;
    else
      return fixed > limit
        ? limit + 1
        : fixed +
            checkedRequestBytes(request.slice(STATEMENT_PARAMS), limit - fixed);
  }
  return bytes > limit ? limit + 1 : bytes;
};

/**
 * {@link requestBytes} for a value a protocol check has already accepted: plain
 * data from a structured clone or a literal, whose containers are plain objects
 * and dense arrays. It charges exactly what {@link requestBytes} charges, but
 * reads properties directly, since the check that accepted them already read
 * every one, and falls back to {@link requestBytes} for anything else. Such a
 * container's own properties are exactly its enumerable ones, which
 * Object.keys lists far faster than a for-in loop visits them.
 */
export const checkedRequestBytes = (value: unknown, limit: number): number => {
  if (
    !Number.isSafeInteger(limit) ||
    limit < 0 ||
    limit >= Number.MAX_SAFE_INTEGER
  ) {
    throw new RangeError(
      'A request byte limit must be a non-negative safe integer',
    );
  }
  const overflow = limit + 1;
  // A request holds a few containers, which a short list tells apart without
  // allocating a WeakSet.
  const seen: object[] = [];
  let bytes = 0;
  let unchecked = false;

  const add = (amount: number): void => {
    bytes = amount > limit - bytes ? overflow : bytes + amount;
  };
  const visit = (next: unknown): void => {
    if (bytes > limit || unchecked) return;
    if (
      next === null ||
      next === undefined ||
      typeof next === 'boolean' ||
      typeof next === 'number'
    ) {
      add(8);
    } else if (typeof next === 'string') {
      add(16 + next.length * 2);
    } else if (typeof next !== 'object' || seen.length === 64) {
      unchecked = true;
    } else if (!seen.includes(next)) {
      seen.push(next);
      add(32);
      const keys = Object.keys(next);
      let named = 0;
      if (Array.isArray(next)) {
        const length = next.length;
        add(length * 8);
        // An array's indices come first, in ascending order, and then its named
        // properties. So a dense one's first `length` keys are its indices
        // exactly when the last of them is.
        if (length > 0 && keys[length - 1] !== String(length - 1)) {
          unchecked = true;
          return;
        }
        for (let index = 0; index < length; index++) {
          if (bytes > limit || unchecked) return;
          visit(next[index]);
        }
        named = length;
      }
      for (; named < keys.length; named++) {
        if (bytes > limit || unchecked) return;
        const key = keys[named]!;
        add(24 + key.length * 2);
        visit((next as Record<string, unknown>)[key]);
      }
    }
  };

  visit(value);
  return unchecked ? requestBytes(value, limit) : bytes;
};
