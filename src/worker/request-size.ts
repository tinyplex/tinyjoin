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
 * {@link requestBytes} for a value a protocol check has already accepted: plain
 * data from a structured clone or a literal, whose containers are plain objects
 * and dense arrays. It charges exactly what {@link requestBytes} charges, but
 * reads properties directly, since the check that accepted them already read
 * every one, and falls back to {@link requestBytes} for anything else.
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
      if (Array.isArray(next)) {
        const length = next.length;
        add(length * 8);
        // A dense array's indices come first, in order, then any named
        // properties.
        let index = 0;
        for (const key in next) {
          if (bytes > limit || unchecked) return;
          if (index < length) {
            if (key !== String(index)) {
              unchecked = true;
              return;
            }
            visit(next[index++]);
          } else if (Object.hasOwn(next, key)) {
            add(24 + key.length * 2);
            visit((next as unknown as Record<string, unknown>)[key]);
          }
        }
        if (index < length) unchecked = true;
      } else {
        for (const key in next) {
          if (bytes > limit || unchecked) return;
          if (Object.hasOwn(next, key)) {
            add(24 + key.length * 2);
            visit((next as Record<string, unknown>)[key]);
          }
        }
      }
    }
  };

  visit(value);
  return unchecked ? requestBytes(value, limit) : bytes;
};
