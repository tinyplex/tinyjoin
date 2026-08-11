import {
  isRecord,
  type ApplyOutcome,
  type ChangeBatch,
  type JsonValue,
  type QueryPlan,
  type QueryResult,
  type Row,
  type StorageOptions,
  type SyncState,
  type TableSchema,
} from '../protocol.js';
import type {SourceOptions} from '../source-options.js';
import {ClientError} from './error.js';
import {QueryBuilder, type QueryExecutor} from './query-builder.js';
import {WorkerRpc, type WorkerLike} from './rpc.js';

export interface ClientOptions {
  worker?: WorkerLike;
  workerFactory?: () => WorkerLike;
  workerUrl?: string | URL;
  schemas?: TableSchema[];
  storage?: StorageOptions;
  /** Serializable source configuration run inside the Worker. */
  source?: SourceOptions;
}

export interface TablesChangedEvent extends ApplyOutcome {}

export type SubscriptionOptions = {
  tables?: string[];
};

export interface WhenSyncedOptions {
  timeoutMs?: number;
  signal?: AbortSignal;
}

type SyncWaiter = {
  resolve(state: SyncState): void;
  reject(error: ClientError): void;
  ready: boolean;
  timeout?: ReturnType<typeof setTimeout>;
  signal?: AbortSignal;
  abortListener?: () => void;
};

