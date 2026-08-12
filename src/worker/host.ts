import type {ReplicaSource, ReplicaSourceContext} from '../adapters/types.js';
import {
  PROTOCOL_VERSION,
  isRecord,
  isWorkerRequest,
  type ApplyOutcome,
  type SerializedError,
  type StorageOptions,
  type SyncState,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../protocol.js';
import {createWasmEngine, type WorkerEngine} from './engine.js';
import {
  bindOpfsStorageName,
  builtinSourceConfigurationKey,
  loadBuiltinSource,
  mergeSourceSchemas,
  prepareBuiltinSource,
  type BuiltinSourceFactory,
  type PreparedBuiltinSource,
  type SourceIdentityHasher,
} from './builtin-source.js';
import {createPersistentEngine} from './persistent-engine.js';
import {createOpfsSnapshotStore} from './snapshot-store.js';

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
  source?: ReplicaSource;
  scope?: WorkerScope;
  engineFactory?: () => Promise<WorkerEngine>;
  builtinSourceFactory?: BuiltinSourceFactory;
  sourceIdentityHasher?: SourceIdentityHasher;
}

export interface WorkerController {
  close(): Promise<void>;
}

export function startWorker(
  options: StartWorkerOptions = {},
): WorkerController {
  const scope = options.scope ?? (globalThis as unknown as WorkerScope);
  const engineFactory = options.engineFactory ?? createWasmEngine;
  const builtinSourceFactory =
    options.builtinSourceFactory ?? loadBuiltinSource;
  const sourceIdentityHasher = options.sourceIdentityHasher;
  const sourceAbortController = new AbortController();
  let enginePromise: Promise<WorkerEngine> | undefined;
  let configuredStorage: StorageOptions | undefined;
  let configuredSource: PreparedBuiltinSource | undefined;
  let closingPromise: Promise<void> | undefined;
  let sourceCreationPromise: Promise<ReplicaSource> | undefined = options.source
    ? Promise.resolve(options.source)
    : undefined;
  let sourceRunPromise: Promise<void> | undefined;
  let sourceStarted = false;
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

  const setSyncState = (state: SyncState): void => {
    if (closed) {
      return;
    }
    scope.postMessage({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: state,
    });
  };

  const sourceContext = (engine: WorkerEngine): ReplicaSourceContext => ({
    signal: sourceAbortController.signal,
    async defineTable(schema) {
      engine.defineTable(schema);
    },
    async replaceTable(schema, rows) {
      const outcome = engine.replaceTableSnapshot(schema, rows);
      emitInvalidation(outcome);
      return outcome;
    },
    async applyBatch(batch) {
      const outcome = engine.applyBatch(batch);
      emitInvalidation(outcome);
      return outcome;
    },
    setSyncState,
  });

  const startSource = async (engine: WorkerEngine): Promise<void> => {
    const sourceConfigured =
      options.source !== undefined || configuredSource !== undefined;
    if (!sourceConfigured || sourceStarted) {
      return;
    }
    const sourceId =
      options.source?.id || configuredSource?.options.id || 'custom-source';
    sourceStarted = true;
    setSyncState({phase: 'connecting', sourceId});
    sourceCreationPromise ??= Promise.resolve().then(() =>
      builtinSourceFactory(configuredSource!.options),
    );
    sourceRunPromise = (async () => {
      try {
        const source = await sourceCreationPromise;
        if (sourceAbortController.signal.aborted) {
          return;
        }
        await source.start(sourceContext(engine));
      } catch (error) {
        if (!sourceAbortController.signal.aborted) {
          setSyncState({
            phase: 'error',
            sourceId,
            error: serializeError(error),
          });
        }
      }
    })();
    await sourceRunPromise;
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
      const result = await handleRequest(
        request,
        engine,
        emitInvalidation,
        configuredSource,
        options.source !== undefined || configuredSource !== undefined,
        {
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
        },
      );
      scope.postMessage({
        v: PROTOCOL_VERSION,
        id: request.id,
        ok: true,
        result,
      });
      if (request.method === 'init') {
        void startSource(engine);
      }
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
      const requestedSource = request.params.source
        ? prepareBuiltinSource(request.params.source)
        : undefined;
      if (options.source && requestedSource) {
        throw Object.assign(
          new Error(
            'The TinyGres worker cannot combine a fixed custom source with a built-in source configuration',
          ),
          {code: 'SOURCE_CONFLICT'},
        );
      }
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
      if (
        configuredStorage !== undefined &&
        builtinSourceConfigurationKey(configuredSource) !==
          builtinSourceConfigurationKey(requestedSource)
      ) {
        throw Object.assign(
          new Error(
            'The TinyGres worker is already initialized with a different built-in source',
          ),
          {code: 'SOURCE_ALREADY_INITIALIZED'},
        );
      }
      if (!enginePromise) {
        configuredStorage = request.params.storage;
        configuredSource = requestedSource;
        enginePromise = createSourceBoundEngine(
          request.params.storage,
          requestedSource,
          engineFactory,
          sourceIdentityHasher,
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
      sourceAbortController.abort();
      let firstError: unknown;
      let source: ReplicaSource | undefined;
      try {
        source = await sourceCreationPromise;
      } catch {
        // Source construction failures are already exposed as sync errors.
      }
      try {
        await source?.close?.();
      } catch (error) {
        firstError = error;
      }
      await sourceRunPromise?.catch(() => undefined);
      try {
        engine?.close?.();
      } catch (error) {
        firstError ??= error;
      }
      if (firstError !== undefined) {
        throw firstError;
      }
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
  source: PreparedBuiltinSource | undefined,
  sourceConfigured: boolean,
  transaction: HostTransactionState,
): Promise<unknown> {
  switch (request.method) {
    case 'init':
      assertNoTransaction(transaction.activeId);
      engine.defineTables(
        mergeSourceSchemas(request.params.schemas, source?.schemas ?? []),
      );
      return {revision: engine.revision(), sourceConfigured};
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
      assertTransactionId(
        transaction.activeId,
        request.params.transactionId,
      );
      return engine.query(request.params.plan);
    case 'querySql':
      assertTransactionId(
        transaction.activeId,
        request.params.transactionId,
      );
      return engine.querySql(request.params.sql, request.params.params);
    case 'executeSql': {
      assertLocalWritesAllowed(sourceConfigured);
      assertTransactionId(
        transaction.activeId,
        request.params.transactionId,
      );
      const result = engine.executeSql(
        request.params.sql,
        request.params.params,
      );
      if (transaction.activeId === undefined && result.tables.length > 0) {
        emitInvalidation({revision: result.revision, tables: result.tables});
      }
      return result;
    }
    case 'beginTransaction': {
      assertLocalWritesAllowed(sourceConfigured);
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
      assertTransactionId(
        transaction.activeId,
        request.params.transactionId,
      );
      try {
        const outcome = engine.commitTransaction();
        emitInvalidation(outcome);
        return outcome;
      } catch (error) {
        if (engine.inTransaction()) {
          engine.rollbackTransaction();
        }
        throw error;
      } finally {
        transaction.clear(request.params.transactionId);
      }
    }
    case 'rollbackTransaction':
      assertTransactionId(
        transaction.activeId,
        request.params.transactionId,
      );
      try {
        engine.rollbackTransaction();
        return undefined;
      } finally {
        transaction.clear(request.params.transactionId);
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

function assertLocalWritesAllowed(sourceConfigured: boolean): void {
  if (sourceConfigured) {
    throw workerError(
      'SOURCE_DATABASE_READ_ONLY',
      'Local SQL writes are disabled while a TinyGres replication source is configured',
    );
  }
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

async function createSourceBoundEngine(
  storage: StorageOptions,
  source: PreparedBuiltinSource | undefined,
  engineFactory: () => Promise<WorkerEngine>,
  sourceIdentityHasher: SourceIdentityHasher | undefined,
): Promise<WorkerEngine> {
  if (storage.kind !== 'opfs' || !source) {
    return createConfiguredEngine(storage, engineFactory);
  }
  const name = await bindOpfsStorageName(
    storage.name,
    source,
    sourceIdentityHasher,
  );
  return createConfiguredEngine({kind: 'opfs', name}, engineFactory);
}

async function createConfiguredEngine(
  storage: StorageOptions,
  engineFactory: () => Promise<WorkerEngine>,
): Promise<WorkerEngine> {
  const engine = await engineFactory();
  if (storage.kind === 'memory') {
    return engine;
  }

  let store: Awaited<ReturnType<typeof createOpfsSnapshotStore>> | undefined;
  try {
    store = await createOpfsSnapshotStore(storage.name);
    return createPersistentEngine(engine, store);
  } catch (error) {
    try {
      store?.close();
    } finally {
      engine.close?.();
    }
    throw error;
  }
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
