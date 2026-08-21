import {
  isRecord,
  type ApplyOutcome,
  type ChangeBatch,
  type JsonValue,
  type QueryOptions,
  type QueryPlan,
  type QueryResult,
  type Results,
  type Row,
  type SqlResult,
  type StorageOptions,
  type TableSchema,
} from '../protocol.js';
import {ClientError} from './error.js';
import {QueryBuilder, type QueryExecutor} from './query-builder.js';
import {WorkerRpc, type WorkerLike} from './rpc.js';

export interface ClientOptions {
  worker?: WorkerLike;
  workerFactory?: () => WorkerLike;
  workerUrl?: string | URL;
  schemas?: TableSchema[];
  dataDir?: DataDir;
}

export type DataDir = string;

export interface TablesChangedEvent extends ApplyOutcome {}

export type SubscriptionOptions = {
  tables?: string[];
};

export interface Transaction {
  from<RowType extends object = Row>(table: string): QueryBuilder<RowType>;
  query<RowType = Row>(
    sql: string,
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;
  sql<RowType = Row>(
    strings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<Results<RowType>>;
  exec(sql: string, options?: QueryOptions): Promise<Results[]>;
  rollback(): Promise<void>;
  readonly closed: boolean;
}

export class Client implements QueryExecutor {
  readonly #rpc: WorkerRpc;
  readonly waitReady: Promise<void>;
  readonly #subscriptions = new Set<{
    tables?: Set<string>;
    listener(event: TablesChangedEvent): void;
  }>();
  #revision = 0;
  #ready = false;
  #closing = false;
  #closed = false;
  #closePromise: Promise<void> | undefined;
  #transactionTail: Promise<void> = Promise.resolve();
  #transactionActive = false;

  constructor(options: ClientOptions = {}) {
    assertClientOptions(options);
    const storage = storageFromDataDir(options.dataDir);
    const worker = createWorker(options);
    this.#rpc = new WorkerRpc(worker);
    this.#rpc.onEvent((event) => {
      if (event.event === 'tablesChanged') {
        this.#revision = Math.max(this.#revision, event.payload.revision);
        for (const subscription of this.#subscriptions) {
          if (
            !subscription.tables ||
            event.payload.tables.some((table) =>
              subscription.tables?.has(table),
            )
          ) {
            subscription.listener(event.payload);
          }
        }
      }
    });
    this.waitReady = this.#rpc
      .request('init', {
        schemas: options.schemas ?? [],
        storage,
      })
      .then((result) => {
        if (!isInitResult(result)) {
          const error = clientError(
            'PROTOCOL_MISMATCH',
            'The TinyGres worker returned an invalid initialization result',
          );
          this.#rpc.dispose(error);
          throw error;
        }
        this.#revision = result.revision;
        this.#ready = true;
      });
  }

  get ready(): boolean {
    return this.#ready && !this.#closing && !this.#closed;
  }

  get closed(): boolean {
    return this.#closed;
  }

  from<RowType extends object = Row>(table: string): QueryBuilder<RowType> {
    if (!table.trim()) {
      throw new TypeError('A table name cannot be empty');
    }
    return new QueryBuilder<RowType>(this, {
      table,
      filters: [],
    });
  }

  async query<RowType = Row>(
    sql: string,
    params: JsonValue[] = [],
    options?: QueryOptions,
  ): Promise<Results<RowType>> {
    await this.waitReady;
    this.#assertNoActiveTransaction();
    assertQueryOptions(options);
    const result = await this.#rpc.request('executeSql', {sql, params});
    this.#revision = Math.max(this.#revision, result.revision);
    return toResults<RowType>(result, options);
  }

  sql<RowType = Row>(
    strings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<Results<RowType>> {
    return this.query<RowType>(parameterize(strings, params), params);
  }

  async executePlan(plan: QueryPlan): Promise<QueryResult> {
    await this.waitReady;
    this.#assertNoActiveTransaction();
    const result = await this.#rpc.request('query', {plan});
    this.#revision = Math.max(this.#revision, result.revision);
    return result;
  }

  /** Executes one or more SQL statements without parameters. */
  async exec(
    sql: string,
    options?: QueryOptions,
  ): Promise<Results[]> {
    await this.waitReady;
    this.#assertNoActiveTransaction();
    assertQueryOptions(options);
    const results = await this.#rpc.request('execSql', {sql});
    this.#noteResults(results);
    return results.map((result) => toResults(result, options));
  }

  /**
   * Runs SQL against an isolated staged database and durably publishes all
   * changes together when the callback succeeds.
   */
  transaction<Result>(
    callback: (transaction: Transaction) => Result | Promise<Result>,
  ): Promise<Result> {
    if (typeof callback !== 'function') {
      throw new TypeError('TinyGres transaction requires a callback');
    }
    const run = this.#transactionTail.then(() =>
      this.#runTransaction(callback),
    );
    this.#transactionTail = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  /** Atomically replaces the complete contents and schema of a table. */
  async replaceTable(schema: TableSchema, rows: Row[]): Promise<ApplyOutcome> {
    await this.waitReady;
    this.#assertNoActiveTransaction();
    const outcome = await this.#rpc.request('replaceTable', {schema, rows});
    this.#revision = Math.max(this.#revision, outcome.revision);
    return outcome;
  }

  /** Atomically applies a batch of row upserts and deletes. */
  async applyBatch(batch: ChangeBatch): Promise<ApplyOutcome> {
    await this.waitReady;
    this.#assertNoActiveTransaction();
    const outcome = await this.#rpc.request('applyBatch', {batch});
    this.#revision = Math.max(this.#revision, outcome.revision);
    return outcome;
  }

  subscribe(
    options: SubscriptionOptions,
    listener: (event: TablesChangedEvent) => void,
  ): () => void {
    const subscription = {
      ...(options.tables ? {tables: new Set(options.tables)} : {}),
      listener,
    };
    this.#subscriptions.add(subscription);
    return () => this.#subscriptions.delete(subscription);
  }

  getRevision(): number {
    return this.#revision;
  }

  close(): Promise<void> {
    if (this.#closePromise === undefined) {
      this.#closing = true;
      this.#closePromise = this.#closeOnce();
    }
    return this.#closePromise;
  }

  async #closeOnce(): Promise<void> {
    try {
      await this.waitReady;
      await this.#rpc.request('close', undefined);
    } finally {
      this.#ready = false;
      this.#closing = false;
      this.#closed = true;
      this.#subscriptions.clear();
      this.#rpc.dispose();
    }
  }

  async #runTransaction<Result>(
    callback: (transaction: Transaction) => Result | Promise<Result>,
  ): Promise<Result> {
    await this.waitReady;
    if (this.#closing || this.#closed) {
      throw clientError('CLIENT_CLOSED', 'The TinyGres client is closed');
    }
    const {transactionId} = await this.#rpc.request(
      'beginTransaction',
      undefined,
    );
    this.#transactionActive = true;
    const transaction = new ClientTransaction(
      this.#rpc,
      transactionId,
      (revision) => {
        this.#revision = Math.max(this.#revision, revision);
      },
    );
    let shouldRollback = true;
    try {
      const result = await callback(transaction);
      transaction.seal();
      await transaction.settle();
      if (transaction.rollbackCompleted) {
        shouldRollback = false;
        return result;
      }
      const outcome = await this.#rpc.request('commitTransaction', {
        transactionId,
      });
      shouldRollback = false;
      this.#revision = Math.max(this.#revision, outcome.revision);
      return result;
    } catch (error) {
      transaction.seal();
      if (shouldRollback && !transaction.rollbackCompleted) {
        await this.#rpc
          .request('rollbackTransaction', {transactionId})
          .catch(() => undefined);
      }
      throw error;
    } finally {
      this.#transactionActive = false;
    }
  }

  #assertNoActiveTransaction(): void {
    if (this.#closing || this.#closed) {
      throw clientError('CLIENT_CLOSED', 'The TinyGres client is closed');
    }
    if (this.#transactionActive) {
      throw clientError(
        'TRANSACTION_ACTIVE',
        'Use the transaction object while a TinyGres transaction is active',
      );
    }
  }

  #noteResults(results: SqlResult[]): void {
    for (const result of results) {
      this.#revision = Math.max(this.#revision, result.revision);
    }
  }
}

