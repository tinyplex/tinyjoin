import type {
  ApplyOutcome,
  Change,
  ChangeBatch,
  Filter,
  JsonValue,
  OrderBy,
  QueryPlan,
  QueryResult,
  Row,
  SqlResult,
  TableSchema,
} from '../protocol.js';
import type {WorkerEngine} from './engine.js';
import type {PageDevice} from './page-device.js';

const VERSION = 1;
const SUCCESS = 0;
const FAILURE = 1;
const SAFE_RESPONSE = 0;
const DURABLE_RESPONSE = 1;

export const WASM_OPERATION = {
  defineTables: 1,
  replaceSnapshot: 2,
  applyBatch: 3,
  query: 4,
  executeSql: 5,
  execSql: 6,
  begin: 7,
  commit: 8,
  rollback: 9,
  inTransaction: 10,
  revision: 11,
  close: 12,
} as const;

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
const MAX_SAFE_INTEGER = BigInt(Number.MAX_SAFE_INTEGER);
const MAX_U32 = 0xffff_ffff;
// Keep these logical retained-model estimates aligned with the wasm32 direct
// bridge contract. They are a deterministic resource boundary, not an RSS
// measurement of a particular JavaScript engine.
const STRING_OVERHEAD = 12;
const VECTOR_OVERHEAD = 12;
const JS_VALUE_BYTES = 4;
const OBJECT_OVERHEAD = 64;
const RUST_VALUE_BYTES = 24;
const RUST_TABLE_SCHEMA_BYTES = 36;
const RUST_COLUMN_BYTES = 40;
const RUST_ROW_BYTES = 12;
const RUST_CHANGE_BYTES = 28;
const RUST_FILTER_BYTES = 40;
const RUST_ORDER_BYTES = 16;
const RUST_MAP_ENTRY_OVERHEAD = 128;

const arrayIsArray = Array.isArray;
const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
const hasOwn = Object.hasOwn;
const numberIsFinite = Number.isFinite;
const numberIsSafeInteger = Number.isSafeInteger;
const ownKeys = Reflect.ownKeys;
// `ignoreBOM` means "do not consume the BOM as a signature" here: U+FEFF is
// ordinary JSON/string data and must survive the private wire exactly.
const textDecoder = new TextDecoder('utf-8', {fatal: true, ignoreBOM: true});
const textEncoder = new TextEncoder();

type ColumnType = 'boolean' | 'integer' | 'float' | 'text' | 'json';

interface WireColumnDefinition {
  name: string;
  dataType: ColumnType;
  nullable?: boolean;
  /** Missing, undefined, and null all mean no stored default. */
  default?: JsonValue;
}

/** Hidden typed catalog shape accepted by SQL and table-replacement callers. */
export type WireTableSchema = TableSchema & {
  columns?: readonly WireColumnDefinition[];
};

export type ResponseDisposition = 'safe' | 'durable';

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

export class WasmWireDecodeError extends WasmBridgeError {
  readonly disposition: ResponseDisposition | undefined;

  constructor(message: string, disposition?: ResponseDisposition) {
    super('BRIDGE_SERIALIZATION_ERROR', message, false);
    this.name = 'WasmWireDecodeError';
    this.disposition = disposition;
  }
}

class RemoteWasmError extends WasmBridgeError {}

export interface RawBinaryWasmEngine {
  call(operation: number, payload: Uint8Array): Uint8Array;
  free?(): void;
}

export interface RawBinaryWasmEngineConstructor {
  new (device: PageDevice): RawBinaryWasmEngine;
}

let insidePageDeviceCallback = 0;

/**
 * Opens the private raw WASM engine behind a module-wide page-callback guard.
 * Production code must use this seam rather than constructing the generated
 * raw binding directly.
 */
export function createBinaryWasmEngine(
  RawEngine: RawBinaryWasmEngineConstructor,
  device: PageDevice,
): BinaryWasmEngine {
  assertNotInPageDeviceCallback();
  try {
    return new BinaryWasmEngine(new RawEngine(guardPageDevice(device)));
  } catch (error) {
    throw normalizeWasmConstructorError(error);
  }
}

function guardPageDevice(device: PageDevice): PageDevice {
  return Object.freeze({
    pageCount: () => inPageDeviceCallback(() => device.pageCount()),
    readPage: (low: number, high: number, target: Uint8Array) =>
      inPageDeviceCallback(() => device.readPage(low, high, target)),
    writePage: (low: number, high: number, source: Uint8Array) =>
      inPageDeviceCallback(() => device.writePage(low, high, source)),
    flush: () => inPageDeviceCallback(() => device.flush()),
    close: () => inPageDeviceCallback(() => device.close()),
  });
}

function inPageDeviceCallback<Result>(callback: () => Result): Result {
  insidePageDeviceCallback += 1;
  try {
    return callback();
  } finally {
    insidePageDeviceCallback -= 1;
  }
}

function assertNotInPageDeviceCallback(): void {
  if (insidePageDeviceCallback !== 0) {
    throw new WasmBridgeError(
      'ENGINE_REENTRANT_CALL',
      'TinyGres cannot enter a WASM engine from a page-device callback',
      false,
    );
  }
}

export interface EncodedCall {
  readonly operation: number;
  readonly payload: Uint8Array;
  /** True when a missing/malformed disposition could conceal publication. */
  readonly mayPublish: boolean;
}

interface DecodedResponse<T> {
  readonly value: T;
  readonly disposition: ResponseDisposition;
}

interface EncodeSink {
  readonly budget: TraversalBudget;
  u8(value: number): void;
  u32(value: number): void;
  i64(value: bigint): void;
  f64(value: number): void;
  string(value: string): void;
  finish(): Uint8Array | number;
}

