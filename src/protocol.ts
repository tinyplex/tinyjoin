import {
  arrayIsArray,
  isBoolean,
  isCount,
  isCountWithin,
  isFiniteNumber,
  isNumber,
  isObject,
  isPlainRecord,
  isRecord,
  isString,
  isUndefined,
  MAX_U32,
  objHasOwn,
  objValues,
  ownKeys,
} from './common.js';

export const PROTOCOL_VERSION = 7 as const;

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
  event: 'tablesChanged' | 'resync';
  payload: ApplyOutcome;
};

const MAX_ARRAY_ITEMS = 1_000_000;
const MAX_TRANSACTION_ID_LENGTH = 128;
const SQL_RESULT_KEYS = [
  'command',
  'fields',
  'revision',
  'rowCount',
  'rows',
  'tables',
] as const;

export const isWorkerResponse = (value: unknown): value is WorkerResponse =>
  isEnvelope(value) &&
  isRequestId(value.id) &&
  (value.ok === true
    ? hasExactKeys(value, ['v', 'id', 'ok', 'result'])
    : value.ok === false &&
      hasExactKeys(value, ['v', 'id', 'ok', 'error']) &&
      isSerializedError(value.error));

export const isWorkerEvent = (value: unknown): value is WorkerEvent =>
  isEnvelope(value) &&
  hasExactKeys(value, ['v', 'event', 'payload']) &&
  (value.event === 'tablesChanged' || value.event === 'resync') &&
  isApplyOutcome(value.payload);

export const isWorkerRequest = (value: unknown): value is WorkerRequest => {
  if (!isEnvelope(value) || !isRequestId(value.id)) {
    return false;
  }
  if (!hasExactKeys(value, ['v', 'id', 'method', 'params'])) {
    return false;
  }
  const params = value.params;
  switch (value.method) {
    case 'init':
      return (
        hasExactParams(params, ['storage']) && isStorageOptions(params.storage)
      );
    case 'executeSql':
      return (
        hasParams(params, ['sql', 'params', 'transactionId']) &&
        isString(params.sql) &&
        isJsonValues(params.params) &&
        isOptionalTransactionId(params.transactionId)
      );
    case 'prepareSql':
      return hasExactParams(params, ['sql']) && isString(params.sql);
    case 'executePrepared':
      return (
        hasParams(params, ['statementId', 'params', 'transactionId']) &&
        isPreparedStatementId(params.statementId) &&
        isJsonValues(params.params) &&
        isOptionalTransactionId(params.transactionId)
      );
    case 'closePrepared':
      return (
        hasExactParams(params, ['statementId']) &&
        isPreparedStatementId(params.statementId)
      );
    case 'execSql':
      return (
        hasParams(params, ['sql', 'transactionId']) &&
        isString(params.sql) &&
        isOptionalTransactionId(params.transactionId)
      );
    case 'commitTransaction':
    case 'rollbackTransaction':
      return (
        hasParams(params, ['transactionId']) &&
        isTransactionId(params.transactionId)
      );
    case 'beginTransaction':
    case 'close':
      return isUndefined(params);
    default:
      return false;
  }
};

export const isRpcResult = <Method extends RpcMethod>(
  method: Method,
  value: unknown,
): value is RpcMethods[Method]['response'] => isResult(method, value, true);

/**
 * Checks only the fixed result envelope produced by TinyJoin's bundled Worker.
 * The Worker has already validated the complete WASM result before posting it;
 * avoiding another walk here keeps large row sets off the UI thread's hot path.
 */
export const isRpcResultHeader = <Method extends RpcMethod>(
  method: Method,
  value: unknown,
): value is RpcMethods[Method]['response'] => isResult(method, value, false);

export const isSerializedError = (value: unknown): value is SerializedError =>
  isRecord(value) &&
  hasOnlyKeys(value, ['code', 'message', 'details', 'retryable']) &&
  objHasOwn(value, 'code') &&
  objHasOwn(value, 'message') &&
  isString(value.code) &&
  isString(value.message) &&
  (isUndefined(value.details) || createJsonValidation().isJson(value.details)) &&
  (isUndefined(value.retryable) || isBoolean(value.retryable));

// Each message carries the protocol version it was built for, so that a mixed
// pair of client and Worker fails loudly rather than misreading each other.
const isEnvelope = (value: unknown): value is Record<string, unknown> =>
  isRecord(value) && value.v === PROTOCOL_VERSION;

const isRequestId = (value: unknown): boolean => isCount(value) && value >= 1;

