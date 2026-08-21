export {create, Client} from './client/client.js';
export type {
  SubscriptionOptions,
  TablesChangedEvent,
  ClientOptions,
  DataDir,
  PreparedStatement,
  Transaction,
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
  QueryOptions,
  ResultField,
  Results,
  OrderBy,
  Row,
  RowMode,
  SerializedError,
  TableSchema,
} from './protocol.js';