class ClientTransaction implements Transaction, QueryExecutor {
  readonly #pending = new Set<Promise<unknown>>();
  #open = true;
  #closing = false;
  #rollbackCompleted = false;
  #rollbackPromise: Promise<void> | undefined;

  constructor(
    readonly rpc: WorkerRpc,
    readonly transactionId: string,
    readonly noteRevision: (revision: number) => void,
  ) {}

  from<RowType extends object = Row>(table: string): QueryBuilder<RowType> {
    this.#assertOpen();
    if (!table.trim()) {
      throw new TypeError('A table name cannot be empty');
    }
    return new QueryBuilder<RowType>(this, {table, filters: []});
  }

  query<RowType = Row>(
    sql: string,
    params: JsonValue[] = [],
    options?: QueryOptions,
  ): Promise<Results<RowType>> {
    this.#assertOpen();
    assertQueryOptions(options);
    return this.#track(
      this.rpc
        .request('executeSql', {
          sql,
          params,
          transactionId: this.transactionId,
        })
        .then((result) => {
          this.noteRevision(result.revision);
          return toResults<RowType>(result, options);
        }),
    );
  }

  sql<RowType = Row>(
    strings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<Results<RowType>> {
    return this.query<RowType>(parameterize(strings, params), params);
  }

  exec(
    sql: string,
    options?: QueryOptions,
  ): Promise<Results[]> {
    this.#assertOpen();
    assertQueryOptions(options);
    return this.#track(
      this.rpc
        .request('execSql', {
          sql,
          transactionId: this.transactionId,
        })
        .then((results) => {
          for (const result of results) {
            this.noteRevision(result.revision);
          }
          return results.map((result) => toResults(result, options));
        }),
    );
  }

  executePlan(plan: QueryPlan): Promise<QueryResult> {
    this.#assertOpen();
    return this.#track(
      this.rpc
        .request('query', {plan, transactionId: this.transactionId})
        .then((result) => {
          this.noteRevision(result.revision);
          return result;
        }),
    );
  }

  seal(): void {
    this.#open = false;
  }

  get closed(): boolean {
    return !this.#open;
  }

  get rollbackCompleted(): boolean {
    return this.#rollbackCompleted;
  }

  rollback(): Promise<void> {
    this.#assertOpen();
    this.#closing = true;
    const rollback = this.rpc
      .request('rollbackTransaction', {
        transactionId: this.transactionId,
      })
      .then(() => {
        this.#rollbackCompleted = true;
        this.#open = false;
        this.#closing = false;
      });
    this.#rollbackPromise = rollback;
    this.#pending.add(rollback);
    void rollback.then(
      () => this.#pending.delete(rollback),
      () => this.#pending.delete(rollback),
    );
    return rollback;
  }

  async settle(): Promise<void> {
    while (this.#pending.size > 0) {
      await Promise.all([...this.#pending]);
    }
    await this.#rollbackPromise;
  }

  #track<Result>(promise: Promise<Result>): Promise<Result> {
    this.#assertOpen();
    this.#pending.add(promise);
    void promise.then(
      () => this.#pending.delete(promise),
      () => this.#pending.delete(promise),
    );
    return promise;
  }

  #assertOpen(): void {
    if (!this.#open || this.#closing) {
      throw clientError(
        'TRANSACTION_CLOSED',
        'The TinyGres transaction callback has already completed',
      );
    }
  }
}

