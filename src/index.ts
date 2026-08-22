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
export type {
  JsonPrimitive,
  JsonValue,
  QueryOptions,
  ResultField,
  Results,
  Row,
  RowMode,
  SerializedError,
} from './protocol.js';
