import {
  asCodedError,
  finishChangedKeys,
  isRecord,
  isSafeInteger,
  isUndefined,
  mathMax,
  mergeChangedKeys,
  type PendingChangedKeys,
} from '../common.js';
import {ClientError} from '../client/error.js';
import {
  PROTOCOL_VERSION,
  isSqlResultText,
  isWorkerRequest,
  type ApplyOutcome,
  type SerializedError,
  type SqlResultText,
  type StorageOptions,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../protocol.js';
import {
  createMemoryWasmEngine,
  type WorkerEngine,
  type WorkerEngineFactory,
} from './engine.js';
import {createOpfsWasmEngine} from './opfs-loader.js';

export interface WorkerScope {
  postMessage(message: WorkerResponse | WorkerEvent): void;
  addEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(
    type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void;
  close(): void;
}

export interface StartWorkerOptions {
  scope?: WorkerScope;
  /** Creates an engine that owns durability for the requested storage. */
  durableEngineFactory?: WorkerEngineFactory;
}

/** A request's outcome: its result, or the error a response would carry. */
export type Served =
  | {ok: true; value: unknown}
  | {ok: false; error: ClientError};

export interface WorkerController {
  /**
   * Serves a request from code in this Worker, in order with every other
   * request, without a message or protocol validation. It settles with the
   * result, or rejects with the error a response message would have carried.
   */
  request(request: WorkerRequest): Promise<unknown>;
  /**
   * Serves a request as request() does, but at once, and returns its outcome.
   * `undefined` means it cannot be served before earlier requests, and has not
   * been.
   */
  requestNow(request: WorkerRequest): Served | undefined;
  close(): Promise<void>;
}

const TRANSACTION_ACTIVE = 'TRANSACTION_ACTIVE';
const ALREADY_ACTIVE = 'A TinyJoin transaction is already active';
const OPERATION_FAILED = 'WORKER_OPERATION_FAILED';

/**
 * Serves one dedicated Worker: it owns the engine, serializes every request
 * against it, and owns the single transaction token that the client's
 * transaction API is checked against.
 */
export const startWorker = (
  options: StartWorkerOptions = {},
): WorkerController => {
  const scope = options.scope ?? (globalThis as unknown as WorkerScope);
  let enginePromise: Promise<WorkerEngine> | undefined;
  let configuredStorage: StorageOptions | undefined;
  let closingPromise: Promise<void> | undefined;
  let closed = false;
  let pendingRevision = 0;
  const pendingTables = new Set<string>();
  const pendingKeys: PendingChangedKeys = new Map();
  let invalidationScheduled = false;
  let activeTransactionId: string | undefined;
  let nextTransactionId = 1;
  let requestTail: Promise<void> = Promise.resolve();
  let queuedRequests = 0;
  let openEngine: WorkerEngine | undefined;

  // A script's statements each report their own keys; union them under the same bound the
  // engine and the event merge use, so one overflowing statement does not silently truncate.
  const mergeScriptKeys = (results: {tables: string[]; keys: ApplyOutcome['keys']}[]) => {
    const merged: PendingChangedKeys = new Map();
    for (const result of results) {
      mergeChangedKeys(merged, result.tables, result.keys);
    }
    return finishChangedKeys(merged);
  };

  const respondWith = (message: WorkerResponse): void =>
    scope.postMessage(message);

  const emitInvalidation = (outcome: ApplyOutcome): void => {
    pendingRevision = mathMax(pendingRevision, outcome.revision);
    for (const table of outcome.tables) {
      pendingTables.add(table);
    }
    mergeChangedKeys(pendingKeys, outcome.tables, outcome.keys);
    if (invalidationScheduled || pendingTables.size === 0) {
      return;
    }
    invalidationScheduled = true;
    // A task boundary lets adjacent serialized mutations share one event and
    // ensures callers receive their RPC acknowledgement before invalidation.
    setTimeout(() => {
      invalidationScheduled = false;
      if (closed || pendingTables.size === 0) {
        return;
      }
      const event: WorkerEvent = {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {
          revision: pendingRevision,
          tables: [...pendingTables].sort(),
          keys: finishChangedKeys(pendingKeys),
        },
      };
      pendingTables.clear();
      pendingKeys.clear();
      scope.postMessage(event);
    }, 0);
  };

  // Publishes an outcome only outside a transaction: staged changes become
  // visible when the transaction commits, not as each statement runs. A result
  // still in text published nothing, so it has nothing to announce.
  const emitUnlessInTransaction = (
    outcome: ApplyOutcome | SqlResultText,
  ): void => {
    if (
      !isSqlResultText(outcome) &&
      isUndefined(activeTransactionId) &&
      outcome.tables.length > 0
    ) {
      emitInvalidation(outcome);
    }
  };

  const assertNoTransaction = (): void => {
    if (!isUndefined(activeTransactionId)) {
      throw workerError(TRANSACTION_ACTIVE, ALREADY_ACTIVE);
    }
  };

  const assertTransactionId = (requestedId: string | undefined): void => {
    if (isUndefined(activeTransactionId)) {
      if (!isUndefined(requestedId)) {
        throw workerError(
          'TRANSACTION_NOT_ACTIVE',
          'The TinyJoin transaction is no longer active',
        );
      }
      return;
    }
    if (requestedId !== activeTransactionId) {
      throw workerError(
        TRANSACTION_ACTIVE,
        'Use the active TinyJoin transaction for this operation',
      );
    }
  };

  const clearTransaction = (requestedId: string): void => {
    assertTransactionId(requestedId);
    activeTransactionId = undefined;
  };

  const handleRequest = (
    request: Exclude<WorkerRequest, {method: 'close'}>,
    engine: WorkerEngine,
  ): unknown => {
    switch (request.method) {
      case 'init':
        assertNoTransaction();
        return {revision: engine.revision()};

      case 'executeSql': {
        assertTransactionId(request.params.transactionId);
        const result = engine.executeSql(
          request.params.sql,
          request.params.params,
          request.params.rowMode,
        );
        emitUnlessInTransaction(result);
        return result;
      }

      case 'prepareSql':
        assertNoTransaction();
        return {statementId: engine.prepareSql(request.params.sql)};

      case 'executePrepared': {
        assertTransactionId(request.params.transactionId);
        const result = engine.executePrepared(
          request.params.statementId,
          request.params.params,
          request.params.rowMode,
        );
        emitUnlessInTransaction(result);
        return result;
      }

      case 'closePrepared':
        assertNoTransaction();
        engine.closePrepared(request.params.statementId);
        return undefined;

      case 'execSql': {
        assertTransactionId(request.params.transactionId);
        const results = engine.execSql(
          request.params.sql,
          request.params.rowMode,
        );
        emitUnlessInTransaction({
          revision: mathMax(...results.map((result) => result.revision), 0),
          tables: [...new Set(results.flatMap((result) => result.tables))],
          // Each statement in a script reports its own keys; the event merge unions them.
          keys: mergeScriptKeys(results),
        });
        return results;
      }

      case 'beginTransaction':
        assertNoTransaction();
        engine.beginTransaction();
        activeTransactionId = `tx-${nextTransactionId++}`;
        return {transactionId: activeTransactionId};

      case 'commitTransaction': {
        assertTransactionId(request.params.transactionId);
        try {
          const outcome = engine.commitTransaction();
          emitInvalidation(outcome);
          clearTransaction(request.params.transactionId);
          return outcome;
        } catch (error) {
          let cleanedUp = false;
          try {
            cleanedUp = !engine.inTransaction();
            if (!cleanedUp) {
              engine.rollbackTransaction();
              cleanedUp = true;
            }
          } catch {
            // Preserve the commit error. Keeping the token active lets the
            // client retry rollback if the engine can recover on a later call.
          }
          if (cleanedUp) {
            clearTransaction(request.params.transactionId);
          }
          throw error;
        }
      }

      case 'rollbackTransaction':
        assertTransactionId(request.params.transactionId);
        engine.rollbackTransaction();
        clearTransaction(request.params.transactionId);
        return undefined;
    }
  };

  const engineForRequest = (
    request: Exclude<WorkerRequest, {method: 'close'}>,
  ): Promise<WorkerEngine> => {
    if (request.method === 'init') {
      const storage = request.params.storage;
      if (
        !isUndefined(configuredStorage) &&
        !sameStorage(configuredStorage, storage)
      ) {
        throw workerError(
          'STORAGE_ALREADY_INITIALIZED',
          'The TinyJoin worker is already initialized with different storage',
        );
      }
      configuredStorage = storage;
      enginePromise ??= (options.durableEngineFactory ?? createDefaultEngine)(
        storage,
      ).then((engine) => (openEngine = engine));
      return enginePromise;
    }
    if (!enginePromise) {
      throw workerError(
        'WORKER_NOT_INITIALIZED',
        'Initialize the TinyJoin worker before sending other requests',
      );
    }
    return enginePromise;
  };

  const releaseResources = (engine?: WorkerEngine): Promise<void> => {
    closingPromise ??= (async () => {
      closed = true;
      scope.removeEventListener('message', onMessage);
      engine?.close();
    })();
    return closingPromise;
  };

  const settledEngine = async (): Promise<WorkerEngine | undefined> =>
    enginePromise ? await enginePromise : undefined;

  // Runs one request against the engine, which init opens and close releases.
  const serve = async (request: WorkerRequest): Promise<unknown> => {
    let engine: WorkerEngine | undefined;
    try {
      if (request.method === 'close') {
        await releaseResources(await settledEngine());
        return undefined;
      }
      engine = await engineForRequest(request);
      return handleRequest(request, engine);
    } catch (error) {
      if (request.method === 'init') {
        // The database never opened, so release whatever it had taken. Neither
        // the engine's own failure nor a cleanup failure may replace the error
        // that explains why ready failed.
        engine ??= await settledEngine().catch(() => undefined);
        try {
          await releaseResources(engine);
        } catch {
          // Preserve the initialization error.
        }
      }
      throw error;
    }
  };

  // Once the engine is open, a request with nothing ahead of it can be served
  // at once, which this reports.
  const canServeNow = (
    request: WorkerRequest,
  ): request is Exclude<WorkerRequest, {method: 'close'}> =>
    queuedRequests === 0 &&
    openEngine !== undefined &&
    request.method !== 'init' &&
    request.method !== 'close';

  // Serves a request at once, if it can be, passing the outcome to `settle`,
  // and reports whether it did.
  const serveNow = (
    request: WorkerRequest,
    settle: (ok: boolean, value: unknown) => void,
  ): boolean => {
    if (!canServeNow(request)) {
      return false;
    }
    let result: unknown;
    try {
      result = handleRequest(request, openEngine!);
    } catch (error) {
      settle(false, error);
      return true;
    }
    settle(true, result);
    return true;
  };

  // Serves requests one at a time, in the order they arrive, and passes each
  // outcome to `settle`, which must not throw.
  const schedule = (
    request: WorkerRequest,
    settle: (ok: boolean, value: unknown) => void,
  ): void => {
    if (serveNow(request, settle)) {
      return;
    }
    queuedRequests += 1;
    requestTail = requestTail
      .then(() => serve(request))
      .then(
        (result) => {
          queuedRequests -= 1;
          settle(true, result);
        },
        (error: unknown) => {
          queuedRequests -= 1;
          settle(false, error);
        },
      );
  };

  // Posts a request's response. A close, or an init that failed, then closes
  // the Worker.
  const respond =
    (request: WorkerRequest) =>
    (ok: boolean, value: unknown): void => {
      if (ok) {
        try {
          respondWith({
            v: PROTOCOL_VERSION,
            id: request.id,
            ok: true,
            result: value,
          });
        } catch (error) {
          ok = false;
          value = error;
        }
      }
      if (!ok) {
        respondWith({
          v: PROTOCOL_VERSION,
          id: request.id,
          ok: false,
          error: serializeError(value),
        });
      }
      if (request.method === 'close' || (request.method === 'init' && !ok)) {
        queueMicrotask(() => scope.close());
      }
    };

  const onMessage = (event: MessageEvent<unknown>): void => {
    if (!isWorkerRequest(event.data)) {
      respondWith({
        v: PROTOCOL_VERSION,
        id:
          isRecord(event.data) && isSafeInteger(event.data.id)
            ? event.data.id
            : 0,
        ok: false,
        error: {
          code: 'PROTOCOL_MISMATCH',
          message: 'The worker received an invalid TinyJoin protocol request',
        },
      });
      return;
    }
    schedule(event.data, respond(event.data));
  };

  scope.addEventListener('message', onMessage);
  return {
    request: (request: WorkerRequest): Promise<unknown> =>
      new Promise((resolve, reject) =>
        schedule(request, (ok, value) =>
          ok ? resolve(value) : reject(new ClientError(serializeError(value))),
        ),
      ),

    requestNow: (request: WorkerRequest): Served | undefined => {
      if (!canServeNow(request)) {
        return undefined;
      }
      try {
        return {ok: true, value: handleRequest(request, openEngine!)};
      } catch (error) {
        return {ok: false, error: new ClientError(serializeError(error))};
      }
    },

    close: async (): Promise<void> => {
      try {
        await releaseResources(await settledEngine());
      } finally {
        scope.close();
      }
    },
  };
};

const workerError = (code: string, message: string): Error =>
  Object.assign(new Error(message), {code});

const createDefaultEngine = (storage: StorageOptions): Promise<WorkerEngine> =>
  storage.kind === 'memory'
    ? createMemoryWasmEngine()
    : createOpfsWasmEngine(storage.name);

const sameStorage = (left: StorageOptions, right: StorageOptions): boolean =>
  left.kind === right.kind &&
  (left.kind === 'memory' ||
    (right.kind === 'opfs' && left.name === right.name));

const serializeError = (error: unknown): SerializedError =>
  asCodedError(error) ?? {
    code: OPERATION_FAILED,
    message: error instanceof Error ? error.message : String(error),
  };
