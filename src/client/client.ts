import {
  isRecord,
  type ApplyOutcome,
  type ChangeBatch,
  type JsonValue,
  type QueryPlan,
  type QueryResult,
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
  storage?: StorageOptions;
}

export interface TablesChangedEvent extends ApplyOutcome {}

export type SubscriptionOptions = {
  tables?: string[];
};

export interface Transaction {
  from<RowType extends object = Row>(table: string): QueryBuilder<RowType>;
  query<RowType extends object = Row>(
    sql: string,
    params?: JsonValue[],
  ): Promise<QueryResult<RowType>>;
  exec<RowType extends object = Row>(
    sql: string,
    params?: JsonValue[],
  ): Promise<SqlResult<RowType>>;
}

export class Client implements QueryExecutor {
  readonly #rpc: WorkerRpc;
  readonly #ready: Promise<void>;
  readonly #subscriptions = new Set<{
    tables?: Set<string>;
    listener(event: TablesChangedEvent): void;
  }>();
  #revision = 0;
  #closed = false;
  #closePromise: Promise<void> | undefined;
  #transactionTail: Promise<void> = Promise.resolve();
  #transactionActive = false;

  constructor(options: ClientOptions = {}) {
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
    this.#ready = this.#rpc
      .request('init', {
        schemas: options.schemas ?? [],
        storage: options.storage ?? {kind: 'memory'},
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
      });
  }

  /** Resolves after local memory or OPFS state is ready. */
  ready(): Promise<void> {
    return this.#ready;
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

  async query<RowType extends object = Row>(
    sql: string,
    params: JsonValue[] = [],
  ): Promise<QueryResult<RowType>> {
    await this.#ready;
    this.#assertNoActiveTransaction();
    const result = await this.#rpc.request('querySql', {sql, params});
    this.#revision = Math.max(this.#revision, result.revision);
    return result as QueryResult<RowType>;
  }

  async executePlan(plan: QueryPlan): Promise<QueryResult> {
    await this.#ready;
    this.#assertNoActiveTransaction();
    const result = await this.#rpc.request('query', {plan});
    this.#revision = Math.max(this.#revision, result.revision);
    return result;
  }

  /** Executes one atomic SQL DDL or DML statement. */
  async exec<RowType extends object = Row>(
    sql: string,
    params: JsonValue[] = [],
  ): Promise<SqlResult<RowType>> {
    await this.#ready;
    this.#assertNoActiveTransaction();
    const result = await this.#rpc.request('executeSql', {sql, params});
    this.#revision = Math.max(this.#revision, result.revision);
    return result as SqlResult<RowType>;
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
    await this.#ready;
    this.#assertNoActiveTransaction();
    return this.#rpc.request('replaceTable', {schema, rows});
  }

  /** Atomically applies a batch of row upserts and deletes. */
  async applyBatch(batch: ChangeBatch): Promise<ApplyOutcome> {
    await this.#ready;
    this.#assertNoActiveTransaction();
    return this.#rpc.request('applyBatch', {batch});
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
    if (!this.#closed) {
      this.#closed = true;
    }
    this.#closePromise ??= this.#closeOnce();
    return this.#closePromise;
  }

  async #closeOnce(): Promise<void> {
    try {
      await this.#ready;
      await this.#rpc.request('close', undefined);
    } finally {
      this.#subscriptions.clear();
      this.#rpc.dispose();
    }
  }

  async #runTransaction<Result>(
    callback: (transaction: Transaction) => Result | Promise<Result>,
  ): Promise<Result> {
    await this.#ready;
    if (this.#closed) {
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
      const outcome = await this.#rpc.request('commitTransaction', {
        transactionId,
      });
      shouldRollback = false;
      this.#revision = Math.max(this.#revision, outcome.revision);
      return result;
    } catch (error) {
      transaction.seal();
      if (shouldRollback) {
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
    if (this.#transactionActive) {
      throw clientError(
        'TRANSACTION_ACTIVE',
        'Use the transaction object while a TinyGres transaction is active',
      );
    }
  }
}

class ClientTransaction implements Transaction, QueryExecutor {
  readonly #pending = new Set<Promise<unknown>>();
  #open = true;

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

  query<RowType extends object = Row>(
    sql: string,
    params: JsonValue[] = [],
  ): Promise<QueryResult<RowType>> {
    return this.#track(
      this.rpc
        .request('querySql', {sql, params, transactionId: this.transactionId})
        .then((result) => {
          this.noteRevision(result.revision);
          return result as QueryResult<RowType>;
        }),
    );
  }

  exec<RowType extends object = Row>(
    sql: string,
    params: JsonValue[] = [],
  ): Promise<SqlResult<RowType>> {
    return this.#track(
      this.rpc
        .request('executeSql', {
          sql,
          params,
          transactionId: this.transactionId,
        })
        .then((result) => {
          this.noteRevision(result.revision);
          return result as SqlResult<RowType>;
        }),
    );
  }

  executePlan(plan: QueryPlan): Promise<QueryResult> {
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

  async settle(): Promise<void> {
    while (this.#pending.size > 0) {
      await Promise.all([...this.#pending]);
    }
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
    if (!this.#open) {
      throw clientError(
        'TRANSACTION_CLOSED',
        'The TinyGres transaction callback has already completed',
      );
    }
  }
}

export function createClient(options: ClientOptions = {}): Client {
  return new Client(options);
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

function isInitResult(value: unknown): value is {revision: number} {
  return (
    isRecord(value) &&
    Object.hasOwn(value, 'revision') &&
    Reflect.ownKeys(value).every((key) => key === 'revision') &&
    Number.isSafeInteger(value.revision) &&
    Number(value.revision) >= 0
  );
}
