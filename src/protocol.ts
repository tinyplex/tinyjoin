import {
  arrayIsArray,
  isBoolean,
  isCount,
  isCountWithin,
  isFiniteNumber,
  isNumber,
  isObject,
  isPlainRecord,
  objKeys,
  isRecord,
  isString,
  isUndefined,
  MAX_CHANGED_KEYS_PER_TABLE,
  MAX_U32,
  objHasOwn,
  objValues,
} from './common.js';

export const PROTOCOL_VERSION = 12 as const;

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
  /** TinyJoin extension: primary keys changed by this statement, per table. */
  keys: ChangedKeys;
}

/** One of the five runtime types a column holds. */
export type ColumnType = 'boolean' | 'integer' | 'float' | 'text' | 'json';

export interface ColumnSchema {
  name: string;
  type: ColumnType;
  nullable: boolean;
  /** The literal `DEFAULT`, present only when the column declares one. */
  default?: JsonValue;
  /** The most characters a `VARCHAR(n)` column holds. */
  maxLength?: number;
  /** A name the column had, which setSchema renames it from. */
  renamedFrom?: string;
}

export interface IndexSchema {
  name: string;
  columns: string[];
  unique: boolean;
}

/** What deleting or updating a referenced row does to the rows that reference it. */
export type ForeignKeyAction =
  | 'no action'
  | 'restrict'
  | 'cascade'
  | 'set null'
  | 'set default';

export interface ForeignKeySchema {
  name: string;
  columns: string[];
  references: string;
  referencedColumns: string[];
  onDelete: ForeignKeyAction;
  onUpdate: ForeignKeyAction;
}

export interface TableSchema {
  name: string;
  columns: ColumnSchema[];
  primaryKey: string[];
  indexes: IndexSchema[];
  foreignKeys: ForeignKeySchema[];
  /** A name the table had, which setSchema renames it from. */
  renamedFrom?: string;
}

/** The database's tables, in name order, as the engine's catalog holds them. */
export interface Schema {
  version: number;
  tables: TableSchema[];
}

export interface SetSchemaOptions {
  drop?: boolean;
}

export type StorageOptions = {kind: 'memory'} | {kind: 'opfs'; name: string};

/**
 * The primary keys changed in each table, for the tables whose complete set is known.
 *
 * A table is present only when every key it changed fits the engine's per-table bound; a table
 * that changed more rows than that is absent, so an absent table means "changed, re-read it"
 * rather than "unchanged". The changed-table list stays authoritative either way.
 */
export type ChangedKeys = {[table: string]: Row[]};

export interface ApplyOutcome {
  revision: number;
  tables: string[];
  keys: ChangedKeys;
}

/**
 * A statement's result as the page reads it. Its fields and rows travel as
 * JSON text that only the page parses, `{"fields": [...], "rows": [...]}`,
 * with each row's values in field order: an object keyed by field name, or an
 * array when the request asked for `rowMode: 'array'`.
 *
 * A result that published nothing, and has a shape a
 * {@link StatementResponse} can hold, crosses from the Worker as that flat
 * array instead; one that committed crosses as this object, which the Worker
 * read to publish its changes.
 */
export interface SqlResult {
  command: string;
  revision: number;
  rowCount: number;
  tables: string[];
  keys: ChangedKeys;
  data: string;
}