class TraversalBudget {
  retained = 0;
  nodes = 0;
  operations = 0;

  retain(bytes: number): void {
    if (!numberIsSafeInteger(bytes) || bytes < 0) {
      throw resourceLimit();
    }
    const next = this.retained + bytes;
    if (!numberIsSafeInteger(next) || next > MAX_BYTES) {
      throw resourceLimit();
    }
    this.retained = next;
  }

  string(bytes: number): void {
    this.retain(bytes + STRING_OVERHEAD);
  }

  vector(length: number, elementBytes = JS_VALUE_BYTES): void {
    this.retain(length * elementBytes + VECTOR_OVERHEAD);
  }

  object(): void {
    this.retain(OBJECT_OVERHEAD);
  }

  node(depth: number): void {
    if (depth > MAX_DEPTH) {
      throw invalidBridgeValue('A binary bridge value is too deeply nested');
    }
    this.nodes = checkedIncrement(this.nodes, MAX_NODES);
  }

  operation(count = 1): void {
    if (!numberIsSafeInteger(count) || count < 0) {
      throw resourceLimit();
    }
    const next = this.operations + count;
    if (!numberIsSafeInteger(next) || next > MAX_OPERATIONS) {
      throw resourceLimit();
    }
    this.operations = next;
  }
}

class SizeSink implements EncodeSink {
  readonly budget = new TraversalBudget();
  #bytes = 0;

  u8(value: number): void {
    assertUint(value, 0xff);
    this.#add(1);
  }

  u32(value: number): void {
    assertUint(value, MAX_U32);
    this.#add(4);
  }

  i64(value: bigint): void {
    if (value < -MAX_SAFE_INTEGER || value > MAX_SAFE_INTEGER) {
      throw invalidBridgeValue('A JSON integer is not JavaScript-safe');
    }
    this.#add(8);
  }

  f64(value: number): void {
    if (!numberIsFinite(value)) {
      throw invalidBridgeValue('A JSON number must be finite');
    }
    this.#add(8);
  }

  string(value: string): void {
    if (typeof value !== 'string') {
      throw invalidBridgeValue('A binary bridge string must be a string');
    }
    const bytes = utf8Length(value);
    this.budget.string(bytes);
    this.u32(bytes);
    this.#add(bytes);
  }

  finish(): number {
    return this.#bytes;
  }

  #add(bytes: number): void {
    const next = this.#bytes + bytes;
    if (!numberIsSafeInteger(next) || next > MAX_BYTES) {
      throw resourceLimit();
    }
    this.#bytes = next;
  }
}

class ByteSink implements EncodeSink {
  readonly budget = new TraversalBudget();
  readonly #bytes: Uint8Array;
  readonly #view: DataView;
  #offset = 0;

  constructor(length: number) {
    this.#bytes = new Uint8Array(length);
    this.#view = new DataView(this.#bytes.buffer);
  }

  u8(value: number): void {
    assertUint(value, 0xff);
    this.#reserve(1);
    this.#view.setUint8(this.#offset, value);
    this.#offset += 1;
  }

  u32(value: number): void {
    assertUint(value, MAX_U32);
    this.#reserve(4);
    this.#view.setUint32(this.#offset, value, true);
    this.#offset += 4;
  }

  i64(value: bigint): void {
    if (value < -MAX_SAFE_INTEGER || value > MAX_SAFE_INTEGER) {
      throw invalidBridgeValue('A JSON integer is not JavaScript-safe');
    }
    this.#reserve(8);
    this.#view.setBigInt64(this.#offset, value, true);
    this.#offset += 8;
  }

  f64(value: number): void {
    if (!numberIsFinite(value)) {
      throw invalidBridgeValue('A JSON number must be finite');
    }
    this.#reserve(8);
    this.#view.setFloat64(this.#offset, value, true);
    this.#offset += 8;
  }

  string(value: string): void {
    if (typeof value !== 'string') {
      throw invalidBridgeValue('A binary bridge string must be a string');
    }
    const byteLength = utf8Length(value);
    this.budget.string(byteLength);
    this.u32(byteLength);
    this.#reserve(byteLength);
    const encoded = textEncoder.encodeInto(
      value,
      this.#bytes.subarray(this.#offset, this.#offset + byteLength),
    );
    if (encoded.read !== value.length || encoded.written !== byteLength) {
      throw invalidBridgeValue('A bridge string changed while it was encoded');
    }
    this.#offset += byteLength;
  }

  finish(): Uint8Array {
    if (this.#offset !== this.#bytes.byteLength) {
      throw invalidBridgeValue('A bridge value changed while it was encoded');
    }
    return this.#bytes;
  }

  #reserve(bytes: number): void {
    if (this.#offset + bytes > this.#bytes.byteLength) {
      throw invalidBridgeValue('A bridge value grew while it was encoded');
    }
  }
}

export function encodeDefineTables(
  schemas: readonly TableSchema[],
): EncodedCall {
  return call(
    WASM_OPERATION.defineTables,
    true,
    (sink) => writeSchemas(sink, schemas as readonly WireTableSchema[]),
  );
}

export function encodeReplaceSnapshot(
  schema: TableSchema,
  rows: readonly Row[],
): EncodedCall {
  return call(WASM_OPERATION.replaceSnapshot, true, (sink) => {
    writeSchema(sink, schema as WireTableSchema);
    writeRows(sink, rows);
  });
}

export function encodeApplyBatch(batch: ChangeBatch): EncodedCall {
  return call(WASM_OPERATION.applyBatch, true, (sink) =>
    writeBatch(sink, batch),
  );
}

