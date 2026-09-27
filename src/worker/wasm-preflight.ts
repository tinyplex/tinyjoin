import {
  arrayIsArray,
  isCount,
  isCountWithin,
  isFiniteNumber,
  isNumber,
  isRecord,
  isSafeInteger,
  isString,
  isUndefined,
  MAX_U32,
  objHasOwn,
  ownKeys,
} from '../common.js';
import type {JsonValue} from '../protocol.js';

const JSON_NULL = 0;
const JSON_FALSE = 1;
const JSON_TRUE = 2;
const JSON_I64 = 3;
const JSON_F64 = 5;
const JSON_STRING = 6;
const JSON_ARRAY = 7;
const JSON_OBJECT = 8;

const MAX_BYTES = 16 * 1024 * 1024;
const MAX_NODES = 1_000_000;
const MAX_OPERATIONS = 1_000_000;
const MAX_DEPTH = 64;

// Deterministic estimates aligned with the wasm32 retained model. They bound
// work and allocation independently of a particular JavaScript engine's RSS.
const STRING_OVERHEAD = 12;
const VECTOR_OVERHEAD = 12;
const RUST_VALUE_BYTES = 24;
const RUST_MAP_ENTRY_OVERHEAD = 128;

const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;

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

// The bytes of the request being written. One buffer serves every request,
// since each is written, and then copied into WASM, before the next begins.
const INITIAL_BUFFER_BYTES = 1024;
let buffer = new Uint8Array(INITIAL_BUFFER_BYTES);
let view = new DataView(buffer.buffer);
const textEncoder = new TextEncoder();

/** The request of an operation that takes no arguments. */
export const EMPTY_REQUEST = new Uint8Array(0);

/**
 * Writes one request as WASM reads it, checking and bounding it on the way.
 *
 * A number is little-endian, a string is its UTF-8 length as a u32 and then
 * its bytes, a list is its length as a u32 and then its items, and a JSON
 * value is a tag byte and then its content: a number's eight bytes as a float,
 * or a string, or a list of values, or a list of keys each followed by its
 * value. The writer tracks two quantities at once: the bytes of the request,
 * and the memory the Rust side would retain to hold it. Both are bounded, so
 * a request that cannot be served is rejected before it reaches WASM rather
 * than after it has allocated.
 */
class RequestWriter {
  #length = 0;
  #retained = 0;
  #nodes = 0;
  #operations = 0;

  /** Makes room for `count` more bytes, and returns where they start. */
  reserve(count: number): number {
    const at = this.#length;
    const next = at + count;
    if (!isCount(next) || next > MAX_BYTES) {
      throw resourceLimit();
    }
    if (next > buffer.length) {
      const grown = new Uint8Array(Math.max(next, buffer.length * 2));
      grown.set(buffer.subarray(0, at));
      buffer = grown;
      view = new DataView(buffer.buffer);
    }
    this.#length = next;
    return at;
  }

  // Each writes only after reserving: reserving can replace the buffer and its
  // view, which an expression naming them first would still hold.
  u8(value: number): void {
    assertUint(value, 0xff);
    const at = this.reserve(1);
    buffer[at] = value;
  }

  u32(value: number): void {
    assertUint(value, MAX_U32);
    const at = this.reserve(4);
    view.setUint32(at, value, true);
  }

  /** A finite number, as the eight bytes of a float. */
  number(value: number): void {
    const at = this.reserve(8);
    view.setFloat64(at, value, true);
  }

  string(value: string): void {
    if (!isString(value)) {
      throw invalidBridgeValue('A bridge string must be a string');
    }
    const lengthAt = this.reserve(4);
    const start = this.#length;
    // UTF-8 takes at most three bytes for each UTF-16 unit, and TextEncoder
    // replaces a lone surrogate with U+FFFD, which takes three too. Only a
    // string that might not fit is measured exactly first.
    const room =
      start + value.length * 3 > MAX_BYTES ? utf8Length(value) : value.length * 3;
    this.reserve(room);
    const {written} = textEncoder.encodeInto(
      value,
      buffer.subarray(start, start + room),
    );
    this.#length = start + written;
    view.setUint32(lengthAt, written, true);
    this.retain(written + STRING_OVERHEAD);
  }

