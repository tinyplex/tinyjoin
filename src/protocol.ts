export const PROTOCOL_VERSION = 1 as const;

export type JsonPrimitive = null | boolean | number | string;
export type JsonValue =
  | JsonPrimitive
  | JsonValue[]
  | {[key: string]: JsonValue};
export type Row = Record<string, JsonValue>;

export interface TableSchema {
  name: string;
  primaryKey: string[];
}

export interface SourceCursor {
  kind: string;
  value: string;
}

export type Change =
  | {type: 'upsert'; table: string; row: Row}
  | {type: 'delete'; table: string; key: Row};

export interface ChangeBatch {
  sourceId?: string;
  cursor?: SourceCursor;
  transactionId?: string;
  committedAt?: string;
  changes: Change[];
}

export type Filter = {
  column: string;
  operator: 'eq';
  value: JsonValue;
};

export interface QueryPlan {
  table: string;
  columns?: string[];
  filters: Filter[];
  limit?: number;
}

export interface ApplyOutcome {
  revision: number;
  tables: string[];
}

export interface QueryResult<RowType extends object = Row> {
  revision: number;
  rows: RowType[];
}

export interface SerializedError {
  code: string;
  message: string;
  details?: JsonValue;
  retryable?: boolean;
}

export type SyncPhase =
  | 'idle'
  | 'connecting'
  | 'snapshotting'
  | 'live-best-effort'
  | 'live-durable'
  | 'stale'
  | 'resyncing'
  | 'locked'
  | 'error';

export interface SyncState {
  phase: SyncPhase;
  sourceId?: string;
  lastReconciledAt?: string;
  error?: SerializedError;
}

export interface RpcMethods {
  init: {
    request: {schemas: TableSchema[]};
    response: {revision: number};
  };
  defineTable: {
    request: {schema: TableSchema};
    response: undefined;
  };
  replaceTable: {
    request: {table: string; rows: Row[]};
    response: ApplyOutcome;
  };
  applyBatch: {
    request: {batch: ChangeBatch};
    response: ApplyOutcome;
  };
  query: {
    request: {plan: QueryPlan};
    response: QueryResult;
  };
  querySql: {
    request: {sql: string; params: JsonValue[]};
    response: QueryResult;
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

export type WorkerEvent =
  | {
      v: typeof PROTOCOL_VERSION;
      event: 'tablesChanged';
      payload: ApplyOutcome;
    }
  | {
      v: typeof PROTOCOL_VERSION;
      event: 'syncStateChanged';
      payload: SyncState;
    };

export function isWorkerResponse(value: unknown): value is WorkerResponse {
  if (!isRecord(value) || value.v !== PROTOCOL_VERSION) {
    return false;
  }
  if (!Number.isSafeInteger(value.id) || typeof value.ok !== 'boolean') {
    return false;
  }
  return value.ok
    ? 'result' in value
    : isSerializedError(value.error);
}

export function isWorkerEvent(value: unknown): value is WorkerEvent {
  if (!isRecord(value) || value.v !== PROTOCOL_VERSION) {
    return false;
  }
  if (value.event === 'tablesChanged') {
    return isApplyOutcome(value.payload);
  }
  if (value.event === 'syncStateChanged') {
    return isSyncState(value.payload);
  }
  return false;
}

export function isWorkerRequest(value: unknown): value is WorkerRequest {
  if (
    !isRecord(value) ||
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
        Array.isArray(value.params.schemas) &&
        value.params.schemas.every(isTableSchema)
      );
    case 'defineTable':
      return isRecord(value.params) && isTableSchema(value.params.schema);
    case 'replaceTable':
      return (
        isRecord(value.params) &&
        typeof value.params.table === 'string' &&
        Array.isArray(value.params.rows) &&
        value.params.rows.every(isRow)
      );
    case 'applyBatch':
      return isRecord(value.params) && isChangeBatch(value.params.batch);
    case 'query':
      return isRecord(value.params) && isQueryPlan(value.params.plan);
    case 'querySql':
      return (
        isRecord(value.params) &&
        typeof value.params.sql === 'string' &&
        Array.isArray(value.params.params) &&
        value.params.params.every((param) => isJsonValue(param))
      );
    case 'close':
      return value.params === undefined;
    default:
      return false;
  }
}

export function isSerializedError(
  value: unknown,
): value is SerializedError {
  return (
    isRecord(value) &&
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
    Number.isSafeInteger(value.revision) &&
    Array.isArray(value.tables) &&
    value.tables.every((table) => typeof table === 'string')
  );
}

function isTableSchema(value: unknown): value is TableSchema {
  return (
    isRecord(value) &&
    typeof value.name === 'string' &&
    value.name.length > 0 &&
    Array.isArray(value.primaryKey) &&
    value.primaryKey.length > 0 &&
    value.primaryKey.every(
      (column) => typeof column === 'string' && column.length > 0,
    )
  );
}

function isRow(value: unknown): value is Row {
  return (
    isRecord(value) &&
    Object.values(value).every((cell) => isJsonValue(cell))
  );
}

function isChangeBatch(value: unknown): value is ChangeBatch {
  if (!isRecord(value) || !Array.isArray(value.changes)) {
    return false;
  }
  if (
    (value.sourceId !== undefined && typeof value.sourceId !== 'string') ||
    (value.transactionId !== undefined &&
      typeof value.transactionId !== 'string') ||
    (value.committedAt !== undefined && typeof value.committedAt !== 'string') ||
    (value.cursor !== undefined && !isSourceCursor(value.cursor))
  ) {
    return false;
  }
  return value.changes.every((change) => {
    if (!isRecord(change) || typeof change.table !== 'string') {
      return false;
    }
    return change.type === 'upsert'
      ? isRow(change.row)
      : change.type === 'delete' && isRow(change.key);
  });
}

function isSourceCursor(value: unknown): value is SourceCursor {
  return (
    isRecord(value) &&
    typeof value.kind === 'string' &&
    typeof value.value === 'string'
  );
}

function isQueryPlan(value: unknown): value is QueryPlan {
  if (
    !isRecord(value) ||
    typeof value.table !== 'string' ||
    !Array.isArray(value.filters)
  ) {
    return false;
  }
  if (
    value.columns !== undefined &&
    (!Array.isArray(value.columns) ||
      !value.columns.every((column) => typeof column === 'string'))
  ) {
    return false;
  }
  if (
    value.limit !== undefined &&
    (!Number.isSafeInteger(value.limit) || Number(value.limit) < 0)
  ) {
    return false;
  }
  return value.filters.every(
    (filter) =>
      isRecord(filter) &&
      typeof filter.column === 'string' &&
      filter.operator === 'eq' &&
      isJsonValue(filter.value),
  );
}

function isSyncState(value: unknown): value is SyncState {
  if (!isRecord(value) || !SYNC_PHASES.has(value.phase)) {
    return false;
  }
  return (
    (value.sourceId === undefined || typeof value.sourceId === 'string') &&
    (value.lastReconciledAt === undefined ||
      typeof value.lastReconciledAt === 'string') &&
    (value.error === undefined || isSerializedError(value.error))
  );
}

const SYNC_PHASES = new Set<unknown>([
  'idle',
  'connecting',
  'snapshotting',
  'live-best-effort',
  'live-durable',
  'stale',
  'resyncing',
  'locked',
  'error',
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
    ? value.every((item) => isJsonValue(item, seen, depth + 1))
    : isRecord(value) &&
      Object.values(value).every((item) => isJsonValue(item, seen, depth + 1));
  seen.delete(value);
  return valid;
}