export function encodeQuery(plan: QueryPlan): EncodedCall {
  return call(WASM_OPERATION.query, false, (sink) => writeQuery(sink, plan));
}

export function encodeExecuteSql(
  sql: string,
  params: readonly JsonValue[],
): EncodedCall {
  return sqlCall(WASM_OPERATION.executeSql, true, sql, params);
}

export function encodeExecSql(sql: string): EncodedCall {
  return call(WASM_OPERATION.execSql, true, (sink) => sink.string(sql));
}

export function encodeUnitCall(
  operation:
    | typeof WASM_OPERATION.begin
    | typeof WASM_OPERATION.commit
    | typeof WASM_OPERATION.rollback
    | typeof WASM_OPERATION.inTransaction
    | typeof WASM_OPERATION.revision
    | typeof WASM_OPERATION.close,
): EncodedCall {
  return call(operation, operation === WASM_OPERATION.commit, () => {});
}

function sqlCall(
  operation: number,
  mayPublish: boolean,
  sql: string,
  params: readonly JsonValue[],
): EncodedCall {
  return call(operation, mayPublish, (sink) => {
    sink.string(sql);
    writeJsonValues(sink, params, 0, 'SQL parameters');
  });
}

function call(
  operation: number,
  mayPublish: boolean,
  write: (sink: EncodeSink) => void,
): EncodedCall {
  // Pass one validates the complete graph and exact byte count. Only then is
  // the output allocation made. Pass two repeats all descriptor/type checks so
  // a hostile Proxy cannot grow past the private exact-sized buffer.
  const size = new SizeSink();
  size.u8(VERSION);
  write(size);
  const output = new ByteSink(size.finish() as number);
  output.u8(VERSION);
  write(output);
  return {operation, payload: output.finish() as Uint8Array, mayPublish};
}

function writeSchemas(
  sink: EncodeSink,
  schemas: readonly WireTableSchema[],
): void {
  writeArray(
    sink,
    schemas,
    0,
    'table schemas',
    RUST_TABLE_SCHEMA_BYTES,
    (target, schema) => writeSchema(target, schema as WireTableSchema),
  );
}

function writeSchema(sink: EncodeSink, input: WireTableSchema): void {
  const schema = record(input, 'table schema');
  sink.string(requiredString(schema, 'name', 'table schema'));
  writeStringArray(
    sink,
    requiredField(schema, 'primaryKey', 'table schema'),
    'table schema primary key',
  );
  const columns = optionalField(schema, 'columns');
  writeArray(
    sink,
    columns === MISSING ? [] : columns,
    0,
    'table schema columns',
    RUST_COLUMN_BYTES,
    writeColumn,
  );
}

function writeColumn(sink: EncodeSink, input: unknown): void {
  const column = record(input, 'column definition');
  sink.string(requiredString(column, 'name', 'column definition'));
  const dataType = requiredString(column, 'dataType', 'column definition');
  sink.u8(columnTypeTag(dataType));
  const nullable = optionalField(column, 'nullable');
  if (nullable !== MISSING && typeof nullable !== 'boolean') {
    throw invalidBridgeValue('A column nullable flag must be boolean');
  }
  sink.u8(nullable === MISSING || nullable ? 1 : 0);
  const defaultValue = optionalField(column, 'default');
  if (defaultValue === MISSING || defaultValue === null) {
    sink.u8(0);
  } else {
    sink.u8(1);
    writeJsonValue(sink, defaultValue, 0);
  }
}

function writeBatch(sink: EncodeSink, input: ChangeBatch): void {
  const batch = record(input, 'change batch');
  writeArray(
    sink,
    requiredField(batch, 'changes', 'change batch'),
    0,
    'changes',
    RUST_CHANGE_BYTES,
    writeChange,
  );
}

function writeChange(sink: EncodeSink, input: unknown): void {
  const change = record(input, 'change');
  const type = requiredString(change, 'type', 'change');
  const table = requiredString(change, 'table', 'change');
  if (type === 'upsert') {
    sink.u8(0);
    sink.string(table);
    writeRow(sink, requiredField(change, 'row', 'upsert change'), 0);
  } else if (type === 'delete') {
    sink.u8(1);
    sink.string(table);
    writeRow(sink, requiredField(change, 'key', 'delete change'), 0);
  } else {
    throw invalidBridgeValue('A change has an invalid type');
  }
}

function writeQuery(sink: EncodeSink, input: QueryPlan): void {
  const plan = record(input, 'query plan');
  ensureOnlyFields(plan, [
    'table',
    'columns',
    'filters',
    'orderBy',
    'limit',
    'offset',
  ]);
  sink.string(requiredString(plan, 'table', 'query plan'));
  const columns = optionalField(plan, 'columns');
  if (columns === MISSING) {
    sink.u8(0);
  } else {
    sink.u8(1);
    writeStringArray(sink, columns, 'query projection');
  }
  writeArray(
    sink,
    requiredField(plan, 'filters', 'query plan'),
    0,
    'query filters',
    RUST_FILTER_BYTES,
    writeFilter,
  );
  const orderBy = optionalField(plan, 'orderBy');
  writeArray(
    sink,
    orderBy === MISSING ? [] : orderBy,
    0,
    'query ordering',
    RUST_ORDER_BYTES,
    writeOrderBy,
  );
  const limit = optionalField(plan, 'limit');
  if (limit === MISSING) {
    sink.u8(0);
  } else {
    sink.u8(1);
    sink.u32(queryPosition(limit, 'limit'));
  }
  const offsetValue = optionalField(plan, 'offset');
  const offset = offsetValue === MISSING ? 0 : queryPosition(offsetValue, 'offset');
  sink.u32(offset);
  if (limit !== MISSING && offset + Number(limit) > MAX_U32) {
    throw invalidBridgeValue('Query OFFSET plus LIMIT exceeds u32');
  }
}

