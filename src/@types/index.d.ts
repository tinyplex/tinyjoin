/// tinyjoin

/// JsonPrimitive
export type JsonPrimitive = null | boolean | number | string;

/// JsonValue
export type JsonValue =
  | JsonPrimitive
  | JsonValue[]
  | {[key: string]: JsonValue};

/// Row
export type Row = Record<string, JsonValue>;

/// DataDir
export type DataDir = string;

/// RowMode
export type RowMode = 'array' | 'object';

/// QueryOptions
export interface QueryOptions {
  /// QueryOptions.rowMode
  rowMode?: RowMode;
}

/// ResultField
export interface ResultField {
  /// ResultField.name
  name: string;

  /// ResultField.dataTypeID
  dataTypeID: number;
}

/// Results
export interface Results<RowType = Row> {
  /// Results.rows
  rows: RowType[];

  /// Results.fields
  fields: ResultField[];

  /// Results.affectedRows
  affectedRows?: number;

  /// Results.command
  command?: string;

  /// Results.rowCount
  rowCount?: number;

  /// Results.revision
  revision: number;

  /// Results.tables
  tables: string[];
}

/// SerializedError
export interface SerializedError {
  /// SerializedError.code
  code: string;

  /// SerializedError.message
  message: string;

  /// SerializedError.details
  details?: JsonValue;

  /// SerializedError.retryable
  retryable?: boolean;
}

/// WorkerLike
export interface WorkerLike {
  /// WorkerLike.postMessage
  postMessage(message: unknown): void;

  /// WorkerLike.addEventListener.message
  addEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;

  /// WorkerLike.addEventListener.messageerror
  addEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;

  /// WorkerLike.addEventListener.error
  addEventListener(
    type: 'error',
    listener: (event: ErrorEvent) => void,
  ): void;

  /// WorkerLike.removeEventListener.message
  removeEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;

  /// WorkerLike.removeEventListener.messageerror
  removeEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;

  /// WorkerLike.removeEventListener.error
  removeEventListener(
    type: 'error',
    listener: (event: ErrorEvent) => void,
  ): void;

  /// WorkerLike.terminate
  terminate?: () => void;
}

/// ClientOptions
export interface ClientOptions {
  /// ClientOptions.worker
  worker?: WorkerLike;

  /// ClientOptions.workerFactory
  workerFactory?: () => WorkerLike;

  /// ClientOptions.workerUrl
  workerUrl?: string | URL;

  /// ClientOptions.dataDir
  dataDir?: DataDir;
}

/// TablesChangedEvent
export interface TablesChangedEvent {
  /// TablesChangedEvent.reset
  reset?: boolean;

  /// TablesChangedEvent.revision
  revision: number;

  /// TablesChangedEvent.tables
  tables: string[];
}

/// SubscriptionOptions
export interface SubscriptionOptions {
  /// SubscriptionOptions.tables
  tables?: string[];
}

/// PreparedStatement
export interface PreparedStatement<RowType = Row> {
  /// PreparedStatement.execute
  execute(
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;

  /// PreparedStatement.close
  close(): Promise<void>;

  /// PreparedStatement.closed
  readonly closed: boolean;
}

/// Transaction
export interface Transaction {
  /// Transaction.query
  query<RowType = Row>(
    sql: string,
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;

  /// Transaction.sql
  sql<RowType = Row>(
    strings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<Results<RowType>>;

  /// Transaction.exec
  exec(sql: string, options?: QueryOptions): Promise<Results[]>;

  /// Transaction.execute
  execute<RowType = Row>(
    statement: PreparedStatement<RowType>,
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;

  /// Transaction.rollback
  rollback(): Promise<void>;

  /// Transaction.closed
  readonly closed: boolean;
}

/// Client
export class Client {
  /// Client.constructor
  constructor(options?: ClientOptions);

  /// Client.waitReady
  readonly waitReady: Promise<void>;

  /// Client.ready
  readonly ready: boolean;

  /// Client.closed
  readonly closed: boolean;

  /// Client.query
  query<RowType = Row>(
    sql: string,
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;

  /// Client.sql
  sql<RowType = Row>(
    strings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<Results<RowType>>;

  /// Client.prepare
  prepare<RowType = Row>(sql: string): Promise<PreparedStatement<RowType>>;

  /// Client.exec
  exec(sql: string, options?: QueryOptions): Promise<Results[]>;

  /// Client.transaction
  transaction<Result>(
    callback: (transaction: Transaction) => Result | Promise<Result>,
  ): Promise<Result>;

  /// Client.subscribe
  subscribe(
    options: SubscriptionOptions,
    listener: (event: TablesChangedEvent) => void,
  ): () => void;

  /// Client.getRevision
  getRevision(): number;

  /// Client.close
  close(): Promise<void>;
}

/// create
export function create(): Promise<Client>;
export function create(options: ClientOptions): Promise<Client>;
export function create(
  dataDir: DataDir | undefined,
  options?: ClientOptions,
): Promise<Client>;

/// ClientError
export class ClientError extends Error {
  /// ClientError.constructor
  constructor(error: SerializedError);

  /// ClientError.code
  readonly code: string;

  /// ClientError.details
  readonly details: SerializedError['details'];

  /// ClientError.retryable
  readonly retryable: boolean;
}
