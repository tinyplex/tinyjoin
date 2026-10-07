import {
  arrayIsArray,
  isCountWithin,
  isFiniteNumber,
  isNumber,
  isRecord,
  isSafeInteger,
  isString,
  isUndefined,
  MAX_U32,
  objKeys,
} from '../common.js';
import type {JsonValue} from '../protocol.js';

// The tags of a JSON value in a request. The scalars' tags are also those of
// the values in a statement result's header.
export const JSON_NULL = 0;
export const JSON_FALSE = 1;
export const JSON_TRUE = 2;
export const JSON_I64 = 3;
export const JSON_F64 = 5;
export const JSON_STRING = 6;
const JSON_ARRAY = 7;
const JSON_OBJECT = 8;

const MAX_BYTES = 16 * 1024 * 1024;
const MAX_NODES = 1_000_000;
const MAX_OPERATIONS = 1_000_000;
const MAX_DEPTH = 64;
const MAX_SAFE_INTEGER = Number.MAX_SAFE_INTEGER;

// Deterministic estimates aligned with the wasm32 retained model. They bound
// work and allocation independently of a particular JavaScript engine's RSS.
const STRING_OVERHEAD = 12;
const VECTOR_OVERHEAD = 12;
const RUST_VALUE_BYTES = 24;
const RUST_MAP_ENTRY_OVERHEAD = 128;

export class WasmBridgeError extends Error {
  readonly code: string;
  readonly retryable?: boolean;

  constructor(code: string, message: string, retryable?: boolean) {
    super(message);
    this.name = 'WasmBridgeError';
    this.code = code;
    if (retryable !== undefined) {
      this.retryable = retryable;
    }
  }
}

/** The request of an operation that takes no arguments. */
export const EMPTY_REQUEST = new Uint8Array(0);

/*
 * A request is written as WASM reads it, and checked and bounded on the way.
 *
 * A number is little-endian, a string is its UTF-8 length as a u32 and then
 * its bytes, a list is its length as a u32 and then its items, and a JSON
 * value is a tag byte and then its content: a number's eight bytes as a float,
 * or a string, or a list of values, or a list of keys each followed by its
 * value. Two quantities are tracked at once: the bytes of the request, and the
 * memory the Rust side would retain to hold it. Both are bounded, so a request
 * that cannot be served is rejected before it reaches WASM rather than after
 * it has allocated.
 *
 * The request being written is this module's own state, rather than an object
 * made for each: one buffer serves every request, since each is written, and
 * then copied into WASM, before the next begins. A statement is written for
 * every statement a page runs, mostly while this code is still cold, when
 * each call and each property read costs what dozens of them cost later. So
 * the path a statement of scalar parameters takes checks what it must where
 * it stands, and calls nothing it need not.
 */
const INITIAL_BUFFER_BYTES = 1024;
const MAX_KEPT_BUFFER_BYTES = 1024 * 1024;
const MAX_COPIED_STRING_LENGTH = 64;
let buffer = new Uint8Array(INITIAL_BUFFER_BYTES);
let view = new DataView(buffer.buffer);
// How many bytes the buffer has room for, and how many of them are written.
let capacity = INITIAL_BUFFER_BYTES;
let length = 0;
// What the Rust side would retain, and the values and steps the request takes.
let retained = 0;
let nodes = 0;
let operations = 0;
const textEncoder = new TextEncoder();

/** Begins a request, of which `written` bytes are already in the buffer. */
const begin = (written: number): void => {
  length = written;
  retained = nodes = operations = 0;
};

/**
 * The request written, which stays valid only until the next is begun. A
 * buffer grown for an unusually large request is let go afterwards.
 */
const finish = (): Uint8Array => {
  const written = buffer.subarray(0, length);
  if (capacity > MAX_KEPT_BUFFER_BYTES) {
    buffer = new Uint8Array(INITIAL_BUFFER_BYTES);
    view = new DataView(buffer.buffer);
    capacity = INITIAL_BUFFER_BYTES;
  }
  return written;
};

/**
 * Makes room for a request of `needed` bytes in all, which is more than the
 * buffer has. It never grows past the bound on a request, so that bytes
 * within its room are within the bound too.
 */
const grow = (needed: number): void => {
  if (needed > MAX_BYTES) {
    throw resourceLimit();
  }
  const grown = new Uint8Array(
    Math.min(MAX_BYTES, Math.max(needed, capacity * 2)),
  );
  grown.set(buffer.subarray(0, length));
  buffer = grown;
  view = new DataView(grown.buffer);
  // Counted only once the room is there, so that a buffer which could not be
  // made leaves the room as it was.
  capacity = grown.length;
};

// Each of these writes only after making room: making room can replace the
// buffer and its view, which an expression naming them first would still hold.
const u8 = (value: number): void => {
  if (length >= capacity) {
    grow(length + 1);
  }
  buffer[length++] = value;
};