function writeFilter(sink: EncodeSink, input: unknown): void {
  const filter = record(input, 'query filter');
  sink.string(requiredString(filter, 'column', 'query filter'));
  sink.u8(filterOperatorTag(requiredString(filter, 'operator', 'query filter')));
  writeJsonValue(
    sink,
    requiredField(filter, 'value', 'query filter'),
    0,
  );
}

function writeOrderBy(sink: EncodeSink, input: unknown): void {
  const order = record(input, 'query ordering');
  ensureOnlyFields(order, ['column', 'direction', 'nulls']);
  sink.string(requiredString(order, 'column', 'query ordering'));
  const direction = requiredString(order, 'direction', 'query ordering');
  sink.u8(
    direction === 'asc'
      ? 0
      : direction === 'desc'
        ? 1
        : invalidTag('query direction'),
  );
  const nulls = requiredString(order, 'nulls', 'query ordering');
  sink.u8(
    nulls === 'default'
      ? 0
      : nulls === 'first'
        ? 1
        : nulls === 'last'
          ? 2
          : invalidTag('null ordering'),
  );
}

function writeRows(sink: EncodeSink, rows: unknown): void {
  writeArray(sink, rows, 0, 'rows', RUST_ROW_BYTES, (target, row) =>
    writeRow(target, row, 0),
  );
}

function writeRow(sink: EncodeSink, input: unknown, depth: number): void {
  const row = record(input, 'row');
  sink.budget.node(depth);
  const count = enumerableDataCount(row);
  writeCount(sink, count);
  let visited = 0;
  forEachEnumerableDataEntry(row, (key, value) => {
    visited += 1;
    sink.string(key);
    sink.budget.retain(RUST_VALUE_BYTES + RUST_MAP_ENTRY_OVERHEAD);
    writeJsonValue(sink, value, depth + 1);
  });
  if (visited !== count) {
    throw invalidBridgeValue('A bridge object changed while it was encoded');
  }
}

function writeJsonValues(
  sink: EncodeSink,
  input: unknown,
  depth: number,
  label: string,
): void {
  writeArray(sink, input, depth, label, RUST_VALUE_BYTES, (target, value) =>
    writeJsonValue(target, value, depth),
  );
}

function writeJsonValue(
  sink: EncodeSink,
  value: unknown,
  depth: number,
): void {
  sink.budget.operation();
  sink.budget.node(depth);
  if (value === null) {
    sink.u8(JSON_NULL);
  } else if (value === false) {
    sink.u8(JSON_FALSE);
  } else if (value === true) {
    sink.u8(JSON_TRUE);
  } else if (typeof value === 'number') {
    if (!numberIsFinite(value)) {
      throw invalidBridgeValue('A JSON number must be finite');
    }
    if (numberIsSafeInteger(value)) {
      sink.u8(JSON_I64);
      // BigInt(-0) is 0n, preserving the prior bridge's canonical spelling.
      sink.i64(BigInt(value));
    } else {
      sink.u8(JSON_F64);
      sink.f64(value);
    }
  } else if (typeof value === 'string') {
    sink.u8(JSON_STRING);
    sink.string(value);
  } else if (isArray(value)) {
    sink.u8(JSON_ARRAY);
    const length = denseArrayLength(value, 'JSON array');
    sink.budget.vector(length, RUST_VALUE_BYTES);
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
      sink.budget.retain(RUST_VALUE_BYTES + RUST_MAP_ENTRY_OVERHEAD);
      writeJsonValue(sink, child, depth + 1);
    });
    if (visited !== count) {
      throw invalidBridgeValue('A bridge object changed while it was encoded');
    }
  } else {
    throw invalidBridgeValue('A value is not JSON-compatible');
  }
}

function writeStringArray(sink: EncodeSink, input: unknown, label: string): void {
  writeArray(
    sink,
    input,
    0,
    label,
    STRING_OVERHEAD,
    (target, value) => {
      if (typeof value !== 'string') {
        throw invalidBridgeValue(`${label} must contain strings`);
      }
      target.string(value);
    },
  );
}

function writeArray(
  sink: EncodeSink,
  input: unknown,
  depth: number,
  label: string,
  retainedElementBytes: number,
  write: (sink: EncodeSink, value: unknown) => void,
): void {
  const length = denseArrayLength(input, label);
  sink.budget.vector(length, retainedElementBytes);
  writeCount(sink, length);
  for (let index = 0; index < length; index += 1) {
    write(sink, indexedDataValue(input, index, label));
  }
  if (denseArrayLength(input, label) !== length) {
    throw invalidBridgeValue('A bridge array changed while it was encoded');
  }
}

function writeCount(sink: EncodeSink, count: number): void {
  if (!numberIsSafeInteger(count) || count < 0 || count > MAX_OPERATIONS) {
    throw resourceLimit();
  }
  sink.u32(count);
}

const MISSING = Symbol('missing');

function record(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) {
    throw invalidBridgeValue(`${label} must be an object`);
  }
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !isArray(value);
}

function isArray(value: unknown): value is unknown[] {
  try {
    return arrayIsArray(value);
  } catch {
    throw invalidBridgeValue('A bridge array could not be inspected');
  }
}

function ownDataField(
  value: Record<string, unknown>,
  name: string,
): unknown | typeof MISSING {
  let descriptor: PropertyDescriptor | undefined;
  try {
    descriptor = getOwnPropertyDescriptor(value, name);
  } catch {
    throw invalidBridgeValue('A bridge property descriptor could not be read');
  }
  if (descriptor === undefined) {
    return MISSING;
  }
  if (!hasOwn(descriptor, 'value')) {
    throw invalidBridgeValue('Bridge accessors are not supported');
  }
  return descriptor.value;
}