export function create(): Promise<Client>;
export function create(options: ClientOptions): Promise<Client>;
export function create(
  dataDir: DataDir | undefined,
  options?: ClientOptions,
): Promise<Client>;
export async function create(
  dataDirOrOptions: DataDir | ClientOptions | undefined = {},
  options?: ClientOptions,
): Promise<Client> {
  let resolvedOptions: ClientOptions;
  if (typeof dataDirOrOptions === 'string') {
    if (options?.dataDir !== undefined) {
      throw new TypeError(
        'Provide the TinyGres data directory either positionally or in options.dataDir, not both',
      );
    }
    resolvedOptions = {...options, dataDir: dataDirOrOptions};
  } else if (dataDirOrOptions === undefined) {
    resolvedOptions = options ?? {};
  } else {
    if (options !== undefined) {
      throw new TypeError(
        'TinyGres options must be the first argument when no positional data directory is used',
      );
    }
    resolvedOptions = dataDirOrOptions;
  }

  const client = new Client(resolvedOptions);
  try {
    await client.waitReady;
    return client;
  } catch (error) {
    await client.close().catch(() => undefined);
    throw error;
  }
}

function createWorker(options: ClientOptions): WorkerLike {
  const selected = [
    options.worker,
    options.workerFactory,
    options.workerUrl,
  ].filter((value) => value !== undefined);
  if (selected.length > 1) {
    throw new TypeError(
      'Provide only one of worker, workerFactory, or workerUrl to TinyGres',
    );
  }

  return (
    options.worker ??
    options.workerFactory?.() ??
    (options.workerUrl
      ? createUrlWorker(options.workerUrl)
      : createDefaultWorker())
  );
}