/** A count or an identifier that its caller has checked. */
const u32 = (value: number): void => {
  if (length + 4 > capacity) {
    grow(length + 4);
  }
  view.setUint32(length, value, true);
  length += 4;
};

const string = (value: string): void => {
  if (typeof value !== 'string') {
    throw invalidBridgeValue('A bridge string must be a string');
  }
  const units = value.length;
  // Its length is written once its bytes are counted, in the four before them.
  const start = length + 4;
  // UTF-8 takes at most three bytes for each UTF-16 unit, and TextEncoder
  // replaces a lone surrogate with U+FFFD, which takes three too. Only a
  // string that might not fit is measured exactly first.
  const room = start + units * 3 > MAX_BYTES ? utf8Length(value) : units * 3;
  if (start + room > capacity) {
    grow(start + room);
  }
  // A short ASCII string, as most parameters are, is copied a unit at a
  // time: a browser's TextEncoder call costs more than the copy. Any other
  // is encoded whole.
  let written = 0;
  if (units <= MAX_COPIED_STRING_LENGTH) {
    for (; written < units; written++) {
      const code = value.charCodeAt(written);
      if (code >= 0x80) {
        break;
      }
      buffer[start + written] = code;
    }
  }
  if (written !== units) {
    written = textEncoder.encodeInto(
      value,
      buffer.subarray(start, start + room),
    ).written;
  }
  view.setUint32(length, written, true);
  length = start + written;
  if ((retained += written + STRING_OVERHEAD) > MAX_BYTES) {
    throw resourceLimit();
  }
};

const retain = (count: number): void => {
  if ((retained += count) > MAX_BYTES) {
    throw resourceLimit();
  }
};

export const encodePrepareSql = (sql: string): Uint8Array => {
  begin(0);
  string(sql);
  return finish();
};

export const encodeClosePrepared = (statementId: number): Uint8Array => {
  if (!isCountWithin(statementId, 1, MAX_U32)) {
    throw invalidStatementId();
  }
  begin(0);
  u32(statementId);
  return finish();
};

export const encodeSetSchema = (schema: JsonValue, drop: boolean): Uint8Array => {
  buffer[0] = drop ? 1 : 0;
  begin(1);
  writeJsonValues([schema], 0, 'schema');
  return finish();
};

export const encodeExecSql = (sql: string, arrayRows: boolean): Uint8Array => {
  buffer[0] = arrayRows ? 1 : 0;
  begin(1);
  string(sql);
  return finish();
};

/**
 * Writes a statement's request: whether its rows are to be arrays, the
 * statement, which is the number it was prepared under if `prepared` and its
 * SQL text if not, and its parameters, the values of `input` from `from` on,
 * as a list. Every check and bound is applied to those values alone, as if
 * they had been sliced out of `input` first.
 *
 * Nearly every parameter is a scalar, which is written where it is met, with
 * the checks that {@link writeJsonValue} makes of it made in place. The first
 * value of any other kind, and every value after it, is written as that
 * writes any value, so the parameters are walked once whatever they hold.
 *
 * One function writes both kinds of statement, and all of a request but its
 * strings, because of what the build makes of a function with a single
 * caller: a closure, made anew wherever it is called, which for a part of
 * this would be once for every statement.
 */
export const encodeStatement = (
  prepared: boolean,
  statement: string | number,
  input: unknown,
  arrayRows: boolean,
  from = 0,
): Uint8Array => {
  buffer[0] = arrayRows ? 1 : 0;
  retained = 0;
  if (prepared) {
    // A nonzero unsigned 32-bit integer, checked where it stands.
    if (
      !(
        typeof statement === 'number' &&
        statement >= 1 &&
        statement <= MAX_U32 &&
        statement % 1 === 0
      )
    ) {
      throw invalidStatementId();
    }
    view.setUint32(1, statement, true);
    length = 5;
  } else {
    length = 1;
    string(statement as string);
  }
  if (!isArray(input)) {
    throw invalidBridgeValue('SQL parameters must be an array');
  }
  const end = input.length;
  const count = from < end ? end - from : 0;
  if (
    count > MAX_OPERATIONS ||
    (retained += count * RUST_VALUE_BYTES + VECTOR_OVERHEAD) > MAX_BYTES
  ) {
    throw resourceLimit();
  }
  if (length + 4 > capacity) {
    grow(length + 4);
  }
  view.setUint32(length, count, true);
  length += 4;
  for (let index = from; index < end; index++) {
    const item: unknown = input[index];
    // The difference of a number from itself is zero only when it is finite.
    if (typeof item === 'number' && item - item === 0) {
      if (length + 9 > capacity) {
        grow(length + 9);
      }
      // An integer that Rust can hold exactly is written as one.
      buffer[length] =
        item % 1 === 0 && item <= MAX_SAFE_INTEGER && item >= -MAX_SAFE_INTEGER
          ? JSON_I64
          : JSON_F64;
      view.setFloat64(length + 1, item, true);
      length += 9;
    } else if (typeof item === 'string') {
      if (length >= capacity) {
        grow(length + 1);
      }
      buffer[length++] = JSON_STRING;
      string(item);
    } else if (item === null || item === true || item === false) {
      if (length >= capacity) {
        grow(length + 1);
      }
      buffer[length++] =
        item === null ? JSON_NULL : item ? JSON_TRUE : JSON_FALSE;
    } else {
      // Each scalar before it was a value and a step, which are counted only
      // now that something may add enough to them to pass their bounds: the
      // parameters alone are too few to.
      nodes = operations = index - from;
      for (; index < end; index++) {
        writeListItem(input[index], 0, 'SQL parameters');
      }
    }
  }
  return finish();
};

