export const PROTOCOL_VERSION = 7 as const;
const MAX_U32 = 0xffff_ffff;

export type JsonPrimitive = null | boolean | number | string;
export type JsonValue =
  | JsonPrimitive
  | JsonValue[]
  | {[key: string]: JsonValue};
export type Row = Record<string, JsonValue>;

export type RowMode = 'array' | 'object';

export interface QueryOptions {
  rowMode?: RowMode;
}

export interface ResultField {
  name: string;
  dataTypeID: number;
}

/** The result returned by the public SQL API. */
export interface Results<RowType = Row> {
  rows: RowType[];
  fields: ResultField[];
  affectedRows?: number;
  command?: string;
  rowCount?: number;
  /** TinyJoin extension: the database revision observed by this statement. */
  revision: number;
  /** TinyJoin extension: tables changed by this statement. */
  tables: string[];
}

export type StorageOptions = {kind: 'memory'} | {kind: 'opfs'; name: string};

export interface ApplyOutcome {
  revision: number;
  tables: string[];
}

export interface SqlResult<RowType extends object = Row> {
  command: string;
  fields: ResultField[];
  revision: number;
  rowCount: number;
  rows: RowType[];
  tables: string[];
}

export interface SerializedError {
  code: string;
  message: string;
  details?: JsonValue;
  retryable?: boolean;
}

export interface RpcMethods {
  init: {
    request: {storage: StorageOptions};
    response: {revision: number};
  };
  executeSql: {
    request: {sql: string; params: JsonValue[]; transactionId?: string};
    response: SqlResult;
  };
  prepareSql: {
    request: {sql: string};
    response: {statementId: number};
  };
  executePrepared: {
    request: {
      statementId: number;
      params: JsonValue[];
      transactionId?: string;
    };
    response: SqlResult;
  };
  closePrepared: {
    request: {statementId: number};
    response: undefined;
  };
  execSql: {
    request: {sql: string; transactionId?: string};
    response: SqlResult[];
  };
  beginTransaction: {
    request: undefined;
    response: {transactionId: string};
  };
  commitTransaction: {
    request: {transactionId: string};
    response: ApplyOutcome;
  };
  rollbackTransaction: {
    request: {transactionId: string};
    response: undefined;
  };
  close: {
    request: undefined;
    response: undefined;
  };
}

export type RpcMethod = keyof RpcMethods;

export type WorkerRequest = {
  [Method in RpcMethod]: {
    v: typeof PROTOCOL_VERSION;
    id: number;
    method: Method;
    params: RpcMethods[Method]['request'];
  };
}[RpcMethod];

export type WorkerResponse =
  | {
      v: typeof PROTOCOL_VERSION;
      id: number;
      ok: true;
      result: unknown;
    }
  | {
      v: typeof PROTOCOL_VERSION;
      id: number;
      ok: false;
      error: SerializedError;
    };

export type WorkerEvent = {
  v: typeof PROTOCOL_VERSION;
  event: 'tablesChanged';
  payload: ApplyOutcome;
};

export function isWorkerResponse(value: unknown): value is WorkerResponse {
  if (
    !isRecord(value) ||
    value.v !== PROTOCOL_VERSION ||
    !isSafeNonNegativeInteger(value.id) ||
    Number(value.id) < 1
  ) {
    return false;
  }
  if (value.ok === true) {
    return hasExactKeys(value, ['v', 'id', 'ok', 'result']);
  }
  return (
    value.ok === false &&
    hasExactKeys(value, ['v', 'id', 'ok', 'error']) &&
    isSerializedError(value.error)
  );
}

export function isWorkerEvent(value: unknown): value is WorkerEvent {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, ['v', 'event', 'payload']) ||
    value.v !== PROTOCOL_VERSION
  ) {
    return false;
  }
  if (value.event === 'tablesChanged') {
    return isApplyOutcome(value.payload);
  }
  return false;
}

