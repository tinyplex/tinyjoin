import {
  isRecord,
  type ApplyOutcome,
  type JsonValue,
  type QueryOptions,
  type Results,
  type Row,
  type SqlResult,
  type StorageOptions,
} from '../protocol.js';
import {ClientError} from './error.js';
import {
  WorkerRpc,
  type ResultValidation,
  type WorkerLike,
} from './rpc.js';

export interface ClientOptions {
  worker?: WorkerLike;
  workerFactory?: () => WorkerLike;
  workerUrl?: string | URL;
  dataDir?: DataDir;
}

export type DataDir = string;

export interface TablesChangedEvent extends ApplyOutcome {}

export type SubscriptionOptions = {
  tables?: string[];
};

export interface PreparedStatement<RowType = Row> {
  execute(
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;
  close(): Promise<void>;
  readonly closed: boolean;
}

type PreparedStatementState = {
  readonly owner: object;
  readonly statementId: number;
  readonly inFlight: Set<Promise<unknown>>;
  readonly assertDirectOperationAllowed: () => void;
  readonly assertClientOpen: () => void;
  readonly executeDirect: (
    params: JsonValue[],
    options?: QueryOptions,
  ) => Promise<Results<unknown>>;
  readonly closeRemote: () => Promise<void>;
  readonly trackClose: (close: Promise<void>) => void;
  readonly unregister: () => void;
  closed: boolean;
  clientClosed: boolean;
  closePromise?: Promise<void>;
};

const preparedStatementStates = new WeakMap<object, PreparedStatementState>();

class ClientPreparedStatement<RowType> implements PreparedStatement<RowType> {
  constructor(state: PreparedStatementState) {
    preparedStatementStates.set(this, state);
  }

  execute(
    params: JsonValue[] = [],
    options?: QueryOptions,
  ): Promise<Results<RowType>> {
    try {
      const state = preparedStatementState(this);
      state.assertClientOpen();
      assertPreparedStatementOpen(state);
      state.assertDirectOperationAllowed();
      assertQueryOptions(options);
      return trackPreparedExecution(
        state,
        state.executeDirect(params, options) as Promise<Results<RowType>>,
      );
    } catch (error) {
      return Promise.reject(error);
    }
  }

  close(): Promise<void> {
    let state: PreparedStatementState;
    try {
      state = preparedStatementState(this);
    } catch (error) {
      return Promise.reject(error);
    }
    if (state.closed) {
      return state.closePromise ?? Promise.resolve();
    }
    try {
      state.assertDirectOperationAllowed();
    } catch (error) {
      return Promise.reject(error);
    }

    state.closed = true;
    const pending = [...state.inFlight];
    state.closePromise = (async () => {
      await Promise.allSettled(pending);
      if (!state.clientClosed) {
        await state.closeRemote();
      }
    })().finally(state.unregister);
    state.trackClose(state.closePromise);
    return state.closePromise;
  }

  get closed(): boolean {
    return preparedStatementState(this).closed;
  }
}

export interface Transaction {
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
  execute<RowType = Row>(
    statement: PreparedStatement<RowType>,
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;
  rollback(): Promise<void>;
  readonly closed: boolean;
}

export class Client {
  readonly #rpc: WorkerRpc;
  readonly #preparedOwner = {};
  readonly #preparedStatements = new Set<PreparedStatementState>();
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
  #preparedCloseGeneration = 0;
  #preparedCloseTail: Promise<void> = Promise.resolve();
  #transactionTail: Promise<void> = Promise.resolve();
  #transactionActive = false;

  constructor(options: ClientOptions = {}) {
    assertClientOptions(options);
    const storage = storageFromDataDir(options.dataDir);
    const worker = createWorker(options);
    this.#rpc = new WorkerRpc(worker.worker, worker.resultValidation);
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

  async prepare<RowType = Row>(
    sql: string,
  ): Promise<PreparedStatement<RowType>> {
    await this.waitReady;
    this.#assertNoActiveTransaction();
    const {statementId} = await this.#rpc.request('prepareSql', {sql});
    this.#assertOpen();

    let state!: PreparedStatementState;
    state = {
      owner: this.#preparedOwner,
      statementId,
      inFlight: new Set(),
      assertDirectOperationAllowed: () => this.#assertNoActiveTransaction(),
      assertClientOpen: () => this.#assertOpen(),
      executeDirect: (params, options) =>
        this.#executePrepared<unknown>(statementId, params, options),
      closeRemote: () => this.#closePrepared(statementId),
      trackClose: (close) => this.#trackPreparedClose(close),
      unregister: () => this.#preparedStatements.delete(state),
      closed: false,
      clientClosed: false,
    };
    this.#preparedStatements.add(state);
    return new ClientPreparedStatement<RowType>(state);
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
      for (const statement of this.#preparedStatements) {
        statement.closed = true;
        statement.clientClosed = true;
      }
      this.#preparedStatements.clear();
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
    const {transactionId} = await this.#beginTransactionAfterPreparedCloses();
    const transaction = new ClientTransaction(
      this.#rpc,
      transactionId,
      this.#preparedOwner,
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

  async #beginTransactionAfterPreparedCloses(): Promise<{
    transactionId: string;
  }> {
    while (true) {
      this.#assertOpen();
      const generation = this.#preparedCloseGeneration;
      const tail = this.#preparedCloseTail;
      await tail;
      if (
        generation !== this.#preparedCloseGeneration ||
        tail !== this.#preparedCloseTail
      ) {
        continue;
      }

      // Reserve the client before dispatching BEGIN. The generation check and
      // reservation are synchronous, so a new prepared close cannot slip
      // between the drained barrier and the Worker request.
      this.#assertOpen();
      this.#transactionActive = true;
      try {
        return await this.#rpc.request('beginTransaction', undefined);
      } catch (error) {
        this.#transactionActive = false;
        throw error;
      }
    }
  }

  #trackPreparedClose(close: Promise<void>): void {
    const previous = this.#preparedCloseTail;
    this.#preparedCloseGeneration += 1;
    this.#preparedCloseTail = Promise.allSettled([previous, close]).then(
      () => undefined,
    );
  }