/** Writes a list of JSON values: a nested array's, or a schema alone. */
const writeJsonValues = (input: unknown[], depth: number, label: string): void => {
  const count = input.length;
  if (count > MAX_OPERATIONS) {
    throw resourceLimit();
  }
  retain(count * RUST_VALUE_BYTES + VECTOR_OVERHEAD);
  u32(count);
  for (let index = 0; index < count; index++) {
    writeListItem(input[index], depth, label);
  }
};

const writeListItem = (item: unknown, depth: number, label: string): void => {
  if (isUndefined(item)) {
    throw invalidBridgeValue(`${label} cannot be sparse`);
  }
  writeJsonValue(item, depth);
};

const writeJsonValue = (value: unknown, depth: number): void => {
  operations = checkedIncrement(operations, MAX_OPERATIONS);
  if (depth > MAX_DEPTH) {
    throw invalidBridgeValue('A bridge value is too deeply nested');
  }
  nodes = checkedIncrement(nodes, MAX_NODES);
  if (value === null) {
    u8(JSON_NULL);
  } else if (value === false) {
    u8(JSON_FALSE);
  } else if (value === true) {
    u8(JSON_TRUE);
  } else if (isNumber(value)) {
    if (!isFiniteNumber(value)) {
      throw invalidBridgeValue('A JSON number must be finite');
    }
    u8(isSafeInteger(value) ? JSON_I64 : JSON_F64);
    if (length + 8 > capacity) {
      grow(length + 8);
    }
    view.setFloat64(length, value, true);
    length += 8;
  } else if (isString(value)) {
    u8(JSON_STRING);
    string(value);
  } else if (isArray(value)) {
    u8(JSON_ARRAY);
    writeJsonValues(value, depth + 1, 'JSON array');
  } else if (isRecord(value)) {
    u8(JSON_OBJECT);
    const keys = objKeys(value);
    if (keys.length > MAX_OPERATIONS) {
      throw resourceLimit();
    }
    u32(keys.length);
    for (const key of keys) {
      string(key);
      retain(RUST_VALUE_BYTES + RUST_MAP_ENTRY_OVERHEAD);
      writeJsonValue(value[key], depth + 1);
    }
  } else {
    throw invalidBridgeValue('A value is not JSON-compatible');
  }
};

// Every request reaches the writer as plain data: a structured clone that the
// protocol check accepted, or values the Worker built itself. Structured
// cloning leaves no accessors or proxies, so properties are read directly.
// Array.isArray is still read through a try/catch, since a revoked proxy
// throws from it.
const isArray = (value: unknown): value is unknown[] => {
  try {
    return arrayIsArray(value);
  } catch {
    throw invalidBridgeValue('A bridge array could not be inspected');
  }
};

const checkedIncrement = (value: number, maximum: number): number => {
  if (value >= maximum) {
    throw resourceLimit();
  }
  return value + 1;
};

const utf8Length = (value: string): number => {
  let bytes = 0;
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code < 0x80) {
      bytes += 1;
    } else if (code < 0x800) {
      bytes += 2;
    } else if (
      code >= 0xd800 &&
      code <= 0xdbff &&
      index + 1 < value.length &&
      value.charCodeAt(index + 1) >= 0xdc00 &&
      value.charCodeAt(index + 1) <= 0xdfff
    ) {
      bytes += 4;
      index += 1;
    } else {
      // TextEncoder replaces lone surrogates with U+FFFD (three bytes).
      bytes += 3;
    }
    if (bytes > MAX_BYTES) {
      throw resourceLimit();
    }
  }
  return bytes;
};

const invalidBridgeValue = (message: string): WasmBridgeError =>
  new WasmBridgeError('INVALID_BRIDGE_VALUE', message);

const invalidStatementId = (): WasmBridgeError =>
  invalidBridgeValue(
    'A prepared statement ID must be a nonzero unsigned 32-bit integer',
  );

const resourceLimit = (): WasmBridgeError =>
  new WasmBridgeError('RESOURCE_LIMIT', 'A bridge call exceeded its resource limit');