/** The fields and rows a {@link SqlResult} carries as JSON text. */
export interface SqlData {
  fields: ResultField[];
  rows: (Row | JsonValue[])[];
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
    request: {
      sql: string;
      params: JsonValue[];
      transactionId?: string;
      rowMode?: RowMode;
    };
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
      rowMode?: RowMode;
    };
    response: SqlResult;
  };
  closePrepared: {
    request: {statementId: number};
    response: undefined;
  };
  execSql: {
    request: {sql: string; transactionId?: string; rowMode?: RowMode};
    response: SqlResult[];
  };
  beginTransaction: {
    request: undefined;
    response: {transactionId: string};
  };
  commitTransaction: {
    request: {transactionId: string};
    /**
     * The revision the commit published. The tables and keys it changed reach
     * subscribers in the change event that follows, so the response does not
     * carry them a second time.
     */
    response: {revision: number};
  };
  rollbackTransaction: {
    request: {transactionId: string};
    response: undefined;
  };
  check: {
    request: undefined;
    response: undefined;
  };
  schema: {
    request: undefined;
    response: Schema;
  };
  setSchema: {
    request: {schema: Schema; drop: boolean};
    response: boolean;
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

/** The operation a {@link StatementRequest} asks for: a statement's SQL text. */
export const STATEMENT_SQL = 1;
/** The operation a {@link StatementRequest} asks for: a prepared statement. */
export const STATEMENT_PREPARED = 4;
/** Where a {@link StatementRequest}'s parameters begin. */
export const STATEMENT_PARAMS = 6;

/**
 * A statement sent as one flat array, rather than as a {@link WorkerRequest}:
 * its `executeSql` or `executePrepared` request with every value in a fixed
 * place and no names. A structured clone copies an array of scalars in about
 * half the time it copies the request object, and both ends read it by index.
 *
 * The slots are the protocol version, the request's id, the operation
 * ({@link STATEMENT_SQL} or {@link STATEMENT_PREPARED}), the SQL text or the
 * prepared statement's id, the transaction's id or `0` outside one, `1` for
 * array rows or `0` for object rows, and then the statement's parameters,
 * from {@link STATEMENT_PARAMS} on.
 */
export type StatementRequest = [
  v: typeof PROTOCOL_VERSION,
  id: number,
  operation: typeof STATEMENT_SQL | typeof STATEMENT_PREPARED,
  target: string | number,
  transaction: string | 0,
  arrayRows: 0 | 1,
  ...params: JsonValue[],
];

/**
 * The commands a {@link StatementResponse} names by number: each is at its
 * number's place.
 */
export const STATEMENT_COMMANDS = ['INSERT', 'UPDATE', 'DELETE', 'SELECT'] as const;
/** The number of `SELECT` in {@link STATEMENT_COMMANDS}. */
export const STATEMENT_SELECT = 3;

/**
 * The response to a statement whose result published nothing, as one flat
 * array rather than a {@link WorkerResponse} holding a {@link SqlResult}:
 * every statement inside a transaction, every read, and a write outside a
 * transaction that changed no row. Nothing in it is JSON text for either
 * thread to write or parse, apart from a read's rows.
 *
 * The first five slots are the protocol version, the request's id, the
 * command's number in {@link STATEMENT_COMMANDS}, the revision, and the row
 * count. What follows depends on the command. A `SELECT` has one more slot,
 * its fields and rows as the JSON text {@link SqlResult.data} holds. A write
 * that changed no table has none. A write that changed a table has the
 * table's name, and, when the table's changed keys are reported, the number of
 * columns in its primary key, those columns' names, and then each changed
 * key's values in that order, one key after another.
 *
 * A result of any other shape, such as one with `RETURNING` rows, or one that
 * changed several tables, crosses as a {@link SqlResult} in a
 * {@link WorkerResponse}, as a result that committed does.
 */
export type StatementResponse = [
  v: typeof PROTOCOL_VERSION,
  id: number,
  command: number,
  revision: number,
  rowCount: number,
  ...rest: JsonPrimitive[],
];

/**
 * A {@link StatementResponse} before it is posted, as the engine's bridge
 * returns it: its first two slots, the version and the request's id, are
 * filled by whoever posts it.
 */
export type StatementResult = [
  v: number,
  id: number,
  command: number,
  revision: number,
  rowCount: number,
  ...rest: JsonPrimitive[],
];

const MAX_ARRAY_ITEMS = 1_000_000;
const MAX_TRANSACTION_ID_LENGTH = 128;
const SQL_RESULT_KEYS = [
  'command',
  'revision',
  'rowCount',
  'tables',
  'keys',
  'data',
] as const;

// A response has exactly its four keys: each is checked by name, so counting
// the keys is what confirms there is no other, without walking them.
export const isWorkerResponse = (value: unknown): value is WorkerResponse =>
  isEnvelope(value) &&
  isRequestId(value.id) &&
  objKeys(value).length === 4 &&
  (value.ok === true
    ? objHasOwn(value, 'result')
    : value.ok === false && isSerializedError(value.error));

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
        hasParams(params, ['sql', 'params', 'transactionId', 'rowMode']) &&
        isString(params.sql) &&
        isJsonValues(params.params) &&
        isOptionalTransactionId(params.transactionId) &&
        isOptionalRowMode(params.rowMode)
      );
    case 'prepareSql':
      return hasExactParams(params, ['sql']) && isString(params.sql);
    case 'executePrepared':
      return (
        hasParams(params, ['statementId', 'params', 'transactionId', 'rowMode']) &&
        isPreparedStatementId(params.statementId) &&
        isJsonValues(params.params) &&
        isOptionalTransactionId(params.transactionId) &&
        isOptionalRowMode(params.rowMode)
      );
    case 'closePrepared':
      return (
        hasExactParams(params, ['statementId']) &&
        isPreparedStatementId(params.statementId)
      );
    case 'execSql':
      return (
        hasParams(params, ['sql', 'transactionId', 'rowMode']) &&
        isString(params.sql) &&
        isOptionalTransactionId(params.transactionId) &&
        isOptionalRowMode(params.rowMode)
      );
    case 'commitTransaction':
    case 'rollbackTransaction':
      return (
        hasParams(params, ['transactionId']) &&
        isTransactionId(params.transactionId)
      );
    case 'beginTransaction':
    case 'check':
    case 'schema':
    case 'close':
      return isUndefined(params);
    // The engine reads the schema's shape, and says what is wrong with it.
    case 'setSchema':
      return (
        hasParams(params, ['schema', 'drop']) &&
        createJsonValidation().isJson(params.schema) &&
        isBoolean(params.drop)
      );
    default:
      return false;
  }
};