function requiredField(
  value: Record<string, unknown>,
  name: string,
  label: string,
): unknown {
  const field = ownDataField(value, name);
  if (field === MISSING || field === undefined) {
    throw invalidBridgeValue(`${label}.${name} is required`);
  }
  return field;
}

function optionalField(
  value: Record<string, unknown>,
  name: string,
): unknown | typeof MISSING {
  const field = ownDataField(value, name);
  return field === undefined ? MISSING : field;
}

function requiredString(
  value: Record<string, unknown>,
  name: string,
  label: string,
): string {
  const field = requiredField(value, name, label);
  if (typeof field !== 'string') {
    throw invalidBridgeValue(`${label}.${name} must be a string`);
  }
  return field;
}

function enumerableDataCount(value: Record<string, unknown>): number {
  let count = 0;
  forEachEnumerableDataEntry(value, () => {
    count += 1;
    if (count > MAX_OPERATIONS) {
      throw resourceLimit();
    }
  });
  return count;
}

function forEachEnumerableDataEntry(
  value: Record<string, unknown>,
  visit: (key: string, value: unknown) => void,
): void {
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
    if (typeof key !== 'string') {
      continue;
    }
    let descriptor: PropertyDescriptor | undefined;
    try {
      descriptor = getOwnPropertyDescriptor(value, key);
    } catch {
      throw invalidBridgeValue('A bridge property descriptor could not be read');
    }
    if (descriptor === undefined || !hasOwn(descriptor, 'value')) {
      throw invalidBridgeValue('Bridge accessors are not supported');
    }
    if (descriptor.enumerable) {
      visit(key, descriptor.value);
    }
  }
}

function ensureOnlyFields(
  value: Record<string, unknown>,
  allowed: readonly string[],
): void {
  forEachEnumerableDataEntry(value, (key) => {
    if (!allowed.includes(key)) {
      throw invalidBridgeValue('A bridge object contains an unknown field');
    }
  });
}

function denseArrayLength(value: unknown, label: string): number {
  if (!isArray(value)) {
    throw invalidBridgeValue(`${label} must be an array`);
  }
  const length = ownDataField(value as unknown as Record<string, unknown>, 'length');
  if (
    typeof length !== 'number' ||
    !numberIsSafeInteger(length) ||
    length < 0 ||
    length > MAX_OPERATIONS
  ) {
    throw resourceLimit();
  }
  return length;
}

function indexedDataValue(value: unknown, index: number, label: string): unknown {
  const item = ownDataField(
    value as unknown as Record<string, unknown>,
    String(index),
  );
  if (item === MISSING || item === undefined) {
    throw invalidBridgeValue(`${label} cannot be sparse`);
  }
  return item;
}

function queryPosition(value: unknown, label: string): number {
  if (
    typeof value !== 'number' ||
    !numberIsSafeInteger(value) ||
    value < 0 ||
    value > MAX_U32
  ) {
    throw invalidBridgeValue(`Query ${label} must be an unsigned 32-bit integer`);
  }
  return value;
}

function columnTypeTag(type: string): number {
  switch (type) {
    case 'boolean':
      return 0;
    case 'integer':
      return 1;
    case 'float':
      return 2;
    case 'text':
      return 3;
    case 'json':
      return 4;
    default:
      return invalidTag('column type');
  }
}

function filterOperatorTag(operator: string): number {
  switch (operator) {
    case 'eq':
      return 0;
    case 'neq':
      return 1;
    case 'lt':
      return 2;
    case 'lte':
      return 3;
    case 'gt':
      return 4;
    case 'gte':
      return 5;
    default:
      return invalidTag('filter operator');
  }
}

function invalidTag(label: string): never {
  throw invalidBridgeValue(`Invalid ${label}`);
}

function assertUint(value: number, maximum: number): void {
  if (!numberIsSafeInteger(value) || value < 0 || value > maximum) {
    throw invalidBridgeValue('A binary unsigned integer is out of range');
  }
}

function checkedIncrement(value: number, maximum: number): number {
  const next = value + 1;
  if (!numberIsSafeInteger(next) || next > maximum) {
    throw resourceLimit();
  }
  return next;
}

function utf8Length(value: string): number {
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
}

class ResponseReader {
  readonly disposition: ResponseDisposition;
  readonly #bytes: Uint8Array;
  readonly #view: DataView;
  readonly #budget = new TraversalBudget();
  #offset = 0;

  constructor(value: unknown) {
    if (!(value instanceof Uint8Array) || value.byteLength > MAX_BYTES) {
      throw new WasmWireDecodeError('WASM returned an invalid binary response');
    }
    this.#bytes = value;
    this.#view = new DataView(value.buffer, value.byteOffset, value.byteLength);
    if (this.u8() !== VERSION) {
      throw new WasmWireDecodeError('WASM returned an unsupported wire version');
    }
    const status = this.u8();
    const disposition = this.u8();
    if (disposition !== SAFE_RESPONSE && disposition !== DURABLE_RESPONSE) {
      throw new WasmWireDecodeError('WASM returned an invalid disposition');
    }
    this.disposition = disposition === DURABLE_RESPONSE ? 'durable' : 'safe';
    if (status === FAILURE) {
      if (this.disposition !== 'safe') {
        throw new WasmWireDecodeError(
          'WASM returned a durable failure envelope',
          this.disposition,
        );
      }
      const code = this.string();
      const message = this.string();
      const retryableTag = this.u8();
      if (retryableTag > 2) {
        throw this.#invalid('WASM returned an invalid retryability tag');
      }
      const retryable = retryableTag === 0 ? undefined : retryableTag === 2;
      this.finish();
      throw new RemoteWasmError(code, message, retryable);
    }
    if (status !== SUCCESS) {
      throw new WasmWireDecodeError(
        'WASM returned an invalid response status',
        this.disposition,
      );
    }
  }

