import {
  PROTOCOL_VERSION,
  isRecord,
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

export function startWorker(
  options: StartWorkerOptions = {},
): WorkerController {
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

  const emitInvalidation = (outcome: ApplyOutcome): void => {
    pendingRevision = Math.max(pendingRevision, outcome.revision);
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

  const respond = async (request: WorkerRequest): Promise<void> => {
    let engine: WorkerEngine | undefined;
    try {
      if (request.method === 'close') {
        engine = enginePromise ? await enginePromise : undefined;
        await releaseResources(engine);
        scope.postMessage({
          v: PROTOCOL_VERSION,
          id: request.id,
          ok: true,
          result: undefined,
        });
        queueMicrotask(() => scope.close());
        return;
      }

      engine = await engineForRequest(request);
      const result = await handleRequest(request, engine, emitInvalidation, {
        get activeId() {
          return activeTransactionId;
        },
        begin() {
          if (activeTransactionId !== undefined) {
            throw workerError(
              'TRANSACTION_ACTIVE',
              'A TinyGres transaction is already active',
            );
          }
          const id = `tx-${nextTransactionId++}`;
          activeTransactionId = id;
          return id;
        },
        clear(id) {
          assertTransactionId(activeTransactionId, id);
          activeTransactionId = undefined;
        },
      });
      scope.postMessage({
        v: PROTOCOL_VERSION,
        id: request.id,
        ok: true,
        result,
      });
    } catch (error) {
      if (request.method === 'init') {
        if (!engine && enginePromise) {
          try {
            engine = await enginePromise;
          } catch {
            // Engine construction already closes partially opened resources.
          }
        }
        try {
          await releaseResources(engine);
        } catch {
          // Preserve the initialization error that explains why ready failed.
        }
      }
      scope.postMessage({
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

  const engineForRequest = async (
    request: Exclude<WorkerRequest, {method: 'close'}>,
  ): Promise<WorkerEngine> => {
    if (request.method === 'init') {
      if (
        configuredStorage !== undefined &&
        !sameStorage(configuredStorage, request.params.storage)
      ) {
        throw Object.assign(
          new Error(
            'The TinyGres worker is already initialized with different storage',
          ),
          {code: 'STORAGE_ALREADY_INITIALIZED'},
        );
      }
      if (!enginePromise) {
        configuredStorage = request.params.storage;
        enginePromise = createEngine(
          request.params.storage,
          options.durableEngineFactory,
        );
      }
      return enginePromise;
    }
    if (!enginePromise) {
      throw Object.assign(
        new Error(
          'Initialize the TinyGres worker before sending other requests',
        ),
        {code: 'WORKER_NOT_INITIALIZED'},
      );
    }
    return enginePromise;
  };

  const onMessage = (event: MessageEvent<unknown>): void => {
    if (!isWorkerRequest(event.data)) {
      const id =
        isRecord(event.data) && Number.isSafeInteger(event.data.id)
          ? Number(event.data.id)
          : 0;
      scope.postMessage({
        v: PROTOCOL_VERSION,
        id,
        ok: false,
        error: {
          code: 'PROTOCOL_MISMATCH',
          message: 'The worker received an invalid TinyGres protocol request',
        },
      });
      return;
    }
    const request = event.data;
    requestTail = requestTail.then(() => respond(request));
  };

  const releaseResources = (
    engine: WorkerEngine | undefined,
  ): Promise<void> => {
    closingPromise ??= (async () => {
      closed = true;
      scope.removeEventListener('message', onMessage);
      engine?.close();
    })();
    return closingPromise;
  };

  const close = async (): Promise<void> => {
    try {
      const engine = enginePromise ? await enginePromise : undefined;
      await releaseResources(engine);
    } finally {
      scope.close();
    }
  };

  scope.addEventListener('message', onMessage);
  return {close};
}

async function handleRequest(
  request: WorkerRequest,
  engine: WorkerEngine,
  emitInvalidation: (outcome: ApplyOutcome) => void,
  transaction: HostTransactionState,
): Promise<unknown> {
  switch (request.method) {
    case 'init':
      assertNoTransaction(transaction.activeId);
      engine.defineTables(request.params.schemas);
      return {revision: engine.revision()};
    case 'defineTable':
      assertNoTransaction(transaction.activeId);
      engine.defineTable(request.params.schema);
      return undefined;
    case 'replaceTable': {
      assertNoTransaction(transaction.activeId);
      const outcome = engine.replaceTableSnapshot(
        request.params.schema,
        request.params.rows,
      );
      emitInvalidation(outcome);
      return outcome;
    }
    case 'applyBatch': {
      assertNoTransaction(transaction.activeId);
      const outcome = engine.applyBatch(request.params.batch);
      emitInvalidation(outcome);
      return outcome;
    }
    case 'query':
      assertTransactionId(transaction.activeId, request.params.transactionId);
      return engine.query(request.params.plan);
    case 'executeSql': {
      assertTransactionId(transaction.activeId, request.params.transactionId);
      const result = engine.executeSql(
        request.params.sql,
        request.params.params,
      );
      if (transaction.activeId === undefined && result.tables.length > 0) {
        emitInvalidation({revision: result.revision, tables: result.tables});
      }
      return result;
    }
    case 'prepareSql':
      assertNoTransaction(transaction.activeId);
      return {statementId: engine.prepareSql(request.params.sql)};
    case 'executePrepared': {
      assertTransactionId(transaction.activeId, request.params.transactionId);
      const result = engine.executePrepared(
        request.params.statementId,
        request.params.params,
      );
      if (transaction.activeId === undefined && result.tables.length > 0) {
        emitInvalidation({revision: result.revision, tables: result.tables});
      }
      return result;
    }
    case 'closePrepared':
      assertNoTransaction(transaction.activeId);
      engine.closePrepared(request.params.statementId);
      return undefined;
    case 'execSql': {
      assertTransactionId(transaction.activeId, request.params.transactionId);
      const results = engine.execSql(request.params.sql);
      if (transaction.activeId === undefined) {
        const tables = [...new Set(results.flatMap((result) => result.tables))];
        if (tables.length > 0) {
          emitInvalidation({
            revision: Math.max(...results.map((result) => result.revision)),
            tables,
          });
        }
      }
      return results;
    }
    case 'beginTransaction': {
      assertNoTransaction(transaction.activeId);
      engine.beginTransaction();
      try {
        return {transactionId: transaction.begin()};
      } catch (error) {
        engine.rollbackTransaction();
        throw error;
      }
    }
    case 'commitTransaction': {
      assertTransactionId(transaction.activeId, request.params.transactionId);
      try {
        const outcome = engine.commitTransaction();
        emitInvalidation(outcome);
        transaction.clear(request.params.transactionId);
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
          transaction.clear(request.params.transactionId);
        }
        throw error;
      }
    }
    case 'rollbackTransaction': {
      assertTransactionId(transaction.activeId, request.params.transactionId);
      engine.rollbackTransaction();
      transaction.clear(request.params.transactionId);
      return undefined;
    }
    case 'close':
      throw new Error('Close requests are handled before engine dispatch');
  }
}

interface HostTransactionState {
  readonly activeId: string | undefined;
  begin(): string;
  clear(id: string): void;
}

function assertNoTransaction(activeId: string | undefined): void {
  if (activeId !== undefined) {
    throw workerError(
      'TRANSACTION_ACTIVE',
      'A TinyGres transaction is already active',
    );
  }
}

function assertTransactionId(
  activeId: string | undefined,
  requestedId: string | undefined,
): void {
  if (activeId === undefined && requestedId === undefined) {
    return;
  }
  if (activeId === undefined) {
    throw workerError(
      'TRANSACTION_NOT_ACTIVE',
      'The TinyGres transaction is no longer active',
    );
  }
  if (requestedId !== activeId) {
    throw workerError(
      'TRANSACTION_ACTIVE',
      'Use the active TinyGres transaction for this operation',
    );
  }
}

function workerError(code: string, message: string): Error {
  return Object.assign(new Error(message), {code});
}

async function createEngine(
  storage: StorageOptions,
  durableEngineFactory: WorkerEngineFactory | undefined,
): Promise<WorkerEngine> {
  return (durableEngineFactory ?? createDefaultEngine)(storage);
}

async function createDefaultEngine(
  storage: StorageOptions,
): Promise<WorkerEngine> {
  if (storage.kind === 'memory') {
    return createMemoryWasmEngine();
  }
  return createOpfsWasmEngine(storage.name);
}

function sameStorage(left: StorageOptions, right: StorageOptions): boolean {
  return (
    left.kind === right.kind &&
    (left.kind === 'memory' ||
      (right.kind === 'opfs' && left.name === right.name))
  );
}

function serializeError(error: unknown): SerializedError {
  if (
    isRecord(error) &&
    typeof error.code === 'string' &&
    typeof error.message === 'string'
  ) {
    return {
      code: error.code,
      message: error.message,
      ...(typeof error.retryable === 'boolean'
        ? {retryable: error.retryable}
        : {}),
    };
  }
  if (error instanceof Error) {
    return {code: 'WORKER_OPERATION_FAILED', message: error.message};
  }
  return {code: 'WORKER_OPERATION_FAILED', message: String(error)};
}
