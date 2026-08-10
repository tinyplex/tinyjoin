export {createClient, Client} from './client/client.js';
export type {
  SubscriptionOptions,
  TablesChangedEvent,
  ClientOptions,
} from './client/client.js';
export {ClientError} from './client/error.js';
export {QueryBuilder} from './client/query-builder.js';
export type {QueryResponse} from './client/query-builder.js';
export type {
  ApplyOutcome,
  Change,
  ChangeBatch,
  Filter,
  JsonPrimitive,
  JsonValue,
  QueryPlan,
  QueryResult,
  Row,
  SerializedError,
  SourceCursor,
  SyncPhase,
  SyncState,
  TableSchema,
} from './protocol.js';
