import type {ReplicaSource, ReplicaSourceContext} from '../adapters/types.js';
import {
  PROTOCOL_VERSION,
  isRecord,
  isWorkerRequest,
  type ApplyOutcome,
  type SerializedTinygresError,
  type SyncState,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../protocol.js';
import {createWasmEngine, type WorkerEngine} from './engine.js';

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

export interface TinygresWorkerOptions {
  source?: ReplicaSource;
  scope?: WorkerScope;
  engineFactory?: () => Promise<WorkerEngine>;
}

export interface TinygresWorkerController {
  close(): Promise<void>;
}

export function startTinygresWorker(
  options: TinygresWorkerOptions = {},
): TinygresWorkerController {
  const scope = options.scope ?? (globalThis as unknown as WorkerScope);
  const enginePromise = (options.engineFactory ?? createWasmEngine)();
  const sourceAbortController = new AbortController();
  let sourceStarted = false;
  let closed = false;
  let pendingRevision = 0;
  const pendingTables = new Set<string>();
  let invalidationScheduled = false;

  const emitInvalidation = (outcome: ApplyOutcome): void => {
    pendingRevision = Math.max(pendingRevision, outcome.revision);
    for (const table of outcome.tables) {
      pendingTables.add(table);
    }
    if (invalidationScheduled || pendingTables.size === 0) {
      return;
    }
    invalidationScheduled = true;
    queueMicrotask(() => {
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
    });
  };

  const setSyncState = (state: SyncState): void => {
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
      engine.defineTable(schema);
      const outcome = engine.replaceTable(schema.name, rows);
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
    if (!options.source || sourceStarted) {
      return;
    }
    sourceStarted = true;
    setSyncState({phase: 'connecting', sourceId: options.source.id});
    try {
      await options.source.start(sourceContext(engine));
    } catch (error) {
      if (!sourceAbortController.signal.aborted) {
        setSyncState({
          phase: 'error',
          sourceId: options.source.id,
          error: serializeError(error),
        });
      }
    }
  };

  const respond = async (request: WorkerRequest): Promise<void> => {
    try {
      const engine = await enginePromise;
      const result = await handleRequest(request, engine, emitInvalidation);
      scope.postMessage({
        v: PROTOCOL_VERSION,
        id: request.id,
        ok: true,
        result,
      });
      if (request.method === 'init') {
        void startSource(engine);
      } else if (request.method === 'close') {
        queueMicrotask(() => void close());
      }
    } catch (error) {
      scope.postMessage({
        v: PROTOCOL_VERSION,
        id: request.id,
        ok: false,
        error: serializeError(error),
      });
    }
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
          message: 'The worker received an invalid Tinygres protocol request',
        },
      });
      return;
    }
    void respond(event.data);
  };

  const close = async (): Promise<void> => {
    if (closed) {
      return;
    }
    closed = true;
    scope.removeEventListener('message', onMessage);
    sourceAbortController.abort();
    await options.source?.close?.();
    const engine = await enginePromise;
    engine.close?.();
    scope.close();
  };

  scope.addEventListener('message', onMessage);
  return {close};
}

async function handleRequest(
  request: WorkerRequest,
  engine: WorkerEngine,
  emitInvalidation: (outcome: ApplyOutcome) => void,
): Promise<unknown> {
  switch (request.method) {
    case 'init':
      for (const schema of request.params.schemas) {
        engine.defineTable(schema);
      }
      return {revision: engine.revision()};
    case 'defineTable':
      engine.defineTable(request.params.schema);
      return undefined;
    case 'replaceTable': {
      const outcome = engine.replaceTable(
        request.params.table,
        request.params.rows,
      );
      emitInvalidation(outcome);
      return outcome;
    }
    case 'applyBatch': {
      const outcome = engine.applyBatch(request.params.batch);
      emitInvalidation(outcome);
      return outcome;
    }
    case 'query':
      return engine.query(request.params.plan);
    case 'querySql':
      return engine.querySql(request.params.sql, request.params.params);
    case 'close':
      return undefined;
  }
}

function serializeError(error: unknown): SerializedTinygresError {
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