function createUrlWorker(url: string | URL): WorkerLike {
  assertWorkerAvailable();
  return new Worker(url, {name: 'tinygres', type: 'module'});
}

function createDefaultWorker(): WorkerLike {
  assertWorkerAvailable();
  return new Worker(new URL('../worker/default-entry.js', import.meta.url), {
    name: 'tinygres',
    type: 'module',
  });
}

function assertWorkerAvailable(): void {
  if (typeof Worker === 'undefined') {
    throw new Error(
      'TinyGres requires a browser Worker. Importing is SSR-safe, but create the client in the browser or provide a Worker-like implementation.',
    );
  }
}

function clientError(code: string, message: string): ClientError {
  return new ClientError({code, message});
}

function assertClientOptions(options: ClientOptions): void {
  const supported = new Set([
    'dataDir',
    'schemas',
    'worker',
    'workerFactory',
    'workerUrl',
  ]);
  const prototype = isRecord(options)
    ? Object.getPrototypeOf(options)
    : undefined;
  if (
    !isRecord(options) ||
    (prototype !== Object.prototype && prototype !== null) ||
    Reflect.ownKeys(options).some(
      (key) => typeof key !== 'string' || !supported.has(key),
    )
  ) {
    throw new TypeError(
      'TinyGres client options support only dataDir, schemas, worker, workerFactory, and workerUrl',
    );
  }
}

function storageFromDataDir(dataDir: DataDir | undefined): StorageOptions {
  if (dataDir === undefined || dataDir === 'memory://') {
    return {kind: 'memory'};
  }
  if (typeof dataDir !== 'string' || !dataDir.startsWith('opfs://')) {
    throw new TypeError(
      'TinyGres dataDir must be memory:// or opfs:// followed by a database name',
    );
  }
  const name = dataDir.slice('opfs://'.length);
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(name)) {
    throw new TypeError(
      'A TinyGres OPFS database name must be 1-64 ASCII letters, numbers, dots, underscores, or hyphens, and start with a letter or number',
    );
  }
  return {kind: 'opfs', name};
}

function parameterize(
  strings: TemplateStringsArray,
  params: readonly JsonValue[],
): string {
  if (!Array.isArray(strings) || strings.length !== params.length + 1) {
    throw new TypeError('TinyGres sql must be used as a tagged template');
  }
  let sql = strings[0] ?? '';
  for (let index = 0; index < params.length; index++) {
    sql += `$${index + 1}${strings[index + 1] ?? ''}`;
  }
  return sql;
}

function toResults<RowType>(
  result: SqlResult,
  options?: QueryOptions,
): Results<RowType> {
  assertQueryOptions(options);
  const rows =
    options?.rowMode === 'array'
      ? rowsAsArrays(result.rows, result.fields)
      : result.rows;
  return {
    rows: rows as RowType[],
    fields: result.fields,
    affectedRows: affectedRows(result),
    command: result.command,
    ...(hasRowCount(result.command) ? {rowCount: result.rowCount} : {}),
    revision: result.revision,
    tables: result.tables,
  };
}

function rowsAsArrays(
  rows: Row[],
  fields: SqlResult['fields'],
): JsonValue[][] {
  if (rows.length > 0 && fields.length === 0) {
    throw clientError(
      'ROW_METADATA_UNAVAILABLE',
      'TinyGres cannot return array rows without field metadata',
    );
  }
  return rows.map((row) => fields.map((field) => row[field.name] ?? null));
}

function affectedRows(result: SqlResult): number {
  return /^(?:DELETE|INSERT|MERGE|UPDATE)$/.test(result.command)
    ? result.rowCount
    : 0;
}

function hasRowCount(command: string): boolean {
  return /^(?:DELETE|INSERT|MERGE|SELECT|UPDATE)$/.test(command);
}

function assertQueryOptions(options: QueryOptions | undefined): void {
  if (options === undefined) {
    return;
  }
  if (
    !isRecord(options) ||
    Reflect.ownKeys(options).some((key) => key !== 'rowMode') ||
    (options.rowMode !== undefined &&
      options.rowMode !== 'array' &&
      options.rowMode !== 'object')
  ) {
    throw new TypeError(
      'TinyGres query options currently support only rowMode: object or array',
    );
  }
}

function isInitResult(value: unknown): value is {revision: number} {
  return (
    isRecord(value) &&
    Object.hasOwn(value, 'revision') &&
    Reflect.ownKeys(value).every((key) => key === 'revision') &&
    Number.isSafeInteger(value.revision) &&
    Number(value.revision) >= 0
  );
}