  u8(): number {
    this.#take(1);
    const value = this.#view.getUint8(this.#offset);
    this.#offset += 1;
    return value;
  }

  u32(): number {
    this.#take(4);
    const value = this.#view.getUint32(this.#offset, true);
    this.#offset += 4;
    return value;
  }

  u64(): bigint {
    this.#take(8);
    const value = this.#view.getBigUint64(this.#offset, true);
    this.#offset += 8;
    return value;
  }

  i64(): bigint {
    this.#take(8);
    const value = this.#view.getBigInt64(this.#offset, true);
    this.#offset += 8;
    return value;
  }

  f64(): number {
    this.#take(8);
    const value = this.#view.getFloat64(this.#offset, true);
    this.#offset += 8;
    if (!numberIsFinite(value)) {
      throw this.#invalid('WASM returned a non-finite JSON number');
    }
    return value;
  }

  count(): number {
    const count = this.u32();
    if (count > MAX_OPERATIONS) {
      throw this.#invalid('WASM returned an oversized collection');
    }
    return count;
  }

  string(): string {
    const length = this.u32();
    this.#budget.string(length);
    const bytes = this.#slice(length);
    if (!validUtf8(bytes)) {
      throw this.#invalid('WASM returned invalid UTF-8');
    }
    try {
      return textDecoder.decode(bytes);
    } catch {
      throw this.#invalid('WASM returned invalid UTF-8');
    }
  }

  strings(): string[] {
    const length = this.count();
    this.#budget.vector(length);
    const values = new Array<string>(length);
    for (let index = 0; index < length; index += 1) {
      this.#budget.operation();
      values[index] = this.string();
    }
    return values;
  }

  fields(): Array<{name: string; dataTypeID: number}> {
    const length = this.count();
    this.#budget.vector(length);
    const fields = new Array<{name: string; dataTypeID: number}>(length);
    for (let index = 0; index < length; index += 1) {
      this.#budget.operation();
      this.resultObject(['name', 'dataTypeID']);
      fields[index] = {name: this.string(), dataTypeID: this.u32()};
    }
    return fields;
  }

  sqlResult(): SqlResult {
    this.resultObject([
      'command',
      'revision',
      'rowCount',
      'fields',
      'rows',
      'tables',
    ]);
    return {
      command: this.string(),
      revision: this.safeNumber(),
      rowCount: this.safeNumber(),
      fields: this.fields(),
      rows: this.rows(),
      tables: this.strings(),
    };
  }

  sqlResults(): SqlResult[] {
    const length = this.count();
    this.#budget.vector(length);
    const results = new Array<SqlResult>(length);
    for (let index = 0; index < length; index += 1) {
      this.#budget.operation();
      results[index] = this.sqlResult();
    }
    return results;
  }

  rows(): Row[] {
    const length = this.count();
    this.#budget.vector(length);
    const rows = new Array<Row>(length);
    for (let index = 0; index < length; index += 1) {
      this.#budget.operation();
      rows[index] = this.row(0);
    }
    return rows;
  }

  row(depth: number): Row {
    this.#budget.node(depth);
    const length = this.count();
    this.#budget.object();
    const row = Object.create(null) as Row;
    for (let index = 0; index < length; index += 1) {
      this.#budget.operation();
      const key = this.string();
      if (hasOwn(row, key)) {
        throw this.#invalid('WASM returned a duplicate object key');
      }
      row[key] = this.value(depth + 1);
    }
    return row;
  }

  value(depth: number): JsonValue {
    this.#budget.node(depth);
    switch (this.u8()) {
      case JSON_NULL:
        return null;
      case JSON_FALSE:
        return false;
      case JSON_TRUE:
        return true;
      case JSON_I64: {
        const value = this.i64();
        if (value < -MAX_SAFE_INTEGER || value > MAX_SAFE_INTEGER) {
          throw this.#invalid('WASM returned an unsafe JSON integer');
        }
        return Number(value);
      }
      case JSON_F64:
        return this.f64();
      case JSON_STRING:
        return this.string();
      case JSON_ARRAY: {
        const length = this.count();
        this.#budget.vector(length);
        const values = new Array<JsonValue>(length);
        for (let index = 0; index < length; index += 1) {
          this.#budget.operation();
          values[index] = this.value(depth + 1);
        }
        return values;
      }
      case JSON_OBJECT:
        return this.rowBody(depth);
      default:
        throw this.#invalid('WASM returned an invalid JSON tag');
    }
  }

  rowBody(depth: number): Row {
    const length = this.count();
    this.#budget.object();
    const row = Object.create(null) as Row;
    for (let index = 0; index < length; index += 1) {
      this.#budget.operation();
      const key = this.string();
      if (hasOwn(row, key)) {
        throw this.#invalid('WASM returned a duplicate object key');
      }
      row[key] = this.value(depth + 1);
    }
    return row;
  }

  safeNumber(): number {
    const value = this.u64();
    if (value > MAX_SAFE_INTEGER) {
      throw this.#invalid('WASM returned an unsafe JavaScript integer');
    }
    return Number(value);
  }

  resultObject(keys: readonly string[]): void {
    this.#budget.object();
    for (const key of keys) {
      this.#budget.operation();
      this.#budget.string(utf8Length(key));
    }
  }

