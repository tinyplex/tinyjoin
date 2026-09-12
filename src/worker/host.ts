import {
  asCodedError,
  isRecord,
  isSafeInteger,
  isUndefined,
  mathMax,
} from '../common.js';
import {
  PROTOCOL_VERSION,
  isWorkerRequest,
  type ApplyOutcome,
  type SerializedError,
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

export interface WorkerController {
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
  let invalidationScheduled = false;
  let activeTransactionId: string | undefined;
  let nextTransactionId = 1;
  let requestTail: Promise<void> = Promise.resolve();

  const respondWith = (message: WorkerResponse): void =>
    scope.postMessage(message);

  const emitInvalidation = (outcome: ApplyOutcome): void => {
    pendingRevision = mathMax(pendingRevision, outcome.revision);
    for (const table of outcome.tables) {
      pendingTables.add(table);
    }
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
        },
      };
      pendingTables.clear();
      scope.postMessage(event);
    }, 0);
  };

  // Publishes an outcome only outside a transaction: staged changes become
  // visible when the transaction commits, not as each statement runs.
  const emitUnlessInTransaction = (outcome: ApplyOutcome): void => {
    if (isUndefined(activeTransactionId) && outcome.tables.length > 0) {
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
        const results = engine.execSql(request.params.sql);
        emitUnlessInTransaction({
          revision: mathMax(...results.map((result) => result.revision), 0),
          tables: [...new Set(results.flatMap((result) => result.tables))],
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
      );
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

  const respond = async (request: WorkerRequest): Promise<void> => {
    let engine: WorkerEngine | undefined;
    try {
      if (request.method === 'close') {
        await releaseResources(await settledEngine());
        respondWith({
          v: PROTOCOL_VERSION,
          id: request.id,
          ok: true,
          result: undefined,
        });
        queueMicrotask(() => scope.close());
        return;
      }
      engine = await engineForRequest(request);
      respondWith({
        v: PROTOCOL_VERSION,
        id: request.id,
        ok: true,
        result: handleRequest(request, engine),
      });
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
      respondWith({
        v: PROTOCOL_VERSION,
        id: request.id,
        ok: false,
        error: serializeError(error),
      });
      if (request.method === 'close' || request.method === 'init') {
        queueMicrotask(() => scope.close());
      }
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
    const request = event.data;
    requestTail = requestTail.then(() => respond(request));
  };

  scope.addEventListener('message', onMessage);
  return {
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