  retain(count: number): void {
    if (!isCount(count)) {
      throw resourceLimit();
    }
    const next = this.#retained + count;
    if (!isCount(next) || next > MAX_BYTES) {
      throw resourceLimit();
    }
    this.#retained = next;
  }

  vector(length: number, elementBytes: number): void {
    this.retain(length * elementBytes + VECTOR_OVERHEAD);
  }

  node(depth: number): void {
    if (depth > MAX_DEPTH) {
      throw invalidBridgeValue('A bridge value is too deeply nested');
    }
    this.#nodes = checkedIncrement(this.#nodes, MAX_NODES);
  }

  operation(): void {
    this.#operations = checkedIncrement(this.#operations, MAX_OPERATIONS);
  }

  /**
   * The request written, which stays valid only until the next is begun. A
   * buffer grown for an unusually large request is let go afterwards.
   */
  bytes(): Uint8Array {
    const written = buffer.subarray(0, this.#length);
    if (buffer.length > MAX_KEPT_BUFFER_BYTES) {
      buffer = new Uint8Array(INITIAL_BUFFER_BYTES);
      view = new DataView(buffer.buffer);
    }
    return written;
  }
}

const MAX_KEPT_BUFFER_BYTES = 1024 * 1024;

/** Writes a SQL statement and its parameters. */
export const encodeExecuteSql = (
  sql: string,
  params: readonly JsonValue[],
  arrayRows: boolean,
): Uint8Array => {
  const request = new RequestWriter();
  request.u8(arrayRows ? 1 : 0);
  request.string(sql);
  writeJsonValues(request, params, 0, 'SQL parameters');
  return request.bytes();
};

export const encodePrepareSql = (sql: string): Uint8Array => {
  const request = new RequestWriter();
  request.string(sql);
  return request.bytes();
};

export const encodeExecutePrepared = (
  statementId: number,
  params: readonly JsonValue[],
  arrayRows: boolean,
): Uint8Array => {
  const request = new RequestWriter();
  request.u8(arrayRows ? 1 : 0);
  request.u32(preparedStatementId(statementId));
  writeJsonValues(request, params, 0, 'SQL parameters');
  return request.bytes();
};

export const encodeClosePrepared = (statementId: number): Uint8Array => {
  const request = new RequestWriter();
  request.u32(preparedStatementId(statementId));
  return request.bytes();
};

export const encodeExecSql = (sql: string, arrayRows: boolean): Uint8Array => {
  const request = new RequestWriter();
  request.u8(arrayRows ? 1 : 0);
  request.string(sql);
  return request.bytes();
};

const writeJsonValues = (
  request: RequestWriter,
  input: unknown,
  depth: number,
  label: string,
): void => {
  const length = denseArrayLength(input, label);
  request.vector(length, RUST_VALUE_BYTES);
  writeCount(request, length);
  for (let index = 0; index < length; index += 1) {
    writeJsonValue(request, indexedDataValue(input, index, label), depth);
  }
  if (denseArrayLength(input, label) !== length) {
    throw invalidBridgeValue('A bridge array changed while it was inspected');
  }
};

const writeJsonValue = (
  request: RequestWriter,
  value: unknown,
  depth: number,
): void => {
  request.operation();
  request.node(depth);
  if (value === null) {
    request.u8(JSON_NULL);
  } else if (value === false) {
    request.u8(JSON_FALSE);
  } else if (value === true) {
    request.u8(JSON_TRUE);
  } else if (isNumber(value)) {
    if (!isFiniteNumber(value)) {
      throw invalidBridgeValue('A JSON number must be finite');
    }
    request.u8(isSafeInteger(value) ? JSON_I64 : JSON_F64);
    request.number(value);
  } else if (isString(value)) {
    request.u8(JSON_STRING);
    request.string(value);
  } else if (isArray(value)) {
    request.u8(JSON_ARRAY);
    writeJsonValues(request, value, depth + 1, 'JSON array');
  } else if (isRecord(value)) {
    request.u8(JSON_OBJECT);
    // The count is written once the entries are.
    const countAt = request.reserve(4);
    let count = 0;
    forEachEnumerableDataEntry(value, (key, child) => {
      count += 1;
      request.string(key);
      request.retain(RUST_VALUE_BYTES + RUST_MAP_ENTRY_OVERHEAD);
      writeJsonValue(request, child, depth + 1);
    });
    view.setUint32(countAt, count, true);
  } else {
    throw invalidBridgeValue('A value is not JSON-compatible');
  }
};

const writeCount = (request: RequestWriter, count: number): void => {
  if (!isCountWithin(count, 0, MAX_OPERATIONS)) {
    throw resourceLimit();
  }
  request.u32(count);
};

const MISSING = Symbol('missing');

// A bridge value may be a hostile object: every read goes through the property
// descriptor, so that a getter cannot observe the walk or change what WASM then
// receives. Array.isArray is read through a try/catch for the same reason: a
// Proxy can throw from any trap.
const isArray = (value: unknown): value is unknown[] => {
  try {
    return arrayIsArray(value);
  } catch {
    throw invalidBridgeValue('A bridge array could not be inspected');
  }
};

const ownDataDescriptor = (
  value: object,
  name: string | number,
): PropertyDescriptor | undefined => {
  try {
    return getOwnPropertyDescriptor(value, name);
  } catch {
    throw invalidBridgeValue('A bridge property descriptor could not be read');
  }
};

const ownDataField = (value: object, name: string | number): unknown => {
  const descriptor = ownDataDescriptor(value, name);
  if (isUndefined(descriptor)) {
    return MISSING;
  }
  if (!objHasOwn(descriptor, 'value')) {
    throw invalidBridgeValue('Bridge accessors are not supported');
  }
  return descriptor.value;
};

const forEachEnumerableDataEntry = (
  value: object,
  visit: (key: string, value: unknown) => void,
): void => {
  let keys: (string | symbol)[];
  try {
    keys = ownKeys(value);
  } catch {
    throw invalidBridgeValue('Bridge object keys could not be read');
  }
  if (keys.length > MAX_OPERATIONS) {
    throw resourceLimit();
  }
  for (const key of keys) {
    if (!isString(key)) {
      continue;
    }
    const descriptor = ownDataDescriptor(value, key);
    if (isUndefined(descriptor) || !objHasOwn(descriptor, 'value')) {
      throw invalidBridgeValue('Bridge accessors are not supported');
    }
    if (descriptor.enumerable) {
      visit(key, descriptor.value);
    }
  }
};

const denseArrayLength = (value: unknown, label: string): number => {
  if (!isArray(value)) {
    throw invalidBridgeValue(`${label} must be an array`);
  }
  const length = ownDataField(value, 'length');
  if (!isCountWithin(length, 0, MAX_OPERATIONS)) {
    throw resourceLimit();
  }
  return length;
};

const indexedDataValue = (
  value: unknown,
  index: number,
  label: string,
): unknown => {
  const item = ownDataField(value as object, index);
  if (item === MISSING || isUndefined(item)) {
    throw invalidBridgeValue(`${label} cannot be sparse`);
  }
  return item;
};

const preparedStatementId = (value: unknown): number => {
  if (!isCountWithin(value, 1, MAX_U32)) {
    throw invalidBridgeValue(
      'A prepared statement ID must be a nonzero unsigned 32-bit integer',
    );
  }
  return value;
};

const assertUint = (value: number, maximum: number): void => {
  if (!isCountWithin(value, 0, maximum)) {
    throw invalidBridgeValue('A bridge unsigned integer is out of range');
  }
};

const checkedIncrement = (value: number, maximum: number): number => {
  const next = value + 1;
  if (!isCount(next) || next > maximum) {
    throw resourceLimit();
  }
  return next;
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

const resourceLimit = (): WasmBridgeError =>
  new WasmBridgeError('RESOURCE_LIMIT', 'A bridge call exceeded its resource limit');