  finish(): void {
    if (this.#offset !== this.#bytes.byteLength) {
      throw this.#invalid('WASM returned trailing binary bytes');
    }
  }

  #take(length: number): void {
    if (this.#offset + length > this.#bytes.byteLength) {
      throw this.#invalid('WASM returned a truncated binary response');
    }
  }

  #slice(length: number): Uint8Array {
    this.#take(length);
    const value = this.#bytes.subarray(this.#offset, this.#offset + length);
    this.#offset += length;
    return value;
  }

  #invalid(message: string): WasmWireDecodeError {
    return new WasmWireDecodeError(message, this.disposition);
  }
}

export function decodeUnitResponse(value: unknown): DecodedResponse<void> {
  return decodeResponse(value, () => undefined);
}

export function decodeBooleanResponse(
  value: unknown,
): DecodedResponse<boolean> {
  return decodeResponse(value, (reader) => {
    const boolean = reader.u8();
    if (boolean !== 0 && boolean !== 1) {
      throw new WasmWireDecodeError(
        'WASM returned an invalid boolean',
        reader.disposition,
      );
    }
    return boolean === 1;
  });
}

export function decodeRevisionResponse(
  value: unknown,
): DecodedResponse<number> {
  return decodeResponse(value, (reader) => reader.safeNumber());
}

export function decodeApplyOutcomeResponse(
  value: unknown,
): DecodedResponse<ApplyOutcome> {
  return decodeResponse(value, (reader) => {
    reader.resultObject(['revision', 'tables']);
    return {
      revision: reader.safeNumber(),
      tables: reader.strings(),
    };
  });
}

export function decodeQueryResultResponse(
  value: unknown,
): DecodedResponse<QueryResult> {
  return decodeResponse(value, (reader) => {
    reader.resultObject(['revision', 'fields', 'rows']);
    return {
      revision: reader.safeNumber(),
      fields: reader.fields(),
      rows: reader.rows(),
    };
  });
}

export function decodeSqlResultResponse(
  value: unknown,
): DecodedResponse<SqlResult> {
  return decodeResponse(value, (reader) => reader.sqlResult());
}

export function decodeSqlResultsResponse(
  value: unknown,
): DecodedResponse<SqlResult[]> {
  return decodeResponse(value, (reader) => reader.sqlResults());
}

function decodeResponse<T>(
  value: unknown,
  decode: (reader: ResponseReader) => T,
): DecodedResponse<T> {
  let reader: ResponseReader | undefined;
  try {
    const activeReader = new ResponseReader(value);
    reader = activeReader;
    const result = decode(activeReader);
    activeReader.finish();
    return {value: result, disposition: activeReader.disposition!};
  } catch (error) {
    if (error instanceof RemoteWasmError || error instanceof WasmWireDecodeError) {
      throw error;
    }
    throw new WasmWireDecodeError(
      error instanceof Error ? error.message : 'WASM returned invalid binary data',
      reader?.disposition,
    );
  }
}

/** Decodes the Uint8Array thrown by the WASM constructor/open path. */
export function normalizeWasmConstructorError(error: unknown): unknown {
  if (!(error instanceof Uint8Array)) {
    return error;
  }
  try {
    // A constructor can only return a failure envelope. Any success envelope
    // is itself malformed and is surfaced as a bridge serialization error.
    decodeUnitResponse(error);
    return new WasmWireDecodeError('WASM constructor returned success as an exception');
  } catch (decoded) {
    return decoded;
  }
}

/**
 * WorkerEngine-preserving adapter around the private binary WASM ABI.
 *
 * A malformed result poisons only when the response says it followed a durable
 * publication, or when even the disposition cannot be trusted for an operation
 * that could have published. Remote recovery/unknown-outcome errors also close
 * the adapter immediately.
 */
export class BinaryWasmEngine implements WorkerEngine {
  readonly #raw: RawBinaryWasmEngine;
  #state: 'open' | 'poisoned' | 'closed' = 'open';
  #rawReleased = false;

  constructor(raw: RawBinaryWasmEngine) {
    assertNotInPageDeviceCallback();
    this.#raw = raw;
  }

  defineTable(schema: TableSchema): void {
    this.defineTables([schema]);
  }

  defineTables(schemas: TableSchema[]): void {
    this.#assertCallable();
    this.#invoke(encodeDefineTables(schemas), decodeUnitResponse);
  }

