/**
 * Small shared helpers, used by the client, the Worker host, and the private
 * OPFS runtime alike.
 *
 * Every one of these is a single expression that the published bundles call
 * many times over. Naming them once lets the minifier collapse each call site
 * to a mangled identifier, where the expression itself would have been spelled
 * out in full every time. The aliases for built-ins earn their keep the same
 * way, and also stop a page from changing TinyJoin's behaviour by replacing a
 * global method after the module has loaded.
 */

import type {Row} from './protocol.js';

export const arrayIsArray = Array.isArray;
export const objFreeze = Object.freeze;
export const objHasOwn = Object.hasOwn;
export const objValues = Object.values;
export const ownKeys = Reflect.ownKeys;
export const mathMax = Math.max;

export const isUndefined = (value: unknown): value is undefined =>
  value === undefined;

export const isBoolean = (value: unknown): value is boolean =>
  typeof value === 'boolean';

export const isNumber = (value: unknown): value is number =>
  typeof value === 'number';

export const isString = (value: unknown): value is string =>
  typeof value === 'string';

export const isFunction = (value: unknown): value is (...args: never[]) => void =>
  typeof value === 'function';

export const isObject = (value: unknown): value is object =>
  typeof value === 'object' && value !== null;

/** Any non-null object that is not an array, whatever its prototype. */
export const isRecord = (value: unknown): value is Record<string, unknown> =>
  isObject(value) && !arrayIsArray(value);

/** A plain object safe to walk as JSON, with only string keys. */
export const isPlainRecord = (
  value: unknown,
): value is Record<string, unknown> => {
  if (!isRecord(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return (
    (prototype === Object.prototype || prototype === null) &&
    ownKeys(value).every(isString)
  );
};

export const isInteger = (value: unknown): value is number =>
  Number.isInteger(value);

export const isSafeInteger = (value: unknown): value is number =>
  Number.isSafeInteger(value);

export const isFiniteNumber = (value: unknown): value is number =>
  Number.isFinite(value);

/** A safe integer that counts something, so zero or more. */
export const isCount = (value: unknown): value is number =>
  isSafeInteger(value) && value >= 0;

/** A safe integer within an inclusive range. */
export const isCountWithin = (
  value: unknown,
  minimum: number,
  maximum: number,
): value is number =>
  isSafeInteger(value) && value >= minimum && value <= maximum;

export const MAX_U32 = 0xffff_ffff;

/** The shape of an error that carried a TinyJoin code across a boundary. */
export type CodedError = {
  readonly code: string;
  readonly message: string;
  readonly retryable?: boolean;
};

/**
 * Reads the code and message off a value that already describes a TinyJoin
 * failure, so that a boundary can pass it on rather than restate it.
 */
export const asCodedError = (value: unknown): CodedError | undefined =>
  isRecord(value) &&
  'code' in value &&
  isString(value.code) &&
  'message' in value &&
  isString(value.message)
    ? {
        code: value.code,
        message: value.message,
        ...('retryable' in value && isBoolean(value.retryable)
          ? {retryable: value.retryable}
          : {}),
      }
    : undefined;

/** The DOMException name a browser used to explain a storage failure. */
export const errorName = (error: unknown): string =>
  isObject(error) && 'name' in error ? String(error.name) : '';

/** The `: reason` to append to a TinyJoin message, when there is one. */
export const errorDetail = (error: unknown): string =>
  error instanceof Error && error.message ? `: ${error.message}` : '';

/**
 * The most changed primary keys one table reports in a single change notification.
 *
 * This mirrors the engine's own bound. Coalescing several writes into one event can push a table
 * past it even when no single write did, so the merge below applies the same rule again.
 */
export const MAX_CHANGED_KEYS_PER_TABLE = 1_000;

/**
 * A running changed-key set. `undefined` marks a table whose keys can no longer be reported in
 * full, either because a write did not report them or because coalescing exceeded the bound.
 */
export type PendingChangedKeys = Map<
  string,
  {rows: Row[]; seen: Set<string>} | undefined
>;

/**
 * Folds one outcome's changed keys into a running set.
 *
 * `tables` is authoritative: a table that changed without reporting keys poisons its entry, so a
 * consumer can read the presence of a table in the finished set as "this is every key that
 * changed". Keys are deduplicated because coalesced writes routinely touch a row more than once.
 */
export const mergeChangedKeys = (
  pending: PendingChangedKeys,
  tables: string[],
  keys: {[table: string]: Row[]},
): void => {
  for (const table of tables) {
    const incoming = keys[table];
    if (!incoming) {
      pending.set(table, undefined);
      continue;
    }
    if (!pending.has(table)) pending.set(table, {rows: [], seen: new Set()});
    const entry = pending.get(table);
    if (!entry) continue;
    for (const key of incoming) {
      if (entry.rows.length >= MAX_CHANGED_KEYS_PER_TABLE) {
        pending.set(table, undefined);
        break;
      }
      const identity = JSON.stringify(key);
      if (entry.seen.has(identity)) continue;
      entry.seen.add(identity);
      entry.rows.push(key);
    }
  }
};

/** Drops the tables that could not report a complete key set, leaving only usable entries. */
export const finishChangedKeys = (pending: PendingChangedKeys): {
  [table: string]: Row[];
} => {
  const keys: {[table: string]: Row[]} = {};
  for (const [table, entry] of pending) if (entry) keys[table] = entry.rows;
  return keys;
};