/**
 * Whether `value` is a statement request: an array, with each of its fixed
 * slots as {@link StatementRequest} gives it, and parameters that
 * {@link isWorkerRequest} would accept as a statement's.
 */
export const isStatementRequest = (value: unknown): value is StatementRequest => {
  if (!arrayIsArray(value) || value.length < STATEMENT_PARAMS) {
    return false;
  }
  const length = value.length;
  const operation: unknown = value[2];
  const target: unknown = value[3];
  const transaction: unknown = value[4];
  const arrayRows: unknown = value[5];
  if (
    value[0] !== PROTOCOL_VERSION ||
    !isRequestId(value[1]) ||
    !(operation === STATEMENT_PREPARED
      ? isPreparedStatementId(target)
      : operation === STATEMENT_SQL && typeof target === 'string') ||
    !(transaction === 0 || isTransactionId(transaction)) ||
    !(arrayRows === 0 || arrayRows === 1) ||
    length - STATEMENT_PARAMS > MAX_ARRAY_ITEMS
  ) {
    return false;
  }
  // Nearly every parameter is a scalar, which is checked where it stands. A
  // hole reads as undefined, which no scalar is, and so falls to the full
  // check, which refuses it.
  for (let index = STATEMENT_PARAMS; index < length; index++) {
    const item: unknown = value[index];
    if (typeof item === 'number') {
      if (!Number.isFinite(item)) {
        return false;
      }
    } else if (
      !(item === null || typeof item === 'boolean' || typeof item === 'string')
    ) {
      const validation = createJsonValidation();
      for (let rest = STATEMENT_PARAMS; rest < length; rest++) {
        if (!objHasOwn(value, rest) || !validation.isJson(value[rest])) {
          return false;
        }
      }
      return true;
    }
  }
  return true;
};

/**
 * The most primary-key columns a {@link StatementResponse} may list, which is
 * more than a table may declare.
 */
const MAX_KEY_COLUMNS = 255;

const MAX_SAFE_INTEGER = Number.MAX_SAFE_INTEGER;

/**
 * Whether an array a Worker posted is a statement response, in everything the
 * Worker itself wrote: its fixed slots, and the shape its command gives the
 * rest. A `SELECT`'s rows are JSON text the engine wrote and the Worker passed
 * on unread, which {@link isSqlDataText} checks apart, so that text the page
 * cannot read fails its own statement rather than the Worker.
 *
 * Without `deep`, as for the Worker TinyJoin ships, the changed keys' columns
 * and values are left unread, as a result's header check leaves them. With
 * `deep`, as for a Worker an application supplied, every one of them is
 * checked, and no more keys may be listed than a table reports.
 *
 * The page runs this for every statement, mostly before its code has been
 * optimized, where each helper is a call that costs more than the check it
 * makes. So the id and the counts are tested here with comparisons, which say
 * what isRequestId(), isCount() and isCountWithin() say: a number that is an
 * integer, within the safe range, and at least the least it may be.
 */
