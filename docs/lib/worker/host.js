import { PROTOCOL_VERSION, isRecord, isWorkerRequest, } from '../protocol.js';
import { createMemoryWasmEngine, } from './engine.js';
import { createOpfsWasmEngine } from './opfs-loader.js';
export function startWorker(options = {}) {
    const scope = options.scope ?? globalThis;
    let enginePromise;
    let configuredStorage;
    let closingPromise;
    let closed = false;
    let pendingRevision = 0;
    const pendingTables = new Set();
    let invalidationScheduled = false;
    let activeTransactionId;
    let nextTransactionId = 1;
    let requestTail = Promise.resolve();
    const emitInvalidation = (outcome) => {
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
            const event = {
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
    const respond = async (request) => {
        let engine;
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
            const result = handleRequest(request, engine, emitInvalidation, {
                get activeId() {
                    return activeTransactionId;
                },
                begin() {
                    if (activeTransactionId !== undefined) {
                        throw workerError('TRANSACTION_ACTIVE', 'A TinyJoin transaction is already active');
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
        }
        catch (error) {
            if (request.method === 'init') {
                if (!engine && enginePromise) {
                    try {
                        engine = await enginePromise;
                    }
                    catch {
                        // Engine construction already closes partially opened resources.
                    }
                }
                try {
                    await releaseResources(engine);
                }
                catch {
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
    const engineForRequest = async (request) => {
        if (request.method === 'init') {
            if (configuredStorage !== undefined &&
                !sameStorage(configuredStorage, request.params.storage)) {
                throw Object.assign(new Error('The TinyJoin worker is already initialized with different storage'), { code: 'STORAGE_ALREADY_INITIALIZED' });
            }
            if (!enginePromise) {
                configuredStorage = request.params.storage;
                enginePromise = createEngine(request.params.storage, options.durableEngineFactory);
            }
            return enginePromise;
        }
        if (!enginePromise) {
            throw Object.assign(new Error('Initialize the TinyJoin worker before sending other requests'), { code: 'WORKER_NOT_INITIALIZED' });
        }
        return enginePromise;
    };
    const onMessage = (event) => {
        if (!isWorkerRequest(event.data)) {
            const id = isRecord(event.data) && Number.isSafeInteger(event.data.id)
                ? Number(event.data.id)
                : 0;
            scope.postMessage({
                v: PROTOCOL_VERSION,
                id,
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
    const releaseResources = (engine) => {
        closingPromise ??= (async () => {
            closed = true;
            scope.removeEventListener('message', onMessage);
            engine?.close();
        })();
        return closingPromise;
    };
    const close = async () => {
        try {
            const engine = enginePromise ? await enginePromise : undefined;
            await releaseResources(engine);
        }
        finally {
            scope.close();
        }
    };
    scope.addEventListener('message', onMessage);
    return { close };
}
function handleRequest(request, engine, emitInvalidation, transaction) {
    switch (request.method) {
        case 'init':
            assertNoTransaction(transaction.activeId);
            return { revision: engine.revision() };
        case 'executeSql': {
            assertTransactionId(transaction.activeId, request.params.transactionId);
            const result = engine.executeSql(request.params.sql, request.params.params);
            if (transaction.activeId === undefined && result.tables.length > 0) {
                emitInvalidation({ revision: result.revision, tables: result.tables });
            }
            return result;
        }
        case 'prepareSql':
            assertNoTransaction(transaction.activeId);
            return { statementId: engine.prepareSql(request.params.sql) };
        case 'executePrepared': {
            assertTransactionId(transaction.activeId, request.params.transactionId);
            const result = engine.executePrepared(request.params.statementId, request.params.params);
            if (transaction.activeId === undefined && result.tables.length > 0) {
                emitInvalidation({ revision: result.revision, tables: result.tables });
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
                return { transactionId: transaction.begin() };
            }
            catch (error) {
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
            }
            catch (error) {
                let cleanedUp = false;
                try {
                    cleanedUp = !engine.inTransaction();
                    if (!cleanedUp) {
                        engine.rollbackTransaction();
                        cleanedUp = true;
                    }
                }
                catch {
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
    }
}
function assertNoTransaction(activeId) {
    if (activeId !== undefined) {
        throw workerError('TRANSACTION_ACTIVE', 'A TinyJoin transaction is already active');
    }
}
function assertTransactionId(activeId, requestedId) {
    if (activeId === undefined && requestedId === undefined) {
        return;
    }
    if (activeId === undefined) {
        throw workerError('TRANSACTION_NOT_ACTIVE', 'The TinyJoin transaction is no longer active');
    }
    if (requestedId !== activeId) {
        throw workerError('TRANSACTION_ACTIVE', 'Use the active TinyJoin transaction for this operation');
    }
}
function workerError(code, message) {
    return Object.assign(new Error(message), { code });
}
async function createEngine(storage, durableEngineFactory) {
    return (durableEngineFactory ?? createDefaultEngine)(storage);
}
async function createDefaultEngine(storage) {
    if (storage.kind === 'memory') {
        return createMemoryWasmEngine();
    }
    return createOpfsWasmEngine(storage.name);
}
function sameStorage(left, right) {
    return (left.kind === right.kind &&
        (left.kind === 'memory' ||
            (right.kind === 'opfs' && left.name === right.name)));
}
function serializeError(error) {
    if (isRecord(error) &&
        typeof error.code === 'string' &&
        typeof error.message === 'string') {
        return {
            code: error.code,
            message: error.message,
            ...(typeof error.retryable === 'boolean'
                ? { retryable: error.retryable }
                : {}),
        };
    }
    if (error instanceof Error) {
        return { code: 'WORKER_OPERATION_FAILED', message: error.message };
    }
    return { code: 'WORKER_OPERATION_FAILED', message: String(error) };
}