export function isRpcResult<Method extends RpcMethod>(
  method: Method,
  value: unknown,
): value is RpcMethods[Method]['response'] {
  switch (method) {
    case 'init':
      return (
        isRecord(value) &&
        hasExactKeys(value, ['revision']) &&
        isSafeNonNegativeInteger(value.revision)
      );
    case 'closePrepared':
    case 'rollbackTransaction':
    case 'close':
      return value === undefined;
    case 'commitTransaction':
      return isApplyOutcome(value);
    case 'executeSql':
    case 'executePrepared':
      return isSqlResult(value);
    case 'prepareSql':
      return (
        isRecord(value) &&
        hasExactKeys(value, ['statementId']) &&
        isPreparedStatementId(value.statementId)
      );
    case 'execSql':
      return isDenseArray(value, isSqlResult);
    case 'beginTransaction':
      return (
        isRecord(value) &&
        hasExactKeys(value, ['transactionId']) &&
        isTransactionId(value.transactionId)
      );
    default: {
      const exhaustive: never = method;
      return exhaustive;
    }
  }
}

/**
 * Checks only the fixed result envelope produced by TinyJoin's bundled Worker.
 * The Worker has already validated the complete WASM result before posting it;
 * avoiding another walk here keeps large row sets off the UI thread's hot path.
 */
export function isRpcResultHeader<Method extends RpcMethod>(
  method: Method,
  value: unknown,
): value is RpcMethods[Method]['response'] {
  switch (method) {
    case 'executeSql':
    case 'executePrepared':
      return isSqlResultHeader(value);
    case 'execSql':
      return Array.isArray(value) && value.every(isSqlResultHeader);
    default:
      return isRpcResult(method, value);
  }
}

export function isWorkerRequest(value: unknown): value is WorkerRequest {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, ['v', 'id', 'method', 'params']) ||
    value.v !== PROTOCOL_VERSION ||
    !Number.isSafeInteger(value.id) ||
    Number(value.id) < 1 ||
    typeof value.method !== 'string'
  ) {
    return false;
  }
  switch (value.method) {
    case 'init':
      return (
        isRecord(value.params) &&
        hasExactKeys(value.params, ['storage']) &&
        isStorageOptions(value.params.storage)
      );
    case 'executeSql':
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ['sql', 'params', 'transactionId']) &&
        typeof value.params.sql === 'string' &&
        isDenseArray(value.params.params, (param) => isJsonValue(param)) &&
        isOptionalTransactionId(value.params.transactionId)
      );
    case 'prepareSql':
      return (
        isRecord(value.params) &&
        hasExactKeys(value.params, ['sql']) &&
        typeof value.params.sql === 'string'
      );
    case 'executePrepared':
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, [
          'statementId',
          'params',
          'transactionId',
        ]) &&
        isPreparedStatementId(value.params.statementId) &&
        isDenseArray(value.params.params, (param) => isJsonValue(param)) &&
        isOptionalTransactionId(value.params.transactionId)
      );
    case 'closePrepared':
      return (
        isRecord(value.params) &&
        hasExactKeys(value.params, ['statementId']) &&
        isPreparedStatementId(value.params.statementId)
      );
    case 'execSql':
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ['sql', 'transactionId']) &&
        typeof value.params.sql === 'string' &&
        isOptionalTransactionId(value.params.transactionId)
      );
    case 'beginTransaction':
      return value.params === undefined;
    case 'commitTransaction':
    case 'rollbackTransaction':
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ['transactionId']) &&
        isTransactionId(value.params.transactionId)
      );
    case 'close':
      return value.params === undefined;
    default:
      return false;
  }
}

function isOptionalTransactionId(value: unknown): boolean {
  return value === undefined || isTransactionId(value);
}

function isTransactionId(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0 && value.length <= 128;
}

function isPreparedStatementId(value: unknown): value is number {
  return (
    Number.isSafeInteger(value) &&
    Number(value) > 0 &&
    Number(value) <= MAX_U32
  );
}

function isStorageOptions(value: unknown): value is StorageOptions {
  if (!isRecord(value)) {
    return false;
  }
  return value.kind === 'memory'
    ? hasExactKeys(value, ['kind'])
    : value.kind === 'opfs' &&
        hasExactKeys(value, ['kind', 'name']) &&
        typeof value.name === 'string';
}

function hasOnlyKeys(
  value: Record<string, unknown>,
  allowedKeys: readonly string[],
): boolean {
  return Reflect.ownKeys(value).every(
    (key) => typeof key === 'string' && allowedKeys.includes(key),
  );
}