const isResult = (
  method: RpcMethod,
  value: unknown,
  deep: boolean,
): boolean => {
  const validation = deep ? createJsonValidation() : undefined;
  switch (method) {
    case 'init':
      return (
        isRecord(value) &&
        hasExactKeys(value, ['revision']) &&
        isCount(value.revision)
      );
    case 'closePrepared':
    case 'rollbackTransaction':
    case 'close':
      return isUndefined(value);
    case 'commitTransaction':
      return isApplyOutcome(value);
    case 'executeSql':
    case 'executePrepared':
      return isSqlResult(value, validation);
    case 'prepareSql':
      return (
        isRecord(value) &&
        hasExactKeys(value, ['statementId']) &&
        isPreparedStatementId(value.statementId)
      );
    case 'execSql':
      return deep
        ? isDenseArray(value, (result) => isSqlResult(result, validation))
        : arrayIsArray(value) && value.every((result) => isSqlResult(result));
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
};

const hasParams = (
  value: unknown,
  allowedKeys: readonly string[],
): value is Record<string, unknown> =>
  isRecord(value) && hasOnlyKeys(value, allowedKeys);

const hasExactParams = (
  value: unknown,
  expectedKeys: readonly string[],
): value is Record<string, unknown> =>
  isRecord(value) && hasExactKeys(value, expectedKeys);

const hasOnlyKeys = (
  value: Record<string, unknown>,
  allowedKeys: readonly string[],
): boolean =>
  ownKeys(value).every((key) => isString(key) && allowedKeys.includes(key));

const hasExactKeys = (
  value: Record<string, unknown>,
  expectedKeys: readonly string[],
): boolean =>
  ownKeys(value).length === expectedKeys.length &&
  hasOnlyKeys(value, expectedKeys);

const isOptionalTransactionId = (value: unknown): boolean =>
  isUndefined(value) || isTransactionId(value);

const isTransactionId = (value: unknown): value is string =>
  isString(value) && value.length > 0 && value.length <= MAX_TRANSACTION_ID_LENGTH;

const isPreparedStatementId = (value: unknown): value is number =>
  isCountWithin(value, 1, MAX_U32);

const isStorageOptions = (value: unknown): value is StorageOptions =>
  isRecord(value) &&
  (value.kind === 'memory'
    ? hasExactKeys(value, ['kind'])
    : value.kind === 'opfs' &&
      hasExactKeys(value, ['kind', 'name']) &&
      isString(value.name));

const isDenseArray = <Item>(
  value: unknown,
  isItem: (item: unknown) => boolean,
  step?: () => boolean,
): value is Item[] => {
  if (!arrayIsArray(value) || value.length > MAX_ARRAY_ITEMS) {
    return false;
  }
  for (let index = 0; index < value.length; index++) {
    if ((step && !step()) || !objHasOwn(value, index) || !isItem(value[index])) {
      return false;
    }
  }
  return true;
};

const isStrings = (value: unknown, step?: () => boolean): value is string[] =>
  isDenseArray(value, isString, step);

const isJsonValues = (value: unknown): value is JsonValue[] =>
  isDenseArray(value, createJsonValidation().isJson);

const isApplyOutcome = (value: unknown): value is ApplyOutcome =>
  isRecord(value) &&
  hasExactKeys(value, ['revision', 'tables']) &&
  isCount(value.revision) &&
  isStrings(value.tables);

const isRow = (value: unknown, validation: JsonValidation): value is Row =>
  validation.step() &&
  isPlainRecord(value) &&
  objValues(value).every(validation.isJson);

const isResultField = (value: unknown): value is ResultField =>
  isRecord(value) &&
  hasExactKeys(value, ['name', 'dataTypeID']) &&
  isString(value.name) &&
  isCountWithin(value.dataTypeID, 0, MAX_U32);

// A validation context walks every field and row. The header-only pass checks
// the envelope and leaves the two large arrays to whoever produced them.
const isSqlResult = (
  value: unknown,
  validation?: JsonValidation,
): value is SqlResult =>
  (!validation || validation.step()) &&
  isRecord(value) &&
  hasExactKeys(value, SQL_RESULT_KEYS) &&
  isString(value.command) &&
  (validation
    ? isDenseArray(value.fields, isResultField, validation.step)
    : arrayIsArray(value.fields)) &&
  isCount(value.revision) &&
  isCount(value.rowCount) &&
  (validation
    ? isDenseArray(value.rows, (row) => isRow(row, validation))
    : arrayIsArray(value.rows)) &&
  isStrings(value.tables, validation?.step);

type JsonValidation = ReturnType<typeof createJsonValidation>;

/** Shares one work budget and graph cache across a complete protocol payload. */
const createJsonValidation = () => {
  let remaining = MAX_JSON_VISITS;
  let ancestors: WeakSet<object> | undefined;
  let validatedDepths: WeakMap<object, number> | undefined;
  const step = (): boolean => remaining-- > 0;

  const visit = (value: unknown, depth: number): boolean => {
    if (!step() || depth > MAX_JSON_DEPTH) {
      return false;
    }
    if (value === null || isBoolean(value) || isString(value)) {
      return true;
    }
    if (isNumber(value)) {
      return isFiniteNumber(value);
    }
    if (!isObject(value) || ancestors?.has(value)) {
      return false;
    }
    // Structured clone preserves aliases. Reuse a subtree only when it was
    // already valid at this depth or deeper, so a later longer path cannot
    // hide a descendant beyond the depth limit. Expanded WASM input is still
    // charged separately by its preflight before any Rust allocation.
    if ((validatedDepths?.get(value) ?? -1) >= depth) {
      return true;
    }
    ancestors ??= new WeakSet<object>();
    validatedDepths ??= new WeakMap<object, number>();
    ancestors.add(value);
    const valid = arrayIsArray(value)
      ? isDenseArray(value, (item) => visit(item, depth + 1))
      : isPlainRecord(value) &&
        objValues(value).every((item) => visit(item, depth + 1));
    ancestors.delete(value);
    if (valid) {
      validatedDepths.set(value, depth);
    }
    return valid;
  };

  return {
    step,
    isJson: (value: unknown): value is JsonValue => visit(value, 0),
  };
};

const MAX_JSON_DEPTH = 64;
const MAX_JSON_VISITS = 1_000_000;
