/// node

import type {Client} from '../index.js';

/// node.ClientError
export {ClientError} from '../index.js';

/// node.sharedTypes
export type {
  Client,
  JsonPrimitive,
  JsonValue,
  PreparedStatement,
  QueryOptions,
  ResultField,
  Results,
  Row,
  RowMode,
  SerializedError,
  SubscriptionOptions,
  TablesChangedEvent,
  Transaction,
} from '../index.js';

/// node.create
export function create(dataDir?: 'memory://'): Promise<Client>;