function hasExactKeys(
  value: Record<string, unknown>,
  expectedKeys: readonly string[],
): boolean {
  const keys = Reflect.ownKeys(value);
  return (
    keys.length === expectedKeys.length &&
    keys.every(
      (key) => typeof key === 'string' && expectedKeys.includes(key),
    )
  );
}

function isDenseArray<T>(
  value: unknown,
  isItem: (item: unknown) => item is T,
): value is T[] {
  if (!Array.isArray(value) || value.length > MAX_PROTOCOL_ARRAY_ITEMS) {
    return false;
  }
  for (let index = 0; index < value.length; index++) {
    if (!Object.hasOwn(value, index) || !isItem(value[index])) {
      return false;
    }
  }
  return true;
}

const MAX_PROTOCOL_ARRAY_ITEMS = 1_000_000;

export function isSerializedError(value: unknown): value is SerializedError {
  return (
    isRecord(value) &&
    hasOnlyKeys(value, ['code', 'message', 'details', 'retryable']) &&
    Object.hasOwn(value, 'code') &&
    Object.hasOwn(value, 'message') &&
    typeof value.code === 'string' &&
    typeof value.message === 'string' &&
    (value.details === undefined || isJsonValue(value.details)) &&
    (value.retryable === undefined || typeof value.retryable === 'boolean')
  );
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isApplyOutcome(value: unknown): value is ApplyOutcome {
  return (
    isRecord(value) &&
    hasExactKeys(value, ['revision', 'tables']) &&
    isSafeNonNegativeInteger(value.revision) &&
    isDenseArray(
      value.tables,
      (table): table is string => typeof table === 'string',
    )
  );
}

function isRow(value: unknown): value is Row {
  return (
    isJsonRecord(value) &&
    Object.values(value).every((cell) => isJsonValue(cell))
  );
}

function isResultField(value: unknown): value is ResultField {
  return (
    isRecord(value) &&
    hasExactKeys(value, ['name', 'dataTypeID']) &&
    typeof value.name === 'string' &&
    isSafeNonNegativeInteger(value.dataTypeID) &&
    Number(value.dataTypeID) <= MAX_U32
  );
}

function isSqlResult(value: unknown): value is SqlResult {
  return (
    isRecord(value) &&
    hasExactKeys(value, [
      'command',
      'fields',
      'revision',
      'rowCount',
      'rows',
      'tables',
    ]) &&
    typeof value.command === 'string' &&
    isDenseArray(value.fields, isResultField) &&
    isSafeNonNegativeInteger(value.revision) &&
    isSafeNonNegativeInteger(value.rowCount) &&
    isDenseArray(value.rows, isRow) &&
    isDenseArray(
      value.tables,
      (table): table is string => typeof table === 'string',
    )
  );
}

function isSqlResultHeader(value: unknown): value is SqlResult {
  return (
    isRecord(value) &&
    hasExactKeys(value, [
      'command',
      'fields',
      'revision',
      'rowCount',
      'rows',
      'tables',
    ]) &&
    typeof value.command === 'string' &&
    Array.isArray(value.fields) &&
    isSafeNonNegativeInteger(value.revision) &&
    isSafeNonNegativeInteger(value.rowCount) &&
    Array.isArray(value.rows) &&
    isDenseArray(
      value.tables,
      (table): table is string => typeof table === 'string',
    )
  );
}

function isSafeNonNegativeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) >= 0;
}

function isJsonRecord(value: unknown): value is Record<string, unknown> {
  if (!isRecord(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return (
    (prototype === Object.prototype || prototype === null) &&
    Reflect.ownKeys(value).every((key) => typeof key === 'string')
  );
}

function isJsonValue(
  value: unknown,
  seen = new WeakSet<object>(),
  depth = 0,
): value is JsonValue {
  if (depth > 64) {
    return false;
  }
  if (
    value === null ||
    typeof value === 'boolean' ||
    typeof value === 'string'
  ) {
    return true;
  }
  if (typeof value === 'number') {
    return Number.isFinite(value);
  }
  if (typeof value !== 'object' || seen.has(value)) {
    return false;
  }
  seen.add(value);
  const valid = Array.isArray(value)
    ? isDenseArray(value, (item): item is JsonValue =>
        isJsonValue(item, seen, depth + 1),
      )
    : isJsonRecord(value) &&
      Object.values(value).every((item) => isJsonValue(item, seen, depth + 1));
  seen.delete(value);
  return valid;
}