export class Client implements QueryExecutor {
  readonly #rpc: WorkerRpc;
  readonly #ready: Promise<void>;
  readonly #subscriptions = new Set<{
    tables?: Set<string>;
    listener(event: TablesChangedEvent): void;
  }>();
  readonly #syncListeners = new Set<(state: SyncState) => void>();
  readonly #syncWaiters = new Set<SyncWaiter>();
  #revision = 0;
  #syncState: SyncState = {phase: 'idle'};
  #sourceConfigured: boolean;
  #closed = false;
  #closePromise: Promise<void> | undefined;

  constructor(options: ClientOptions = {}) {
    this.#sourceConfigured = options.source !== undefined;
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
      } else {
        this.#syncState = copySyncState(event.payload);
        this.#settleSyncWaiters();
        for (const listener of this.#syncListeners) {
          listener(copySyncState(this.#syncState));
        }
      }
    });
    this.#ready = this.#rpc
      .request('init', {
        schemas: options.schemas ?? [],
        storage: options.storage ?? {kind: 'memory'},
        ...(options.source ? {source: options.source} : {}),
      })
      .then((result) => {
        if (!isInitResult(result)) {
          const error = syncError(
            'PROTOCOL_MISMATCH',
            'The TinyGres worker returned an invalid initialization result',
          );
          this.#rpc.dispose(error);
          throw error;
        }
        const {revision, sourceConfigured} = result;
        this.#revision = revision;
        this.#sourceConfigured = sourceConfigured;
      });
  }

  /** Resolves after local memory or OPFS state is ready, without waiting for a source. */
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
  async replaceTable(schema: TableSchema, rows: Row[]): Promise<ApplyOutcome> {
    await this.#ready;
    return this.#rpc.request('replaceTable', {schema, rows});
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

  /** Subscribes to sync state and immediately emits its current snapshot. */
  subscribeToSyncState(listener: (state: SyncState) => void): () => void {
    this.#syncListeners.add(listener);
    listener(copySyncState(this.#syncState));
    return () => this.#syncListeners.delete(listener);
  }

  /** Returns a detached snapshot of the latest source synchronization state. */
  getSyncState(): SyncState {
    return copySyncState(this.#syncState);
  }

  /** Waits until the configured source has established a complete live baseline. */
  async whenSynced(options: WhenSyncedOptions = {}): Promise<SyncState> {
    const timeoutMs = validateSyncTimeout(options.timeoutMs);
    if (options.signal?.aborted) {
      throw syncError(
        'SYNC_ABORTED',
        'Waiting for TinyGres synchronization was aborted',
      );
    }
    if (this.#closed) {
      throw syncError('CLIENT_CLOSED', 'The TinyGres client is closed');
    }

    return new Promise<SyncState>((resolve, reject) => {
      const waiter: SyncWaiter = {resolve, reject, ready: false};
      this.#syncWaiters.add(waiter);
      if (timeoutMs !== undefined) {
        waiter.timeout = setTimeout(() => {
          this.#removeSyncWaiter(waiter);
          reject(
            new ClientError({
              code: 'SYNC_TIMEOUT',
              message: `TinyGres did not synchronize within ${timeoutMs}ms`,
              details: {timeoutMs},
              retryable: true,
            }),
          );
        }, timeoutMs);
      }
      if (options.signal) {
        waiter.signal = options.signal;
        waiter.abortListener = () => {
          this.#removeSyncWaiter(waiter);
          reject(
            syncError(
              'SYNC_ABORTED',
              'Waiting for TinyGres synchronization was aborted',
            ),
          );
        };
        options.signal.addEventListener('abort', waiter.abortListener, {
          once: true,
        });
        if (options.signal.aborted) {
          waiter.abortListener();
          return;
        }
      }
      void this.#ready.then(
        () => {
          if (!this.#syncWaiters.has(waiter)) {
            return;
          }
          waiter.ready = true;
          this.#settleSyncWaiters();
        },
        (error: unknown) => {
          if (!this.#syncWaiters.has(waiter)) {
            return;
          }
          this.#removeSyncWaiter(waiter);
          reject(
            ClientError.fromUnknown(error, 'CLIENT_INITIALIZATION_FAILED'),
          );
        },
      );
      this.#settleSyncWaiters();
    });
  }

  getRevision(): number {
    return this.#revision;
  }

  close(): Promise<void> {
    if (!this.#closed) {
      this.#closed = true;
      this.#rejectSyncWaiters(
        syncError('CLIENT_CLOSED', 'The TinyGres client is closed'),
      );
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
      this.#syncListeners.clear();
      this.#rpc.dispose();
    }
  }

  #settleSyncWaiters(): void {
    const outcome = syncOutcome(this.#syncState, this.#sourceConfigured);
    if (!outcome.state && !outcome.error) {
      return;
    }
    for (const waiter of [...this.#syncWaiters]) {
      if (!waiter.ready) {
        continue;
      }
      this.#removeSyncWaiter(waiter);
      if (outcome.state) {
        waiter.resolve(copySyncState(outcome.state));
      } else {
        waiter.reject(copyClientError(outcome.error!));
      }
    }
  }

  #rejectSyncWaiters(error: ClientError): void {
    for (const waiter of [...this.#syncWaiters]) {
      this.#removeSyncWaiter(waiter);
      waiter.reject(copyClientError(error));
    }
  }

  #removeSyncWaiter(waiter: SyncWaiter): void {
    if (!this.#syncWaiters.delete(waiter)) {
      return;
    }
    if (waiter.timeout !== undefined) {
      clearTimeout(waiter.timeout);
    }
    if (waiter.signal && waiter.abortListener) {
      waiter.signal.removeEventListener('abort', waiter.abortListener);
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

function syncOutcome(
  state: SyncState,
  sourceConfigured: boolean,
): {state?: SyncState; error?: ClientError} {
  if (state.phase === 'live-best-effort' || state.phase === 'live-durable') {
    return {state: copySyncState(state)};
  }
  if (state.phase === 'error') {
    return {
      error: new ClientError(
        state.error ?? {
          code: 'SYNC_FAILED',
          message: 'TinyGres synchronization failed',
        },
      ),
    };
  }
  if (state.phase === 'locked') {
    return {
      error: new ClientError(
        state.error ?? {
          code: 'SYNC_LOCKED',
          message: 'TinyGres synchronization is locked',
        },
      ),
    };
  }
  if (state.phase === 'idle' && !sourceConfigured) {
    return {
      error: syncError(
        'SYNC_SOURCE_NOT_CONFIGURED',
        'TinyGres has no replication source to synchronize',
      ),
    };
  }
  return {};
}

function copySyncState(state: SyncState): SyncState {
  return {
    phase: state.phase,
    ...(state.sourceId !== undefined ? {sourceId: state.sourceId} : {}),
    ...(state.lastReconciledAt !== undefined
      ? {lastReconciledAt: state.lastReconciledAt}
      : {}),
    ...(state.error
      ? {
          error: {
            ...state.error,
            ...(state.error.details !== undefined
              ? {details: structuredClone(state.error.details)}
              : {}),
          },
        }
      : {}),
  };
}

function copyClientError(error: ClientError): ClientError {
  return new ClientError({
    code: error.code,
    message: error.message,
    ...(error.details === undefined ? {} : {details: error.details}),
    retryable: error.retryable,
  });
}

function validateSyncTimeout(
  timeoutMs: number | undefined,
): number | undefined {
  if (timeoutMs === undefined) {
    return undefined;
  }
  if (
    !Number.isSafeInteger(timeoutMs) ||
    timeoutMs < 0 ||
    timeoutMs > 0x7fff_ffff
  ) {
    throw new TypeError(
      'whenSynced timeoutMs must be an integer from 0 to 2147483647',
    );
  }
  return timeoutMs;
}

function syncError(code: string, message: string): ClientError {
  return new ClientError({code, message});
}

function isInitResult(
  value: unknown,
): value is {revision: number; sourceConfigured: boolean} {
  return (
    isRecord(value) &&
    Object.hasOwn(value, 'revision') &&
    Object.hasOwn(value, 'sourceConfigured') &&
    Reflect.ownKeys(value).every(
      (key) => key === 'revision' || key === 'sourceConfigured',
    ) &&
    Number.isSafeInteger(value.revision) &&
    Number(value.revision) >= 0 &&
    typeof value.sourceConfigured === 'boolean'
  );
}
