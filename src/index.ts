export {create, Client} from './client/client.js';
export type {
  SubscriptionOptions,
  TablesChangedEvent,
  ClientOptions,
  DataDir,
  PreparedStatement,
  Transaction,
  WorkerLike,
} from './client/client.js';
export {ClientError} from './client/error.js';
export type {
  ChangedKeys,
  ColumnSchema,
  ColumnType,
  ForeignKeyAction,
  ForeignKeySchema,
  IndexSchema,
  JsonPrimitive,
  JsonValue,
  QueryOptions,
  ResultField,
  Results,
  Row,
  RowMode,
  Schema,
  SerializedError,
  SetSchemaOptions,
  TableSchema,
} from './protocol.js';
