import {
  arrayIsArray,
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
  STATEMENT_COMMANDS,
  STATEMENT_PREPARED,
  STATEMENT_SELECT,
  STATEMENT_SQL,
  type ApplyOutcome,
  type ChangedKeys,
  type JsonValue,
  type QueryOptions,
  type ResultField,
  type Results,
  type Row,
  type Schema,
  type SetSchemaOptions,
  type SqlResult,
  type StatementResponse,
  type StorageOptions,
} from '../protocol.js';
import {clientError} from './error.js';
import {
  createWorkerRpc,
  invalidResult,
  type PageRpc,
  type ResultValidation,
} from './rpc.js';

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
  getSchema(): Promise<Schema>;
  setSchema(schema: Schema, options?: SetSchemaOptions): Promise<boolean>;
  check(): Promise<void>;
  close(): Promise<void>;
}

type PreparedStatementState = {
  readonly owner: object;
  readonly statementId: number;
  readonly assertClientOpen: () => void;
  // The executions sent and not yet settled, in a transaction or not, and what
  // lets a close() that waits for them go on once there are none.
  inFlight: number;
  idle: (() => void) | undefined;
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
// The commands whose results carry a row count, and those whose count is the
// rows they changed. A statement's command is one of a few short words, which
// comparing costs less than a regular expression's test.
const countsRows = (command: string): boolean =>
  command === 'SELECT' || changesRows(command);
const changesRows = (command: string): boolean =>
  command === 'INSERT' || command === 'UPDATE' || command === 'DELETE';

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

  // Sends a direct statement once beginDirect would let it: at once, when the
  // database is already ready, rather than after turns of the microtask queue.
  // A failed check, there or in `send`, rejects rather than throws.
  const direct = <Result>(send: () => Promise<Result>): Promise<Result> => {
    if (!ready) {
      return beginDirect().then(send);
    }
    try {
      assertNoActiveTransaction();
      return send();
    } catch (error) {
      return Promise.reject(error);
    }
  };

  // Reads a statement's result as its response arrives, when it came as a
  // result object.
  const readResults = <RowType>(result: SqlResult): Results<RowType> => {
    noteRevision(result.revision);
    return toResults<RowType>(result);
  };

  // Reads a statement's result as its response arrives, when it came flat. The
  // Results are, in every property, value and property order, what toResults()
  // makes of the result object that the response stands for. This is one
  // function, with the revision noted in it, because it runs for nearly every
  // statement, where a call costs more than the work it would tidy away.
  const readStatement = <RowType>(
    response: StatementResponse,
  ): Results<RowType> => {
    const command = response[2];
    const seen = response[3];
    const rowCount = response[4];
    if (seen > revision) {
      revision = seen;
    }
    if (command === STATEMENT_SELECT) {
      const data = readData(response[5] as string);
      return {
        rows: data.rows as RowType[],
        fields: data.fields as ResultField[],
        affectedRows: 0,
        command: STATEMENT_COMMANDS[STATEMENT_SELECT],
        rowCount,
        revision: seen,
        tables: [],
        keys: {},
      };
    }
    // A write's response names the one table it changed, if any, and then, if
    // that table's keys are reported, their columns and each key's values. A
    // table or a column may have a name that every object inherits, such as
    // `__proto__` or `constructor`. Assigning to one of those may set the
    // prototype, or call a setter, or throw, as it does on a page that has
    // frozen Object.prototype. A computed key in a literal defines the
    // property whatever its name, as parsing JSON does. Any other name is
    // assigned, which costs less.
    const length = response.length;
    const tables = length > 5 ? [response[5] as string] : [];
    let keys: ChangedKeys = {};
    if (length > 6) {
      const table = response[5] as string;
      const width = response[6] as number;
      const changed: Row[] = [];
      for (let at = 7 + width; at < length; at += width) {
        let row: Row = {};
        for (let column = 0; column < width; column++) {
          const name = response[7 + column] as string;
          const value = response[at + column] as JsonValue;
          if (name in row) {
            row = {...row, [name]: value};
          } else {
            row[name] = value;
          }
        }
        changed.push(row);
      }
      if (table in keys) {
        keys = {[table]: changed};
      } else {
        keys[table] = changed;
      }
    }
    return {
      rows: [],
      fields: [],
      affectedRows: rowCount,
      command: STATEMENT_COMMANDS[command] as string,
      rowCount,
      revision: seen,
      tables,
      keys,
    };
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
      readStatement,
      readResults,
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

    query: <RowType = Row>(
      sql: string,
      params: JsonValue[] = [],
      options?: QueryOptions,
    ): Promise<Results<RowType>> => {
      // A ready database that no transaction owns takes the statement at once.
      // In any other state the general path waits or refuses. A statement
      // that waited is then sent without another look, as any request that
      // waited is: what began meanwhile is the Worker's to answer for.
      if (!ready || transactionActive || closing) {
        return direct(() =>
          rpc.execute(
            STATEMENT_SQL,
            sql,
            0,
            options !== undefined && asksForArrayRows(options),
            params,
            readStatement<RowType>,
            readResults<RowType>,
          ),
        );
      }
      try {
        return rpc.execute(
          STATEMENT_SQL,
          sql,
          0,
          options !== undefined && asksForArrayRows(options),
          params,
          readStatement<RowType>,
          readResults<RowType>,
        );
      } catch (error) {
        return Promise.reject(error);
      }
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
        assertClientOpen: assertOpen,
        inFlight: 0,
        idle: undefined,
        closed: false,
        clientClosed: false,
      };
      const closeRemote = async (): Promise<void> => {
        if (!closing && !closed) {
          await rpc.request('closePrepared', {statementId});
        }
      };
      const statement: PreparedStatement<RowType> = objFreeze({
        execute: (
          params: JsonValue[] = [],
          options?: QueryOptions,
        ): Promise<Results<RowType>> => {
          try {
            if (state.closed) {
              assertPreparedStatementOpen(state);
            }
            // The statement exists, so the database became ready, and it is
            // not closed, so its client is open. Only an active transaction can
            // still forbid a direct execution, which the full check reports.
            if (transactionActive) {
              assertNoActiveTransaction();
            }
            let arrayRows = false;
            if (options !== undefined) {
              arrayRows = asksForArrayRows(options);
              // Reading the options ran the caller's code, in an accessor or
              // a proxy, which may have closed the client since the checks
              // above.
              assertNoActiveTransaction();
            }
            state.inFlight++;
            return rpc.execute(
              STATEMENT_PREPARED,
              statementId,
              0,
              arrayRows,
              params,
              readStatement<RowType>,
              readResults<RowType>,
              executionSettled,
              state,
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
            assertNoActiveTransaction();
          } catch (error) {
            return Promise.reject(error);
          }
          state.closed = true;
          state.closePromise = (async () => {
            // The statement's executions in flight settle first, whatever
            // their outcome. No more can begin now that it is closed.
            await (state.inFlight > 0
              ? new Promise<void>((resolve) => {
                  state.idle = resolve;
                })
              : undefined);
            if (!state.clientClosed) {
              await closeRemote();
            }
          })().finally(() => preparedStatements.delete(state));
          trackPreparedClose(state.closePromise);
          return state.closePromise;
        },

        get closed(): boolean {
          return state.closed;
        },
      });
      preparedStatements.add(state);
      preparedStatementStates.set(statement, state);
      return statement;
    },

    /** Executes one or more SQL statements without parameters. */
    exec: async (sql: string, options?: QueryOptions): Promise<Results[]> => {
      await beginDirect();
      assertQueryOptions(options);
      const results = await rpc.request('execSql', {
        sql,
        ...rowModeParam(options),
      });
      for (const result of results) {
        noteRevision(result.revision);
      }
      return results.map((result) => toResults(result));
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

    getSchema: (): Promise<Schema> =>
      direct(() => rpc.request('schema', undefined)),

    setSchema: (
      schema: Schema,
      {drop = false}: SetSchemaOptions = {},
    ): Promise<boolean> =>
      direct(() => rpc.request('setSchema', {schema, drop})),

    check: (): Promise<void> => direct(() => rpc.request('check', undefined)),

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

/**
 * Builds the transaction handed to a callback, plus the three controls its
 * client needs. Keeping those off the transaction object means a callback
 * cannot seal or settle the transaction it is running inside.
 */
const createTransactionSession = (
  rpc: PageRpc,
  transactionId: string,
  preparedOwner: object,
  noteRevision: (revision: number) => void,
  readStatement: <RowType>(response: StatementResponse) => Results<RowType>,
  readResults: <RowType>(result: SqlResult) => Results<RowType>,
): TransactionSession => {
  // The scripts and the rollback in flight.
  const pending = new Set<Promise<unknown>>();
  // The statements in flight. They are counted as they are sent and as their
  // responses arrive, rather than held in the set above, so that a statement
  // costs no reaction on its promise.
  let inFlight = 0;
  // What settle() waits on while statements are in flight.
  let landing: {resolve(): void; reject(error: unknown): void} | undefined;
  // The failures of the statements that failed within the last turn of the
  // microtask queue, oldest first.
  const failures: unknown[] = [];
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

  // Tracks a script until it settles.
  const track = <Result>(promise: Promise<Result>): Promise<Result> => {
    assertOpen();
    pending.add(promise);
    forget(promise);
    return promise;
  };

  // Counts a statement as settled, and a prepared statement's execution with
  // it. A failure reaches a settle() that is waiting at once. One that arrives
  // before settle() begins is kept for one turn of the microtask queue, and
  // fails a settle() that begins within it: a Worker that answers within the
  // queue, as a stand-in on the page may, can fail a statement after its
  // callback has returned and before the transaction has looked at what is in
  // flight. After that turn the failure is its caller's alone to handle, as
  // one that the callback awaited and caught is.
  const statementSettled = (
    failed: boolean,
    error: unknown,
    statement: PreparedStatementState | undefined,
  ): void => {
    inFlight--;
    if (
      statement !== undefined &&
      --statement.inFlight === 0 &&
      statement.idle !== undefined
    ) {
      queueMicrotask(statement.idle);
    }
    if (landing !== undefined) {
      if (failed) {
        landing.reject(error);
      } else if (inFlight === 0) {
        landing.resolve();
      }
    } else if (failed) {
      failures.push(error);
      queueMicrotask(() => failures.shift());
    }
  };

  const transaction: Transaction = objFreeze({
    query: <RowType = Row>(
      sql: string,
      params: JsonValue[] = [],
      options?: QueryOptions,
    ): Promise<Results<RowType>> => {
      if (!open || closing) {
        assertOpen();
      }
      const arrayRows = options !== undefined && asksForArrayRows(options);
      inFlight++;
      return rpc.execute(
        STATEMENT_SQL,
        sql,
        transactionId,
        arrayRows,
        params,
        readStatement<RowType>,
        readResults<RowType>,
        statementSettled,
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
        rpc.request(
          'execSql',
          {sql, transactionId, ...rowModeParam(options)},
          (results) => {
            for (const result of results) {
              noteRevision(result.revision);
            }
            return results.map((result) => toResults(result));
          },
        ),
      );
    },

    execute: <RowType = Row>(
      statement: PreparedStatement<RowType>,
      params: JsonValue[] = [],
      options?: QueryOptions,
    ): Promise<Results<RowType>> => {
      if (!open || closing) {
        assertOpen();
      }
      // A value that is no statement, an object or not, has no state.
      const state = preparedStatementStates.get(statement);
      if (state === undefined) {
        throw clientError(
          'INVALID_PREPARED_STATEMENT',
          'The value is not a TinyJoin prepared statement',
        );
      }
      if (state.owner !== preparedOwner) {
        throw clientError(
          'PREPARED_STATEMENT_CLIENT_MISMATCH',
          'The prepared statement belongs to a different TinyJoin client',
        );
      }
      if (state.closed) {
        assertPreparedStatementOpen(state);
      }
      const arrayRows = options !== undefined && asksForArrayRows(options);
      inFlight++;
      state.inFlight++;
      return rpc.execute(
        STATEMENT_PREPARED,
        state.statementId,
        transactionId,
        arrayRows,
        params,
        readStatement<RowType>,
        readResults<RowType>,
        statementSettled,
        state,
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

    // Waits until nothing the transaction sent is in flight. If any of it
    // fails meanwhile, this fails with it at once, so that the transaction
    // rolls back rather than commits around a statement nobody awaited.
    settle: async (): Promise<void> => {
      if (failures.length > 0) {
        throw failures[0];
      }
      while (inFlight > 0 || pending.size > 0) {
        const landed =
          inFlight > 0 &&
          new Promise<void>((resolve, reject) => {
            landing = {resolve, reject};
          });
        await Promise.all([...pending, landed]);
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

// Refuses a statement that may not run: with its client's error if the client
// has closed, and otherwise with its own if it has. A client that closes seals
// every statement it still has, so a statement that is not closed has an open
// client, and one test of `closed` clears a statement of both before this is
// called.
const assertPreparedStatementOpen = (state: PreparedStatementState): void => {
  state.assertClientOpen();
  if (state.closed) {
    throw clientError(
      'PREPARED_STATEMENT_CLOSED',
      'The TinyJoin prepared statement is closed',
    );
  }
};

// Counts a prepared statement's execution as settled, whatever its outcome,
// and lets a close() that was waiting for the last of them go on.
const executionSettled = (
  _failed: boolean,
  _error: unknown,
  state: PreparedStatementState,
): void => {
  if (--state.inFlight === 0 && state.idle !== undefined) {
    queueMicrotask(state.idle);
  }
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

// A query asks the Worker for array rows, which it then writes itself.
const rowModeParam = (
  options: QueryOptions | undefined,
): {rowMode?: 'array'} => (options?.rowMode === 'array' ? {rowMode: 'array'} : {});

// Checks the options a statement was given, and says whether they ask for
// array rows. A statement given none, as nearly all are, does not call this.
const asksForArrayRows = (options: QueryOptions): boolean => {
  assertQueryOptions(options);
  return options.rowMode === 'array';
};

// The rows of a statement that returns none, such as an INSERT's.
const NO_ROWS = '{"fields":[],"rows":[]}';

// A result's fields and rows arrive as JSON text, parsed here only once they
// are used: an object holding an array of each. An array holds neither, so it
// needs no test of its own. Every read comes through here, so the text is
// parsed and its shape tested in place.
const readData = (text: string): {fields: unknown[]; rows: unknown[]} => {
  let data: {fields?: unknown; rows?: unknown} | null | undefined;
  if (text === NO_ROWS) {
    data = {fields: [], rows: []};
  } else {
    try {
      data = JSON.parse(text);
    } catch {
      // Text that is not JSON has no fields or rows either.
    }
  }
  if (
    typeof data !== 'object' ||
    data === null ||
    !arrayIsArray(data.fields) ||
    !arrayIsArray(data.rows)
  ) {
    throw invalidResult();
  }
  return data as {fields: unknown[]; rows: unknown[]};
};

// The public result of a statement, from the result object it came as.
const toResults = <RowType>(result: SqlResult): Results<RowType> => {
  const data = readData(result.data);
  const command = result.command;
  const affectedRows = changesRows(command) ? result.rowCount : 0;
  return countsRows(command)
    ? {
        rows: data.rows as RowType[],
        fields: data.fields as ResultField[],
        affectedRows,
        command,
        rowCount: result.rowCount,
        revision: result.revision,
        tables: result.tables,
        keys: result.keys,
      }
    : {
        rows: data.rows as RowType[],
        fields: data.fields as ResultField[],
        affectedRows,
        command,
        revision: result.revision,
        tables: result.tables,
        keys: result.keys,
      };
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
