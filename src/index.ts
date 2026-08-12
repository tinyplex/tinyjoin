export {createClient, Client} from './client/client.js';
export type {
  SubscriptionOptions,
  TablesChangedEvent,
  ClientOptions,
  Transaction,
  WhenSyncedOptions,
} from './client/client.js';
export {ClientError} from './client/error.js';
export {QueryBuilder} from './client/query-builder.js';
export type {OrderOptions, QueryResponse} from './client/query-builder.js';
export type {
  ApplyOutcome,
  Change,
  ChangeBatch,
  Filter,
  JsonPrimitive,
  JsonValue,
  QueryPlan,
  QueryResult,
  OrderBy,
  Row,
  SerializedError,
  SqlResult,
  SourceCursor,
  StorageOptions,
  SyncPhase,
  SyncState,
  TableSchema,
} from './protocol.js';
export type {
  SourceOptions,
  SupabaseSourceOptions,
  SupabaseTableOptions,
} from './source-options.js';