  #assertNoActiveTransaction(): void {
    this.#assertOpen();
    if (this.#transactionActive) {
      throw clientError(
        'TRANSACTION_ACTIVE',
        'Use the transaction object while a TinyGres transaction is active',
      );
    }
  }

  #assertOpen(): void {
    if (this.#closing || this.#closed) {
      throw clientError('CLIENT_CLOSED', 'The TinyGres client is closed');
    }
  }

  async #executePrepared<RowType>(
    statementId: number,
    params: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>> {
    await this.waitReady;
    this.#assertNoActiveTransaction();
    const result = await this.#rpc.request('executePrepared', {
      statementId,
      params,
    });
    this.#revision = Math.max(this.#revision, result.revision);
    return toResults<RowType>(result, options);
  }

  async #closePrepared(statementId: number): Promise<void> {
    if (this.#closing || this.#closed) {
      return;
    }
    await this.#rpc.request('closePrepared', {statementId});
  }

  #noteResults(results: SqlResult[]): void {
    for (const result of results) {
      this.#revision = Math.max(this.#revision, result.revision);
    }
  }
}

class ClientTransaction implements Transaction {
  readonly #pending = new Set<Promise<unknown>>();
  #open = true;
  #closing = false;
  #rollbackCompleted = false;
  #rollbackPromise: Promise<void> | undefined;

  constructor(
    readonly rpc: WorkerRpc,
    readonly transactionId: string,
    readonly preparedOwner: object,
    readonly noteRevision: (revision: number) => void,
  ) {}

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

  execute<RowType = Row>(
    statement: PreparedStatement<RowType>,
    params: JsonValue[] = [],
    options?: QueryOptions,
  ): Promise<Results<RowType>> {
    this.#assertOpen();
    const state = preparedStatementState(statement);
    if (state.owner !== this.preparedOwner) {
      throw clientError(
        'PREPARED_STATEMENT_CLIENT_MISMATCH',
        'The prepared statement belongs to a different TinyGres client',
      );
    }
    state.assertClientOpen();
    assertPreparedStatementOpen(state);
    assertQueryOptions(options);
    const operation = this.#track(
      this.rpc
        .request('executePrepared', {
          statementId: state.statementId,
          params,
          transactionId: this.transactionId,
        })
        .then((result) => {
          this.noteRevision(result.revision);
          return toResults<RowType>(result, options);
        }),
    );
    return trackPreparedExecution(state, operation);
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

function createWorker(options: ClientOptions): {
  worker: WorkerLike;
  resultValidation: ResultValidation;
} {
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

  if (options.worker) {
    return {worker: options.worker, resultValidation: 'full'};
  }
  if (options.workerFactory) {
    return {worker: options.workerFactory(), resultValidation: 'full'};
  }
  if (options.workerUrl) {
    return {
      worker: createUrlWorker(options.workerUrl),
      resultValidation: 'full',
    };
  }
  return {worker: createDefaultWorker(), resultValidation: 'header'};
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

function preparedStatementState(value: unknown): PreparedStatementState {
  const state =
    typeof value === 'object' && value !== null
      ? preparedStatementStates.get(value)
      : undefined;
  if (!state) {
    throw clientError(
      'INVALID_PREPARED_STATEMENT',
      'The value is not a TinyGres prepared statement',
    );
  }
  return state;
}

function assertPreparedStatementOpen(state: PreparedStatementState): void {
  if (state.closed) {
    throw clientError(
      'PREPARED_STATEMENT_CLOSED',
      'The TinyGres prepared statement is closed',
    );
  }
}

function trackPreparedExecution<Result>(
  state: PreparedStatementState,
  operation: Promise<Result>,
): Promise<Result> {
  state.inFlight.add(operation);
  void operation.then(
    () => state.inFlight.delete(operation),
    () => state.inFlight.delete(operation),
  );
  return operation;
}

function assertClientOptions(options: ClientOptions): void {
  const supported = new Set([
    'dataDir',
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
      'TinyGres client options support only dataDir, worker, workerFactory, and workerUrl',
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
