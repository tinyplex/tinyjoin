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

/**
 * Measures one request the way WASM will read it, without building anything.
 *
 * The sink tracks two quantities at once: the bytes the encoded request would
 * occupy, and the memory the Rust side would retain to hold it. Both are
 * bounded, so a request that cannot be served is rejected before it reaches
 * WASM rather than after it has allocated. Each request makes one, so it is a
 * single object rather than a set of closures.
 */
class PreflightSink {
  #bytes = 0;
  #retained = 0;
  #nodes = 0;
  #operations = 0;

  add(count: number): void {
    const next = this.#bytes + count;
    if (!isCount(next) || next > MAX_BYTES) {
      throw resourceLimit();
    }
    this.#bytes = next;
  }

  u8(value: number): void {
    assertUint(value, 0xff);
    this.add(1);
  }

  u32(value: number): void {
    assertUint(value, MAX_U32);
    this.add(4);
  }

  /** A JavaScript-safe integer, which WASM reads as eight bytes. */
  i64(): void {
    this.add(8);
  }

  f64(value: number): void {
    if (!isFiniteNumber(value)) {
      throw invalidBridgeValue('A JSON number must be finite');
    }
    this.add(8);
  }

  string(value: string): void {
    if (!isString(value)) {
      throw invalidBridgeValue('A bridge string must be a string');
    }
    const length = utf8Length(value);
    this.retain(length + STRING_OVERHEAD);
    this.u32(length);
    this.add(length);
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
}

/** Validates and bounds a request once before passing it unchanged to WASM. */
export const preflightExecuteSql = (
  sql: string,
  params: readonly JsonValue[],
): void => {
  const sink = new PreflightSink();
  sink.string(sql);
  writeJsonValues(sink, params, 0, 'SQL parameters');
};

export const preflightPrepareSql = (sql: string): void =>
  new PreflightSink().string(sql);

export const preflightExecutePrepared = (
  statementId: number,
  params: readonly JsonValue[],
): void => {
  const sink = new PreflightSink();
  sink.u32(preparedStatementId(statementId));
  writeJsonValues(sink, params, 0, 'SQL parameters');
};

export const preflightClosePrepared = (statementId: number): void =>
  new PreflightSink().u32(preparedStatementId(statementId));

export const preflightExecSql = (sql: string): void =>
  new PreflightSink().string(sql);

const writeJsonValues = (
  sink: PreflightSink,
  input: unknown,
  depth: number,
  label: string,
): void => {
  const length = denseArrayLength(input, label);
  sink.vector(length, RUST_VALUE_BYTES);
  writeCount(sink, length);
  for (let index = 0; index < length; index += 1) {
    writeJsonValue(sink, indexedDataValue(input, index, label), depth);
  }
  if (denseArrayLength(input, label) !== length) {
    throw invalidBridgeValue('A bridge array changed while it was inspected');
  }
};

const writeJsonValue = (
  sink: PreflightSink,
  value: unknown,
  depth: number,
): void => {
  sink.operation();
  sink.node(depth);
  if (value === null) {
    sink.u8(JSON_NULL);
  } else if (value === false) {
    sink.u8(JSON_FALSE);
  } else if (value === true) {
    sink.u8(JSON_TRUE);
  } else if (isNumber(value)) {
    if (!isFiniteNumber(value)) {
      throw invalidBridgeValue('A JSON number must be finite');
    }
    if (isSafeInteger(value)) {
      sink.u8(JSON_I64);
      sink.i64();
    } else {
      sink.u8(JSON_F64);
      sink.f64(value);
    }
  } else if (isString(value)) {
    sink.u8(JSON_STRING);
    sink.string(value);
  } else if (isArray(value)) {
    sink.u8(JSON_ARRAY);
    const length = denseArrayLength(value, 'JSON array');
    sink.vector(length, RUST_VALUE_BYTES);
    writeCount(sink, length);
    for (let index = 0; index < length; index += 1) {
      writeJsonValue(
        sink,
        indexedDataValue(value, index, 'JSON array'),
        depth + 1,
      );
    }
  } else if (isRecord(value)) {
    sink.u8(JSON_OBJECT);
    const count = enumerableDataCount(value);
    writeCount(sink, count);
    let visited = 0;
    forEachEnumerableDataEntry(value, (key, child) => {
      visited += 1;
      sink.string(key);
      sink.retain(RUST_VALUE_BYTES + RUST_MAP_ENTRY_OVERHEAD);
      writeJsonValue(sink, child, depth + 1);
    });
    if (visited !== count) {
      throw invalidBridgeValue('A bridge object changed while it was inspected');
    }
  } else {
    throw invalidBridgeValue('A value is not JSON-compatible');
  }
};

const writeCount = (sink: PreflightSink, count: number): void => {
  if (!isCountWithin(count, 0, MAX_OPERATIONS)) {
    throw resourceLimit();
  }
  sink.u32(count);
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

const enumerableDataCount = (value: object): number => {
  let count = 0;
  forEachEnumerableDataEntry(value, () => {
    count += 1;
    if (count > MAX_OPERATIONS) {
      throw resourceLimit();
    }
  });
  return count;
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
