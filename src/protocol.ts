export const PROTOCOL_VERSION = 5 as const;
export const MAX_QUERY_POSITION = 0xffff_ffff;

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
  /** TinyGres extension: the database revision observed by this statement. */
  revision: number;
  /** TinyGres extension: tables changed by this statement. */
  tables: string[];
}

export interface TableSchema {
  name: string;
  primaryKey: string[];
}

export type StorageOptions = {kind: 'memory'} | {kind: 'opfs'; name: string};

export type Change =
  | {type: 'upsert'; table: string; row: Row}
  | {type: 'delete'; table: string; key: Row};

export interface ChangeBatch {
  changes: Change[];
}

export type Filter = {
  column: string;
  operator: 'eq' | 'neq' | 'lt' | 'lte' | 'gt' | 'gte';
  value: JsonValue;
};

export interface QueryPlan {
  table: string;
  columns?: string[];
  filters: Filter[];
  orderBy?: OrderBy[];
  limit?: number;
  offset?: number;
}

export interface OrderBy {
  column: string;
  direction: 'asc' | 'desc';
  nulls: 'default' | 'first' | 'last';
}

export interface ApplyOutcome {
  revision: number;
  tables: string[];
}

export interface QueryResult<RowType extends object = Row> {
  revision: number;
  rows: RowType[];
  fields: ResultField[];
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
    request: {
      schemas: TableSchema[];
      storage: StorageOptions;
    };
    response: {revision: number};
  };
  defineTable: {
    request: {schema: TableSchema};
    response: undefined;
  };
  replaceTable: {
    request: {schema: TableSchema; rows: Row[]};
    response: ApplyOutcome;
  };
  applyBatch: {
    request: {batch: ChangeBatch};
    response: ApplyOutcome;
  };
  query: {
    request: {plan: QueryPlan; transactionId?: string};
    response: QueryResult;
  };
  executeSql: {
    request: {sql: string; params: JsonValue[]; transactionId?: string};
    response: SqlResult;
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
    case 'defineTable':
    case 'rollbackTransaction':
    case 'close':
      return value === undefined;
    case 'replaceTable':
    case 'applyBatch':
    case 'commitTransaction':
      return isApplyOutcome(value);
    case 'query':
      return isQueryResult(value);
    case 'executeSql':
      return isSqlResult(value);
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
        hasExactKeys(value.params, ['schemas', 'storage']) &&
        isDenseArray(value.params.schemas, isTableSchema) &&
        isStorageOptions(value.params.storage)
      );
    case 'defineTable':
      return (
        isRecord(value.params) &&
        hasExactKeys(value.params, ['schema']) &&
        isTableSchema(value.params.schema)
      );
    case 'replaceTable':
      return (
        isRecord(value.params) &&
        hasExactKeys(value.params, ['schema', 'rows']) &&
        isTableSchema(value.params.schema) &&
        isDenseArray(value.params.rows, isRow)
      );
    case 'applyBatch':
      return (
        isRecord(value.params) &&
        hasExactKeys(value.params, ['batch']) &&
        isChangeBatch(value.params.batch)
      );
    case 'query':
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ['plan', 'transactionId']) &&
        isQueryPlan(value.params.plan) &&
        isOptionalTransactionId(value.params.transactionId)
      );
    case 'executeSql':
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ['sql', 'params', 'transactionId']) &&
        typeof value.params.sql === 'string' &&
        isDenseArray(value.params.params, (param) => isJsonValue(param)) &&
        isOptionalTransactionId(value.params.transactionId)
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

function isOrderBy(value: unknown): value is OrderBy {
  return (
    isRecord(value) &&
    hasOnlyKeys(value, ['column', 'direction', 'nulls']) &&
    typeof value.column === 'string' &&
    value.column.length > 0 &&
    (value.direction === 'asc' || value.direction === 'desc') &&
    (value.nulls === 'default' ||
      value.nulls === 'first' ||
      value.nulls === 'last')
  );
}

function isOptionalTransactionId(value: unknown): boolean {
  return value === undefined || isTransactionId(value);
}

function isTransactionId(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0 && value.length <= 128;
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

function isTableSchema(value: unknown): value is TableSchema {
  return (
    isRecord(value) &&
    hasExactKeys(value, ['name', 'primaryKey']) &&
    typeof value.name === 'string' &&
    value.name.length > 0 &&
    isDenseArray(
      value.primaryKey,
      (column): column is string =>
        typeof column === 'string' && column.length > 0,
    ) &&
    value.primaryKey.length > 0
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
    Number(value.dataTypeID) <= MAX_QUERY_POSITION
  );
}

function isQueryResult(value: unknown): value is QueryResult {
  return (
    isRecord(value) &&
    hasExactKeys(value, ['revision', 'rows', 'fields']) &&
    isSafeNonNegativeInteger(value.revision) &&
    isDenseArray(value.rows, isRow) &&
    isDenseArray(value.fields, isResultField)
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

function isChangeBatch(value: unknown): value is ChangeBatch {
  return (
    isRecord(value) &&
    hasExactKeys(value, ['changes']) &&
    isDenseArray(value.changes, isChange)
  );
}

function isChange(value: unknown): value is Change {
  if (!isRecord(value) || typeof value.table !== 'string') {
    return false;
  }
  return value.type === 'upsert'
    ? hasExactKeys(value, ['type', 'table', 'row']) && isRow(value.row)
    : value.type === 'delete' &&
        hasExactKeys(value, ['type', 'table', 'key']) &&
        isRow(value.key);
}

function isQueryPlan(value: unknown): value is QueryPlan {
  if (
    !isRecord(value) ||
    !hasOnlyKeys(value, [
      'table',
      'columns',
      'filters',
      'orderBy',
      'limit',
      'offset',
    ]) ||
    typeof value.table !== 'string' ||
    !Array.isArray(value.filters)
  ) {
    return false;
  }
  if (
    value.columns !== undefined &&
    !isDenseArray(
      value.columns,
      (column): column is string => typeof column === 'string',
    )
  ) {
    return false;
  }
  if (
    value.limit !== undefined &&
    (!Number.isSafeInteger(value.limit) ||
      Number(value.limit) < 0 ||
      Number(value.limit) > MAX_QUERY_POSITION)
  ) {
    return false;
  }
  if (
    value.offset !== undefined &&
    (!Number.isSafeInteger(value.offset) ||
      Number(value.offset) < 0 ||
      Number(value.offset) > MAX_QUERY_POSITION)
  ) {
    return false;
  }
  if (
    value.offset !== undefined &&
    value.limit !== undefined &&
    Number(value.offset) + Number(value.limit) > MAX_QUERY_POSITION
  ) {
    return false;
  }
  if (
    value.orderBy !== undefined &&
    (!Array.isArray(value.orderBy) ||
      value.orderBy.length > 32 ||
      !isDenseArray(value.orderBy, isOrderBy))
  ) {
    return false;
  }
  return isDenseArray(
    value.filters,
    (filter): filter is Filter =>
      isRecord(filter) &&
      hasExactKeys(filter, ['column', 'operator', 'value']) &&
      typeof filter.column === 'string' &&
      FILTER_OPERATORS.has(filter.operator) &&
      isJsonValue(filter.value),
  );
}

const FILTER_OPERATORS = new Set<unknown>([
  'eq',
  'neq',
  'lt',
  'lte',
  'gt',
  'gte',
]);

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
