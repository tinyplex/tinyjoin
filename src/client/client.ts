import {
  finishChangedKeys,
  isFunction,
  isRecord,
  isString,
  isUndefined,
  mathMax,
  mergeChangedKeys,
  objFreeze,
  ownKeys,
  type PendingChangedKeys,
} from '../common.js';
import {createDefaultWorker, createUrlWorker} from '../default-worker.js';
import {
  type ApplyOutcome,
  type JsonValue,
  type QueryOptions,
  type Results,
  type Row,
  type SqlResult,
  type StorageOptions,
} from '../protocol.js';
import {clientError} from './error.js';
import {createWorkerRpc, type ResultValidation, type WorkerRpc} from './rpc.js';

export interface WorkerLike {
  postMessage(message: unknown): void;
  addEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  addEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  addEventListener(type: 'error', listener: (event: ErrorEvent) => void): void;
  removeEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(
    type: 'messageerror',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(
    type: 'error',
    listener: (event: ErrorEvent) => void,
  ): void;
  terminate?: () => void;
}

export interface ClientOptions {
  worker?: WorkerLike;
  workerFactory?: () => WorkerLike;
  workerUrl?: string | URL;
  dataDir?: DataDir;
}

export type DataDir = string;

export interface TablesChangedEvent extends ApplyOutcome {
  reset?: boolean;
}

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

export interface Client {
  readonly waitReady: Promise<void>;
  readonly ready: boolean;
  readonly closed: boolean;
  query<RowType = Row>(
    sql: string,
    params?: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>>;
  sql<RowType = Row>(
    strings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<Results<RowType>>;
  prepare<RowType = Row>(sql: string): Promise<PreparedStatement<RowType>>;
  exec(sql: string, options?: QueryOptions): Promise<Results[]>;
  transaction<Result>(
    callback: (transaction: Transaction) => Result | Promise<Result>,
  ): Promise<Result>;
  subscribe(
    options: SubscriptionOptions,
    listener: (event: TablesChangedEvent) => void,
  ): () => void;
  getRevision(): number;
  close(): Promise<void>;
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

/** A transaction, alongside the controls only its own client may use. */
type TransactionSession = {
  readonly transaction: Transaction;
  readonly seal: () => void;
  readonly settle: () => Promise<void>;
  readonly rolledBack: () => boolean;
};

type Subscription = {
  readonly tables?: Set<string>;
  readonly listener: (event: TablesChangedEvent) => void;
};

const SUPPORTED_OPTIONS = ['dataDir', 'worker', 'workerFactory', 'workerUrl'];
const TRANSACTION_ACTIVE = 'TRANSACTION_ACTIVE';
const OPFS_PREFIX = 'opfs://';
const DATABASE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
const ROW_COUNT_COMMANDS = /^(?:DELETE|INSERT|SELECT|UPDATE)$/;
const AFFECTED_ROW_COMMANDS = /^(?:DELETE|INSERT|UPDATE)$/;

// A prepared statement's state lives here rather than on the statement itself,
// so that a transaction can recognize a statement belonging to its own client
// without the statement exposing anything a page could reach or replace.
const preparedStatementStates = new WeakMap<object, PreparedStatementState>();

/**
 * Opens a database on one dedicated Worker.
 *
 * Client stays a constructor, so that `new Client(...)` and `instanceof Client`
 * keep working, but what it hands back is the frozen object createClient
 * builds. Everything the client knows is a closure variable rather than a
 * property: none of it reaches the published bundle as a name, and none of it
 * can be read or overwritten from outside.
 */
export class Client {
  constructor(options: ClientOptions = {}) {
    return createClient(options);
  }
}

const createClient = (options: ClientOptions): Client => {
  assertClientOptions(options);
  const storage = storageFromDataDir(options.dataDir);
  const [worker, resultValidation] = createWorker(options);
  const rpc = createWorkerRpc(worker, resultValidation);
  const preparedOwner = {};
  const preparedStatements = new Set<PreparedStatementState>();
  const subscriptions = new Set<Subscription>();
  let revision = 0;
  let ready = false;
  let closing = false;
  let closed = false;
  let closePromise: Promise<void> | undefined;
  let preparedCloseGeneration = 0;
  let preparedCloseTail: Promise<void> = Promise.resolve();
  let transactionTail: Promise<void> = Promise.resolve();
  let transactionActive = false;
  let pendingNotification: TablesChangedEvent | undefined;
  const pendingKeys: PendingChangedKeys = new Map();

  // Cross-tab notifications can arrive while this Client has a callback open
  // (or is waiting to begin one). Defer listeners so their documented re-query
  // pattern does not fail with TRANSACTION_ACTIVE.
  const flushNotifications = (): void => {
    if (transactionActive || closing || !pendingNotification) return;
    const event = pendingNotification;
    pendingNotification = undefined;
    pendingKeys.clear();
    for (const subscription of subscriptions) {
      if (
        event.reset || !subscription.tables ||
        event.tables.some((table) => subscription.tables?.has(table))
      ) subscription.listener(event);
    }
  };

  const noteRevision = (next: number): void => {
    revision = mathMax(revision, next);
  };

  const assertOpen = (): void => {
    if (closing || closed) {
      throw clientError('CLIENT_CLOSED', 'The TinyJoin client is closed');
    }
  };

  const assertNoActiveTransaction = (): void => {
    assertOpen();
    if (transactionActive) {
      throw clientError(
        TRANSACTION_ACTIVE,
        'Use the transaction object while a TinyJoin transaction is active',
      );
    }
  };

  // Every direct statement waits for the database to be ready, and refuses to
  // run while a transaction owns the Worker.
  const beginDirect = async (): Promise<void> => {
    await waitReady;
    assertNoActiveTransaction();
  };

  const executePrepared = async <RowType>(
    statementId: number,
    params: JsonValue[],
    options?: QueryOptions,
  ): Promise<Results<RowType>> => {
    await beginDirect();
    const result = await rpc.request('executePrepared', {statementId, params});
    noteRevision(result.revision);
    return toResults<RowType>(result, options);
  };

  const trackPreparedClose = (close: Promise<void>): void => {
    const previous = preparedCloseTail;
    preparedCloseGeneration += 1;
    preparedCloseTail = Promise.allSettled([previous, close]).then(
      () => undefined,
    );
  };

  // Reserves the client for a transaction once no prepared close is in flight.
  // The generation check and the reservation are synchronous, so a new prepared
  // close cannot slip between the drained barrier and the Worker request.
  const beginTransaction = async (): Promise<string> => {
    while (true) {
      assertOpen();
      const generation = preparedCloseGeneration;
      const tail = preparedCloseTail;
      await tail;
      if (
        generation !== preparedCloseGeneration ||
        tail !== preparedCloseTail
      ) {
        continue;
      }
      assertOpen();
      transactionActive = true;
      try {
        return (await rpc.request('beginTransaction', undefined)).transactionId;
      } catch (error) {
        transactionActive = false;
        queueMicrotask(flushNotifications);
        throw error;
      }
    }
  };

  const runTransaction = async <Result>(
    callback: (transaction: Transaction) => Result | Promise<Result>,
  ): Promise<Result> => {
    await waitReady;
    const transactionId = await beginTransaction();
    const session = createTransactionSession(
      rpc,
      transactionId,
      preparedOwner,
      noteRevision,
    );
    let shouldRollback = true;
    try {
      const result = await callback(session.transaction);
      session.seal();
      await session.settle();
      if (session.rolledBack()) {
        return result;
      }
      const outcome = await rpc.request('commitTransaction', {transactionId});
      shouldRollback = false;
      noteRevision(outcome.revision);
      return result;
    } catch (error) {
      session.seal();
      if (shouldRollback && !session.rolledBack()) {
        await rpc
          .request('rollbackTransaction', {transactionId})
          .catch(() => undefined);
      }
      throw error;
    } finally {
      transactionActive = false;
      queueMicrotask(flushNotifications);
    }
  };

  const closeOnce = async (): Promise<void> => {
    try {
      await waitReady;
      await rpc.request('close', undefined);
    } finally {
      ready = false;
      closing = false;
      closed = true;
      subscriptions.clear();
      pendingNotification = undefined;
      pendingKeys.clear();
      rpc.dispose();
    }
  };

  rpc.onEvent((event) => {
    if (event.event === 'tablesChanged' || event.event === 'resync') {
      noteRevision(event.payload.revision);
      const reset = event.event === 'resync' || pendingNotification?.reset;
      if (reset) {
        pendingKeys.clear();
      } else {
        mergeChangedKeys(
          pendingKeys,
          event.payload.tables,
          event.payload.keys,
        );
      }
      pendingNotification = {
        revision,
        tables: reset ? [] : [...new Set([
          ...pendingNotification?.tables ?? [], ...event.payload.tables,
        ])],
        // A reset requires a full re-query, so naming individual keys would be misleading.
        keys: reset ? {} : finishChangedKeys(pendingKeys),
        ...(reset ? {reset: true} : {}),
      };
      flushNotifications();
    }
  });

  const waitReady = rpc.request('init', {storage}).then((result) => {
    revision = result.revision;
    ready = true;
  });

  const client: Client = {
    waitReady,

    get ready(): boolean {
      return ready && !closing && !closed;
    },

    get closed(): boolean {
      return closed;
    },

    query: async <RowType = Row>(
      sql: string,
      params: JsonValue[] = [],
      options?: QueryOptions,
    ): Promise<Results<RowType>> => {
      await beginDirect();
      assertQueryOptions(options);
      const result = await rpc.request('executeSql', {sql, params});
      noteRevision(result.revision);
      return toResults<RowType>(result, options);
    },

    sql: <RowType = Row>(
      strings: TemplateStringsArray,
      ...params: JsonValue[]
    ): Promise<Results<RowType>> =>
      client.query<RowType>(parameterize(strings, params), params),

    prepare: async <RowType = Row>(
      sql: string,
    ): Promise<PreparedStatement<RowType>> => {
      await beginDirect();
      const {statementId} = await rpc.request('prepareSql', {sql});
      assertOpen();

      const state: PreparedStatementState = {
        owner: preparedOwner,
        statementId,
        inFlight: new Set(),
        assertDirectOperationAllowed: assertNoActiveTransaction,
        assertClientOpen: assertOpen,
        executeDirect: (params, options) =>
          executePrepared<unknown>(statementId, params, options),
        closeRemote: async () => {
          if (!closing && !closed) {
            await rpc.request('closePrepared', {statementId});
          }
        },
        trackClose: trackPreparedClose,
        unregister: () => preparedStatements.delete(state),
        closed: false,
        clientClosed: false,
      };
      preparedStatements.add(state);
      return createPreparedStatement<RowType>(state);
    },

    /** Executes one or more SQL statements without parameters. */
    exec: async (sql: string, options?: QueryOptions): Promise<Results[]> => {
      await beginDirect();
      assertQueryOptions(options);
      const results = await rpc.request('execSql', {sql});
      for (const result of results) {
        noteRevision(result.revision);
      }
      return results.map((result) => toResults(result, options));
    },

    /**
     * Runs SQL against an isolated staged database and durably publishes all
     * changes together when the callback succeeds, unless it explicitly rolls
     * back.
     */
    transaction: <Result>(
      callback: (transaction: Transaction) => Result | Promise<Result>,
    ): Promise<Result> => {
      if (!isFunction(callback)) {
        throw new TypeError('TinyJoin transaction requires a callback');
      }
      const run = transactionTail.then(() => runTransaction(callback));
      transactionTail = run.then(
        () => undefined,
        () => undefined,
      );
      return run;
    },

    subscribe: (
      options: SubscriptionOptions,
      listener: (event: TablesChangedEvent) => void,
    ): (() => void) => {
      const subscription: Subscription = {
        ...(options.tables ? {tables: new Set(options.tables)} : {}),
        listener,
      };
      subscriptions.add(subscription);
      return () => subscriptions.delete(subscription);
    },

    getRevision: (): number => revision,

    close: (): Promise<void> => {
      if (isUndefined(closePromise)) {
        closing = true;
        for (const statement of preparedStatements) {
          statement.closed = true;
          statement.clientClosed = true;
        }
        preparedStatements.clear();
        closePromise = closeOnce();
      }
      return closePromise;
    },
  };

  return objFreeze(Object.setPrototypeOf(client, Client.prototype) as Client);
};

const createPreparedStatement = <RowType>(
  state: PreparedStatementState,
): PreparedStatement<RowType> => {
  const statement = objFreeze({
    execute: (
      params: JsonValue[] = [],
      options?: QueryOptions,
    ): Promise<Results<RowType>> => {
      try {
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
    },

    close: (): Promise<void> => {
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
    },

    get closed(): boolean {
      return state.closed;
    },
  });
  preparedStatementStates.set(statement, state);
  return statement;
};

/**
 * Builds the transaction handed to a callback, plus the three controls its
 * client needs. Keeping those off the transaction object means a callback
 * cannot seal or settle the transaction it is running inside.
 */
const createTransactionSession = (
  rpc: WorkerRpc,
  transactionId: string,
  preparedOwner: object,
  noteRevision: (revision: number) => void,
): TransactionSession => {
  const pending = new Set<Promise<unknown>>();
  let open = true;
  let closing = false;
  let rolledBack = false;
  let rollbackPromise: Promise<void> | undefined;

  const assertOpen = (): void => {
    if (!open || closing) {
      throw clientError(
        'TRANSACTION_CLOSED',
        'The TinyJoin transaction callback has already completed',
      );
    }
  };

  const forget = (promise: Promise<unknown>): void => {
    void promise.then(
      () => pending.delete(promise),
      () => pending.delete(promise),
    );
  };

  const track = <Result>(promise: Promise<Result>): Promise<Result> => {
    assertOpen();
    pending.add(promise);
    forget(promise);
    return promise;
  };

  const transaction: Transaction = objFreeze({
    query: <RowType = Row>(
      sql: string,
      params: JsonValue[] = [],
      options?: QueryOptions,
    ): Promise<Results<RowType>> => {
      assertOpen();
      assertQueryOptions(options);
      return track(
        rpc
          .request('executeSql', {sql, params, transactionId})
          .then((result) => {
            noteRevision(result.revision);
            return toResults<RowType>(result, options);
          }),
      );
    },

    sql: <RowType = Row>(
      strings: TemplateStringsArray,
      ...params: JsonValue[]
    ): Promise<Results<RowType>> =>
      transaction.query<RowType>(parameterize(strings, params), params),

    exec: (sql: string, options?: QueryOptions): Promise<Results[]> => {
      assertOpen();
      assertQueryOptions(options);
      return track(
        rpc.request('execSql', {sql, transactionId}).then((results) => {
          for (const result of results) {
            noteRevision(result.revision);
          }
          return results.map((result) => toResults(result, options));
        }),
      );
    },

    execute: <RowType = Row>(
      statement: PreparedStatement<RowType>,
      params: JsonValue[] = [],
      options?: QueryOptions,
    ): Promise<Results<RowType>> => {
      assertOpen();
      const state = preparedStatementState(statement);
      if (state.owner !== preparedOwner) {
        throw clientError(
          'PREPARED_STATEMENT_CLIENT_MISMATCH',
          'The prepared statement belongs to a different TinyJoin client',
        );
      }
      state.assertClientOpen();
      assertPreparedStatementOpen(state);
      assertQueryOptions(options);
      return trackPreparedExecution(
        state,
        track(
          rpc
            .request('executePrepared', {
              statementId: state.statementId,
              params,
              transactionId,
            })
            .then((result) => {
              noteRevision(result.revision);
              return toResults<RowType>(result, options);
            }),
        ),
      );
    },

    rollback: (): Promise<void> => {
      assertOpen();
      closing = true;
      const rollback = rpc
        .request('rollbackTransaction', {transactionId})
        .then(() => {
          rolledBack = true;
          open = false;
          closing = false;
        });
      rollbackPromise = rollback;
      pending.add(rollback);
      forget(rollback);
      return rollback;
    },

    get closed(): boolean {
      return !open;
    },
  });

  return {
    transaction,

    seal: (): void => {
      open = false;
    },

    settle: async (): Promise<void> => {
      while (pending.size > 0) {
        await Promise.all([...pending]);
      }
      await rollbackPromise;
    },

    rolledBack: (): boolean => rolledBack,
  };
};

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
  if (isString(dataDirOrOptions)) {
    if (!isUndefined(options?.dataDir)) {
      throw new TypeError(
        'Provide the TinyJoin data directory either positionally or in options.dataDir, not both',
      );
    }
    resolvedOptions = {...options, dataDir: dataDirOrOptions};
  } else if (isUndefined(dataDirOrOptions)) {
    resolvedOptions = options ?? {};
  } else {
    if (!isUndefined(options)) {
      throw new TypeError(
        'TinyJoin options must be the first argument when no positional data directory is used',
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

// TinyJoin's own Worker has already validated every result it posts, so the
// client only re-checks the envelope on that path. A Worker the application
// supplied has made no such promise, and gets the full walk.
const createWorker = (
  options: ClientOptions,
): [worker: WorkerLike, resultValidation: ResultValidation] => {
  const {worker, workerFactory, workerUrl} = options;
  if (
    [worker, workerFactory, workerUrl].filter((value) => !isUndefined(value))
      .length > 1
  ) {
    throw new TypeError(
      'Provide only one of worker, workerFactory, or workerUrl to TinyJoin',
    );
  }
  if (worker) {
    return [worker, 'full'];
  }
  if (workerFactory) {
    return [workerFactory(), 'full'];
  }
  if (workerUrl) {
    return [createUrlWorker(workerUrl), 'full'];
  }
  return [
    createDefaultWorker(options.dataDir?.startsWith(OPFS_PREFIX)),
    'header',
  ];
};

const preparedStatementState = (value: unknown): PreparedStatementState => {
  const state = isRecord(value)
    ? preparedStatementStates.get(value)
    : undefined;
  if (isUndefined(state)) {
    throw clientError(
      'INVALID_PREPARED_STATEMENT',
      'The value is not a TinyJoin prepared statement',
    );
  }
  return state;
};

const assertPreparedStatementOpen = (state: PreparedStatementState): void => {
  if (state.closed) {
    throw clientError(
      'PREPARED_STATEMENT_CLOSED',
      'The TinyJoin prepared statement is closed',
    );
  }
};

const trackPreparedExecution = <Result>(
  state: PreparedStatementState,
  operation: Promise<Result>,
): Promise<Result> => {
  state.inFlight.add(operation);
  void operation.then(
    () => state.inFlight.delete(operation),
    () => state.inFlight.delete(operation),
  );
  return operation;
};

const assertClientOptions = (options: ClientOptions): void => {
  const prototype = isRecord(options)
    ? Object.getPrototypeOf(options)
    : undefined;
  if (
    !isRecord(options) ||
    (prototype !== Object.prototype && prototype !== null) ||
    ownKeys(options).some(
      (key) => !isString(key) || !SUPPORTED_OPTIONS.includes(key),
    )
  ) {
    throw new TypeError(
      'TinyJoin client options support only dataDir, worker, workerFactory, and workerUrl',
    );
  }
};

const storageFromDataDir = (dataDir: DataDir | undefined): StorageOptions => {
  if (isUndefined(dataDir) || dataDir === 'memory://') {
    return {kind: 'memory'};
  }
  if (!isString(dataDir) || !dataDir.startsWith(OPFS_PREFIX)) {
    throw new TypeError(
      'TinyJoin dataDir must be memory:// or opfs:// followed by a database name',
    );
  }
  const name = dataDir.slice(OPFS_PREFIX.length);
  if (!DATABASE_NAME.test(name)) {
    throw new TypeError(
      'A TinyJoin OPFS database name must be 1-64 ASCII letters, numbers, dots, underscores, or hyphens, and start with a letter or number',
    );
  }
  return {kind: 'opfs', name};
};

const parameterize = (
  strings: TemplateStringsArray,
  params: readonly JsonValue[],
): string => {
  if (!Array.isArray(strings) || strings.length !== params.length + 1) {
    throw new TypeError('TinyJoin sql must be used as a tagged template');
  }
  let sql = strings[0] ?? '';
  for (let index = 0; index < params.length; index++) {
    sql += `$${index + 1}${strings[index + 1] ?? ''}`;
  }
  return sql;
};

const toResults = <RowType>(
  result: SqlResult,
  options?: QueryOptions,
): Results<RowType> => ({
  rows: (options?.rowMode === 'array'
    ? rowsAsArrays(result.rows, result.fields)
    : result.rows) as RowType[],
  fields: result.fields,
  affectedRows: AFFECTED_ROW_COMMANDS.test(result.command)
    ? result.rowCount
    : 0,
  command: result.command,
  ...(ROW_COUNT_COMMANDS.test(result.command)
    ? {rowCount: result.rowCount}
    : {}),
  revision: result.revision,
  tables: result.tables,
  keys: result.keys,
});

const rowsAsArrays = (
  rows: Row[],
  fields: SqlResult['fields'],
): JsonValue[][] => {
  if (rows.length > 0 && fields.length === 0) {
    throw clientError(
      'ROW_METADATA_UNAVAILABLE',
      'TinyJoin cannot return array rows without field metadata',
    );
  }
  return rows.map((row) => fields.map((field) => row[field.name] ?? null));
};

const assertQueryOptions = (options: QueryOptions | undefined): void => {
  if (isUndefined(options)) {
    return;
  }
  if (
    !isRecord(options) ||
    ownKeys(options).some((key) => key !== 'rowMode') ||
    (!isUndefined(options.rowMode) &&
      options.rowMode !== 'array' &&
      options.rowMode !== 'object')
  ) {
    throw new TypeError(
      'TinyJoin query options currently support only rowMode: object or array',
    );
  }
};