export const isStatementResponse = (
  value: readonly unknown[],
  deep: boolean,
): value is StatementResponse => {
  const length = value.length;
  const id: unknown = value[1];
  const command: unknown = value[2];
  const revision: unknown = value[3];
  const rowCount: unknown = value[4];
  if (
    length < 5 ||
    value[0] !== PROTOCOL_VERSION ||
    !(
      typeof id === 'number' &&
      id >= 1 &&
      id <= MAX_SAFE_INTEGER &&
      id % 1 === 0
    ) ||
    !(
      typeof revision === 'number' &&
      revision >= 0 &&
      revision <= MAX_SAFE_INTEGER &&
      revision % 1 === 0
    ) ||
    !(
      typeof rowCount === 'number' &&
      rowCount >= 0 &&
      rowCount <= MAX_SAFE_INTEGER &&
      rowCount % 1 === 0
    )
  ) {
    return false;
  }
  if (command === STATEMENT_SELECT) {
    return length === 6 && typeof value[5] === 'string';
  }
  if (command !== 0 && command !== 1 && command !== 2) {
    return false;
  }
  if (length === 5) {
    return true;
  }
  if (typeof value[5] !== 'string') {
    return false;
  }
  if (length === 6) {
    return true;
  }
  const width: unknown = value[6];
  if (
    !(
      typeof width === 'number' &&
      width >= 1 &&
      width <= MAX_KEY_COLUMNS &&
      width % 1 === 0
    )
  ) {
    return false;
  }
  const values = length - 7 - width;
  if (values < 0 || values % width !== 0) {
    return false;
  }
  if (!deep) {
    return true;
  }
  if (values / width > MAX_CHANGED_KEYS_PER_TABLE) {
    return false;
  }
  for (let index = 7; index < length; index++) {
    const item: unknown = value[index];
    if (!objHasOwn(value, index)) {
      return false;
    }
    if (index < 7 + width) {
      if (typeof item !== 'string') {
        return false;
      }
    } else if (typeof item === 'number') {
      if (!Number.isFinite(item)) {
        return false;
      }
    } else if (
      !(item === null || typeof item === 'boolean' || typeof item === 'string')
    ) {
      return false;
    }
  }
  return true;
};

/**
 * Whether `text` is a result's fields and rows as JSON, parsed and walked in
 * full, as the full check of a {@link SqlResult} walks its `data`.
 */
export const isSqlDataText = (text: string): boolean =>
  isSqlData(parseSqlData(text), createJsonValidation());

export const isRpcResult = <Method extends RpcMethod>(
  method: Method,
  value: unknown,
): value is RpcMethods[Method]['response'] => isResult(method, value, true);

/**
 * Checks only the fixed result envelope produced by TinyJoin's bundled Worker,
 * which writes every result's rows itself. Leaving the rows' JSON text unread
 * here keeps large row sets off the UI thread's hot path until they are used.
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
    case 'check':
    case 'close':
      return isUndefined(value);
    case 'commitTransaction':
      return (
        isRecord(value) &&
        hasExactKeys(value, ['revision']) &&
        isCount(value.revision)
      );
    case 'schema':
      return isSchema(value);
    case 'setSchema':
      return isBoolean(value);
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

// Every message arrives as a structured clone or parsed JSON, whose own keys
// are exactly its enumerable string keys, so Object.keys reads them all, and
// far faster than Reflect.ownKeys would.
const hasOnlyKeys = (
  value: Record<string, unknown>,
  allowedKeys: readonly string[],
): boolean => areKeysWithin(objKeys(value), allowedKeys);

// Keys are distinct, so as many keys as expected, each expected, are exactly
// those expected.
const hasExactKeys = (
  value: Record<string, unknown>,
  expectedKeys: readonly string[],
): boolean => {
  const keys = objKeys(value);
  return (
    keys.length === expectedKeys.length && areKeysWithin(keys, expectedKeys)
  );
};

// Every message is checked this way, so the keys are read once and walked
// without a callback. They usually come in the order they are listed.
const areKeysWithin = (
  keys: readonly string[],
  allowedKeys: readonly string[],
): boolean => {
  for (let index = 0; index < keys.length; index++) {
    const key = keys[index]!;
    if (key !== allowedKeys[index] && !allowedKeys.includes(key)) {
      return false;
    }
  }
  return true;
};

const isOptionalTransactionId = (value: unknown): boolean =>
  isUndefined(value) || isTransactionId(value);

const isOptionalRowMode = (value: unknown): boolean =>
  isUndefined(value) || value === 'array' || value === 'object';

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

// Nearly every parameter is a scalar, which needs no validation context. Any
// container sends the whole array through the full check.
const isJsonValues = (value: unknown): value is JsonValue[] => {
  if (!arrayIsArray(value) || value.length > MAX_ARRAY_ITEMS) {
    return false;
  }
  for (let index = 0; index < value.length; index++) {
    if (!objHasOwn(value, index)) {
      return false;
    }
    const item: unknown = value[index];
    if (isNumber(item)) {
      if (!isFiniteNumber(item)) {
        return false;
      }
    } else if (!(item === null || isBoolean(item) || isString(item))) {
      return isDenseArray(value, createJsonValidation().isJson);
    }
  }
  return true;
};

const COLUMN_TYPES: readonly unknown[] = [
  'boolean',
  'integer',
  'float',
  'text',
  'json',
];

const FOREIGN_KEY_ACTIONS: readonly unknown[] = [
  'no action',
  'restrict',
  'cascade',
  'set null',
  'set default',
];

const isSchema = (value: unknown): value is Schema => {
  const validation = createJsonValidation();
  const isColumn = (column: unknown): boolean =>
    isRecord(column) &&
    hasOnlyKeys(column, ['name', 'type', 'nullable', 'default', 'maxLength']) &&
    isString(column.name) &&
    COLUMN_TYPES.includes(column.type) &&
    isBoolean(column.nullable) &&
    (!objHasOwn(column, 'default') || validation.isJson(column.default)) &&
    (!objHasOwn(column, 'maxLength') ||
      (column.type === 'text' && isCount(column.maxLength) && column.maxLength > 0));
  const isIndex = (index: unknown): boolean =>
    isRecord(index) &&
    hasExactKeys(index, ['name', 'columns', 'unique']) &&
    isString(index.name) &&
    isStrings(index.columns) &&
    isBoolean(index.unique);
  const isForeignKey = (key: unknown): boolean =>
    isRecord(key) &&
    hasExactKeys(key, [
      'name',
      'columns',
      'references',
      'referencedColumns',
      'onDelete',
      'onUpdate',
    ]) &&
    isString(key.name) &&
    isStrings(key.columns) &&
    isString(key.references) &&
    isStrings(key.referencedColumns) &&
    FOREIGN_KEY_ACTIONS.includes(key.onDelete) &&
    FOREIGN_KEY_ACTIONS.includes(key.onUpdate);
  const isTable = (table: unknown): boolean =>
    isRecord(table) &&
    hasExactKeys(table, [
      'name',
      'columns',
      'primaryKey',
      'indexes',
      'foreignKeys',
    ]) &&
    isString(table.name) &&
    isDenseArray(table.columns, isColumn) &&
    isStrings(table.primaryKey) &&
    isDenseArray(table.indexes, isIndex) &&
    isDenseArray(table.foreignKeys, isForeignKey);
  return (
    isRecord(value) &&
    hasExactKeys(value, ['version', 'tables']) &&
    isCount(value.version) &&
    isDenseArray(value.tables, isTable)
  );
};

/**
 * Whether `value` is the outcome of a commit, as an event carries one and as
 * the engine reports one: its revision, and the tables and keys it changed.
 */
