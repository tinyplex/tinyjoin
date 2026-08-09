import type {
  ApplyOutcome,
  ChangeBatch,
  JsonValue,
  QueryPlan,
  QueryResult,
  Row,
  SyncState,
  TableSchema,
} from '../protocol.js';
import {QueryBuilder, type QueryExecutor} from './query-builder.js';
import {WorkerRpc, type WorkerLike} from './rpc.js';

export interface TinygresClientOptions {
  worker?: WorkerLike;
  workerFactory?: () => WorkerLike;
  workerUrl?: string | URL;
  schemas?: TableSchema[];
}

export interface TablesChangedEvent extends ApplyOutcome {}

export type SubscriptionOptions = {
  tables?: string[];
};

export class TinygresClient implements QueryExecutor {
  readonly #rpc: WorkerRpc;
  readonly #ready: Promise<void>;
  readonly #subscriptions = new Set<{
    tables?: Set<string>;
    listener(event: TablesChangedEvent): void;
  }>();
  readonly #syncListeners = new Set<(state: SyncState) => void>();
  #revision = 0;
  #closed = false;

  constructor(worker: WorkerLike, schemas: TableSchema[] = []) {
    this.#rpc = new WorkerRpc(worker);
    this.#rpc.onEvent((event) => {
      if (event.event === 'tablesChanged') {
        this.#revision = Math.max(this.#revision, event.payload.revision);
        for (const subscription of this.#subscriptions) {
          if (
            !subscription.tables ||
            event.payload.tables.some((table) => subscription.tables?.has(table))
          ) {
            subscription.listener(event.payload);
          }
        }
      } else {
        for (const listener of this.#syncListeners) {
          listener(event.payload);
        }
      }
    });
    this.#ready = this.#rpc.request('init', {schemas}).then(({revision}) => {
      this.#revision = revision;
    });
  }

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
    const result = await this.#rpc.request('querySql', {sql, params});
    this.#revision = Math.max(this.#revision, result.revision);
    return result as QueryResult<RowType>;
  }

  async executePlan(plan: QueryPlan): Promise<QueryResult> {
    await this.#ready;
    const result = await this.#rpc.request('query', {plan});
    this.#revision = Math.max(this.#revision, result.revision);
    return result;
  }

  /**
   * Source-integration API. This replaces a complete table snapshot and is not
   * an application write API.
   */
  async replaceTable(
    schema: TableSchema,
    rows: Row[],
  ): Promise<ApplyOutcome> {
    await this.#ready;
    await this.#rpc.request('defineTable', {schema});
    return this.#rpc.request('replaceTable', {table: schema.name, rows});
  }

  /**
   * Source-integration API used by adapters and tests to apply normalized
   * server changes atomically.
   */
  async applyBatch(batch: ChangeBatch): Promise<ApplyOutcome> {
    await this.#ready;
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

  subscribeToSyncState(listener: (state: SyncState) => void): () => void {
    this.#syncListeners.add(listener);
    return () => this.#syncListeners.delete(listener);
  }

  getRevision(): number {
    return this.#revision;
  }

  async close(): Promise<void> {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    try {
      await this.#ready;
      await this.#rpc.request('close', undefined);
    } finally {
      this.#subscriptions.clear();
      this.#syncListeners.clear();
      this.#rpc.dispose();
    }
  }
}

export function createTinygresClient(
  options: TinygresClientOptions = {},
): TinygresClient {
  const selected = [options.worker, options.workerFactory, options.workerUrl].filter(
    (value) => value !== undefined,
  );
  if (selected.length > 1) {
    throw new TypeError(
      'Provide only one of worker, workerFactory, or workerUrl to Tinygres',
    );
  }

  const worker =
    options.worker ??
    options.workerFactory?.() ??
    (options.workerUrl ? createUrlWorker(options.workerUrl) : createDefaultWorker());
  return new TinygresClient(worker, options.schemas);
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
      'Tinygres requires a browser Worker. Importing is SSR-safe, but create the client in the browser or provide a Worker-like implementation.',
    );
  }
}
