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