export const isApplyOutcome = (value: unknown): value is ApplyOutcome =>
  isRecord(value) &&
  hasExactKeys(value, ['revision', 'tables', 'keys']) &&
  isCount(value.revision) &&
  isStrings(value.tables) &&
  isChangedKeys(value.keys);

// Changed keys are reported only for tables that also appear in `tables`, so the walk is bounded
// by the same per-table key bound the engine applied when producing them.
const isChangedKeys = (
  value: unknown,
  validation?: JsonValidation,
): value is ChangedKeys =>
  isPlainRecord(value) &&
  objValues(value).every((rows) =>
    validation
      ? isDenseArray(rows, (row) => isRow(row, validation))
      : arrayIsArray(rows),
  );

const isRow = (value: unknown, validation: JsonValidation): value is Row =>
  validation.step() &&
  isPlainRecord(value) &&
  objValues(value).every(validation.isJson);

const isResultField = (value: unknown): value is ResultField =>
  isRecord(value) &&
  hasExactKeys(value, ['name', 'dataTypeID']) &&
  isString(value.name) &&
  isCountWithin(value.dataTypeID, 0, MAX_U32);

// A validation context also parses the rows' JSON text and walks every field
// and row. The header-only pass leaves the text to whoever produced it.
// Each of a result's keys is checked by name below, so counting them is what
// confirms there is no other, without walking them.
const isSqlResult = (
  value: unknown,
  validation?: JsonValidation,
): value is SqlResult =>
  (!validation || validation.step()) &&
  isRecord(value) &&
  objKeys(value).length === SQL_RESULT_KEYS.length &&
  isString(value.command) &&
  isCount(value.revision) &&
  isCount(value.rowCount) &&
  isStrings(value.tables, validation?.step) &&
  isChangedKeys(value.keys, validation) &&
  isString(value.data) &&
  (!validation || isSqlData(parseSqlData(value.data), validation));

/** Parses a result's rows, or returns undefined if they are not JSON. */
export const parseSqlData = (text: string): unknown => {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
};

const isSqlData = (
  value: unknown,
  validation: JsonValidation,
): value is SqlData =>
  isRecord(value) &&
  hasExactKeys(value, ['fields', 'rows']) &&
  isDenseArray(value.fields, isResultField, validation.step) &&
  isDenseArray(value.rows, (row) =>
    arrayIsArray(row)
      ? validation.step() && isDenseArray(row, validation.isJson)
      : isRow(row, validation),
  );

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