  replaceTableSnapshot(schema: TableSchema, rows: Row[]): ApplyOutcome {
    this.#assertCallable();
    return this.#invoke(
      encodeReplaceSnapshot(schema, rows),
      decodeApplyOutcomeResponse,
    );
  }

  applyBatch(batch: ChangeBatch): ApplyOutcome {
    this.#assertCallable();
    return this.#invoke(encodeApplyBatch(batch), decodeApplyOutcomeResponse);
  }

  query(plan: QueryPlan): QueryResult {
    this.#assertCallable();
    return this.#invoke(encodeQuery(plan), decodeQueryResultResponse);
  }

  executeSql(sql: string, params: JsonValue[]): SqlResult {
    this.#assertCallable();
    return this.#invoke(
      encodeExecuteSql(sql, params),
      decodeSqlResultResponse,
    );
  }

  execSql(sql: string): SqlResult[] {
    this.#assertCallable();
    return this.#invoke(encodeExecSql(sql), decodeSqlResultsResponse);
  }

  beginTransaction(): void {
    this.#assertCallable();
    this.#invoke(
      encodeUnitCall(WASM_OPERATION.begin),
      decodeUnitResponse,
    );
  }

  commitTransaction(): ApplyOutcome {
    this.#assertCallable();
    return this.#invoke(
      encodeUnitCall(WASM_OPERATION.commit),
      decodeApplyOutcomeResponse,
    );
  }

  rollbackTransaction(): void {
    this.#assertCallable();
    this.#invoke(
      encodeUnitCall(WASM_OPERATION.rollback),
      decodeUnitResponse,
    );
  }

  inTransaction(): boolean {
    this.#assertCallable();
    return this.#invoke(
      encodeUnitCall(WASM_OPERATION.inTransaction),
      decodeBooleanResponse,
    );
  }

  revision(): number {
    this.#assertCallable();
    return this.#invoke(
      encodeUnitCall(WASM_OPERATION.revision),
      decodeRevisionResponse,
    );
  }

  close(): void {
    assertNotInPageDeviceCallback();
    if (this.#state === 'closed') {
      return;
    }
    const wasOpen = this.#state === 'open';
    this.#state = 'closed';
    try {
      if (wasOpen) {
        const request = encodeUnitCall(WASM_OPERATION.close);
        const response = this.#raw.call(request.operation, request.payload);
        decodeUnitResponse(response);
      }
    } catch (error) {
      throw normalizeWasmConstructorError(error);
    } finally {
      this.#releaseRaw();
    }
  }

  #invoke<T>(
    request: EncodedCall,
    decode: (value: unknown) => DecodedResponse<T>,
  ): T {
    this.#assertCallable();
    let response: Uint8Array;
    try {
      response = this.#raw.call(request.operation, request.payload);
    } catch (error) {
      const normalized = normalizeWasmConstructorError(error);
      if (
        normalized instanceof WasmBridgeError &&
        !(normalized instanceof WasmWireDecodeError)
      ) {
        this.#closeOnFatalRemoteError(normalized);
        throw normalized;
      }
      if (request.mayPublish) {
        throw this.#poisonUnknown(normalized);
      }
      throw normalized;
    }
    try {
      return decode(response).value;
    } catch (error) {
      if (error instanceof WasmBridgeError && !(error instanceof WasmWireDecodeError)) {
        this.#closeOnFatalRemoteError(error);
        throw error;
      }
      const disposition =
        error instanceof WasmWireDecodeError ? error.disposition : undefined;
      if (disposition === 'durable' || (disposition === undefined && request.mayPublish)) {
        throw this.#poisonUnknown(error);
      }
      throw error;
    }
  }

  #assertCallable(): void {
    assertNotInPageDeviceCallback();
    if (this.#state === 'poisoned') {
      throw new WasmBridgeError(
        'STORAGE_ENGINE_POISONED',
        'The TinyGres engine cannot be used after an uncertain result',
        false,
      );
    }
    if (this.#state === 'closed') {
      throw new WasmBridgeError('ENGINE_CLOSED', 'The TinyGres engine is closed');
    }
  }

  #closeOnFatalRemoteError(error: WasmBridgeError): void {
    if (
      error.code === 'RECOVERY_REQUIRED' ||
      error.code === 'STORAGE_COMMIT_OUTCOME_UNKNOWN' ||
      error.code === 'STORAGE_ENGINE_POISONED'
    ) {
      this.#closeRaw();
    }
  }

  #poisonUnknown(error: unknown): WasmBridgeError {
    this.#closeRaw();
    const detail =
      error instanceof Error && error.message ? `: ${error.message}` : '';
    return new WasmBridgeError(
      'STORAGE_COMMIT_OUTCOME_UNKNOWN',
      `TinyGres could not decode a result after a possible durable mutation${detail}`,
      false,
    );
  }

  #closeRaw(): void {
    if (this.#state !== 'open') {
      return;
    }
    this.#state = 'poisoned';
    try {
      const request = encodeUnitCall(WASM_OPERATION.close);
      this.#raw.call(request.operation, request.payload);
    } catch {
      // The first uncertain/fatal result remains decisive.
    }
    this.#releaseRaw();
  }

  #releaseRaw(): void {
    if (this.#rawReleased) {
      return;
    }
    this.#rawReleased = true;
    try {
      this.#raw.free?.();
    } catch {
      // The first uncertain/fatal/close result remains decisive.
    }
  }
}

function validUtf8(bytes: Uint8Array): boolean {
  let index = 0;
  while (index < bytes.length) {
    const first = bytes[index]!;
    if (first <= 0x7f) {
      index += 1;
      continue;
    }
    let needed: number;
    let minimum: number;
    let value: number;
    if (first >= 0xc2 && first <= 0xdf) {
      needed = 1;
      minimum = 0x80;
      value = first & 0x1f;
    } else if (first >= 0xe0 && first <= 0xef) {
      needed = 2;
      minimum = 0x800;
      value = first & 0x0f;
    } else if (first >= 0xf0 && first <= 0xf4) {
      needed = 3;
      minimum = 0x10000;
      value = first & 0x07;
    } else {
      return false;
    }
    if (index + needed >= bytes.length) {
      return false;
    }
    for (let offset = 1; offset <= needed; offset += 1) {
      const byte = bytes[index + offset]!;
      if ((byte & 0xc0) !== 0x80) {
        return false;
      }
      value = (value << 6) | (byte & 0x3f);
    }
    if (
      value < minimum ||
      value > 0x10ffff ||
      (value >= 0xd800 && value <= 0xdfff)
    ) {
      return false;
    }
    index += needed + 1;
  }
  return true;
}

function invalidBridgeValue(message: string): WasmBridgeError {
  return new WasmBridgeError('INVALID_BRIDGE_VALUE', message);
}

function resourceLimit(): WasmBridgeError {
  return new WasmBridgeError(
    'RESOURCE_LIMIT',
    'A binary bridge call exceeded its resource limit',
  );
}
