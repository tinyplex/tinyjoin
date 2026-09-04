import { isRecord, } from '../protocol.js';
import { ClientError } from './error.js';
import { WorkerRpc } from './rpc.js';
const preparedStatementStates = new WeakMap();
class ClientPreparedStatement {
    constructor(state) {
        preparedStatementStates.set(this, state);
    }
    execute(params = [], options) {
        try {
            const state = preparedStatementState(this);
            state.assertClientOpen();
            assertPreparedStatementOpen(state);
            state.assertDirectOperationAllowed();
            assertQueryOptions(options);
            return trackPreparedExecution(state, state.executeDirect(params, options));
        }
        catch (error) {
            return Promise.reject(error);
        }
    }
    close() {
        let state;
        try {
            state = preparedStatementState(this);
        }
        catch (error) {
            return Promise.reject(error);
        }
        if (state.closed) {
            return state.closePromise ?? Promise.resolve();
        }
        try {
            state.assertDirectOperationAllowed();
        }
        catch (error) {
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
    get closed() {
        return preparedStatementState(this).closed;
    }
}
export class Client {
    #rpc;
    #preparedOwner = {};
    #preparedStatements = new Set();
    waitReady;
    #subscriptions = new Set();
    #revision = 0;
    #ready = false;
    #closing = false;
    #closed = false;
    #closePromise;
    #preparedCloseGeneration = 0;
    #preparedCloseTail = Promise.resolve();
    #transactionTail = Promise.resolve();
    #transactionActive = false;
    constructor(options = {}) {
        assertClientOptions(options);
        const storage = storageFromDataDir(options.dataDir);
        const worker = createWorker(options);
        this.#rpc = new WorkerRpc(worker.worker, worker.resultValidation);
        this.#rpc.onEvent((event) => {
            if (event.event === 'tablesChanged') {
                this.#revision = Math.max(this.#revision, event.payload.revision);
                for (const subscription of this.#subscriptions) {
                    if (!subscription.tables ||
                        event.payload.tables.some((table) => subscription.tables?.has(table))) {
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
            this.#revision = result.revision;
            this.#ready = true;
        });
    }
    get ready() {
        return this.#ready && !this.#closing && !this.#closed;
    }
    get closed() {
        return this.#closed;
    }
    async query(sql, params = [], options) {
        await this.waitReady;
        this.#assertNoActiveTransaction();
        assertQueryOptions(options);
        const result = await this.#rpc.request('executeSql', { sql, params });
        this.#revision = Math.max(this.#revision, result.revision);
        return toResults(result, options);
    }
    sql(strings, ...params) {
        return this.query(parameterize(strings, params), params);
    }
    async prepare(sql) {
        await this.waitReady;
        this.#assertNoActiveTransaction();
        const { statementId } = await this.#rpc.request('prepareSql', { sql });
        this.#assertOpen();
        let state;
        state = {
            owner: this.#preparedOwner,
            statementId,
            inFlight: new Set(),
            assertDirectOperationAllowed: () => this.#assertNoActiveTransaction(),
            assertClientOpen: () => this.#assertOpen(),
            executeDirect: (params, options) => this.#executePrepared(statementId, params, options),
            closeRemote: () => this.#closePrepared(statementId),
            trackClose: (close) => this.#trackPreparedClose(close),
            unregister: () => this.#preparedStatements.delete(state),
            closed: false,
            clientClosed: false,
        };
        this.#preparedStatements.add(state);
        return new ClientPreparedStatement(state);
    }
    /** Executes one or more SQL statements without parameters. */
    async exec(sql, options) {
        await this.waitReady;
        this.#assertNoActiveTransaction();
        assertQueryOptions(options);
        const results = await this.#rpc.request('execSql', { sql });
        this.#noteResults(results);
        return results.map((result) => toResults(result, options));
    }
    /**
     * Runs SQL against an isolated staged database and durably publishes all
     * changes together when the callback succeeds, unless it explicitly rolls
     * back.
     */
    transaction(callback) {
        if (typeof callback !== 'function') {
            throw new TypeError('TinyJoin transaction requires a callback');
        }
        const run = this.#transactionTail.then(() => this.#runTransaction(callback));
        this.#transactionTail = run.then(() => undefined, () => undefined);
        return run;
    }
    subscribe(options, listener) {
        const subscription = {
            ...(options.tables ? { tables: new Set(options.tables) } : {}),
            listener,
        };
        this.#subscriptions.add(subscription);
        return () => this.#subscriptions.delete(subscription);
    }
    getRevision() {
        return this.#revision;
    }
    close() {
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
    async #closeOnce() {
        try {
            await this.waitReady;
            await this.#rpc.request('close', undefined);
        }
        finally {
            this.#ready = false;
            this.#closing = false;
            this.#closed = true;
            this.#subscriptions.clear();
            this.#rpc.dispose();
        }
    }
    async #runTransaction(callback) {
        await this.waitReady;
        const { transactionId } = await this.#beginTransactionAfterPreparedCloses();
        const transaction = new ClientTransaction(this.#rpc, transactionId, this.#preparedOwner, (revision) => {
            this.#revision = Math.max(this.#revision, revision);
        });
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
        }
        catch (error) {
            transaction.seal();
            if (shouldRollback && !transaction.rollbackCompleted) {
                await this.#rpc
                    .request('rollbackTransaction', { transactionId })
                    .catch(() => undefined);
            }
            throw error;
        }
        finally {
            this.#transactionActive = false;
        }
    }
    async #beginTransactionAfterPreparedCloses() {
        while (true) {
            this.#assertOpen();
            const generation = this.#preparedCloseGeneration;
            const tail = this.#preparedCloseTail;
            await tail;
            if (generation !== this.#preparedCloseGeneration ||
                tail !== this.#preparedCloseTail) {
                continue;
            }
            // Reserve the client before dispatching BEGIN. The generation check and
            // reservation are synchronous, so a new prepared close cannot slip
            // between the drained barrier and the Worker request.
            this.#assertOpen();
            this.#transactionActive = true;
            try {
                return await this.#rpc.request('beginTransaction', undefined);
            }
            catch (error) {
                this.#transactionActive = false;
                throw error;
            }
        }
    }
    #trackPreparedClose(close) {
        const previous = this.#preparedCloseTail;
        this.#preparedCloseGeneration += 1;
        this.#preparedCloseTail = Promise.allSettled([previous, close]).then(() => undefined);
    }
    #assertNoActiveTransaction() {
        this.#assertOpen();
        if (this.#transactionActive) {
            throw clientError('TRANSACTION_ACTIVE', 'Use the transaction object while a TinyJoin transaction is active');
        }
    }
    #assertOpen() {
        if (this.#closing || this.#closed) {
            throw clientError('CLIENT_CLOSED', 'The TinyJoin client is closed');
        }
    }
    async #executePrepared(statementId, params, options) {
        await this.waitReady;
        this.#assertNoActiveTransaction();
        const result = await this.#rpc.request('executePrepared', {
            statementId,
            params,
        });
        this.#revision = Math.max(this.#revision, result.revision);
        return toResults(result, options);
    }
    async #closePrepared(statementId) {
        if (this.#closing || this.#closed) {
            return;
        }
        await this.#rpc.request('closePrepared', { statementId });
    }
    #noteResults(results) {
        for (const result of results) {
            this.#revision = Math.max(this.#revision, result.revision);
        }
    }
}
class ClientTransaction {
    rpc;
    transactionId;
    preparedOwner;
    noteRevision;
    #pending = new Set();
    #open = true;
    #closing = false;
    #rollbackCompleted = false;
    #rollbackPromise;
    constructor(rpc, transactionId, preparedOwner, noteRevision) {
        this.rpc = rpc;
        this.transactionId = transactionId;
        this.preparedOwner = preparedOwner;
        this.noteRevision = noteRevision;
    }
    query(sql, params = [], options) {
        this.#assertOpen();
        assertQueryOptions(options);
        return this.#track(this.rpc
            .request('executeSql', {
            sql,
            params,
            transactionId: this.transactionId,
        })
            .then((result) => {
            this.noteRevision(result.revision);
            return toResults(result, options);
        }));
    }
    sql(strings, ...params) {
        return this.query(parameterize(strings, params), params);
    }
    exec(sql, options) {
        this.#assertOpen();
        assertQueryOptions(options);
        return this.#track(this.rpc
            .request('execSql', {
            sql,
            transactionId: this.transactionId,
        })
            .then((results) => {
            for (const result of results) {
                this.noteRevision(result.revision);
            }
            return results.map((result) => toResults(result, options));
        }));
    }
    execute(statement, params = [], options) {
        this.#assertOpen();
        const state = preparedStatementState(statement);
        if (state.owner !== this.preparedOwner) {
            throw clientError('PREPARED_STATEMENT_CLIENT_MISMATCH', 'The prepared statement belongs to a different TinyJoin client');
        }
        state.assertClientOpen();
        assertPreparedStatementOpen(state);
        assertQueryOptions(options);
        const operation = this.#track(this.rpc
            .request('executePrepared', {
            statementId: state.statementId,
            params,
            transactionId: this.transactionId,
        })
            .then((result) => {
            this.noteRevision(result.revision);
            return toResults(result, options);
        }));
        return trackPreparedExecution(state, operation);
    }
    seal() {
        this.#open = false;
    }
    get closed() {
        return !this.#open;
    }
    get rollbackCompleted() {
        return this.#rollbackCompleted;
    }
    rollback() {
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
        void rollback.then(() => this.#pending.delete(rollback), () => this.#pending.delete(rollback));
        return rollback;
    }
    async settle() {
        while (this.#pending.size > 0) {
            await Promise.all([...this.#pending]);
        }
        await this.#rollbackPromise;
    }
    #track(promise) {
        this.#assertOpen();
        this.#pending.add(promise);
        void promise.then(() => this.#pending.delete(promise), () => this.#pending.delete(promise));
        return promise;
    }
    #assertOpen() {
        if (!this.#open || this.#closing) {
            throw clientError('TRANSACTION_CLOSED', 'The TinyJoin transaction callback has already completed');
        }
    }
}
export async function create(dataDirOrOptions = {}, options) {
    let resolvedOptions;
    if (typeof dataDirOrOptions === 'string') {
        if (options?.dataDir !== undefined) {
            throw new TypeError('Provide the TinyJoin data directory either positionally or in options.dataDir, not both');
        }
        resolvedOptions = { ...options, dataDir: dataDirOrOptions };
    }
    else if (dataDirOrOptions === undefined) {
        resolvedOptions = options ?? {};
    }
    else {
        if (options !== undefined) {
            throw new TypeError('TinyJoin options must be the first argument when no positional data directory is used');
        }
        resolvedOptions = dataDirOrOptions;
    }
    const client = new Client(resolvedOptions);
    try {
        await client.waitReady;
        return client;
    }
    catch (error) {
        await client.close().catch(() => undefined);
        throw error;
    }
}
function createWorker(options) {
    const selected = [
        options.worker,
        options.workerFactory,
        options.workerUrl,
    ].filter((value) => value !== undefined);
    if (selected.length > 1) {
        throw new TypeError('Provide only one of worker, workerFactory, or workerUrl to TinyJoin');
    }
    if (options.worker) {
        return { worker: options.worker, resultValidation: 'full' };
    }
    if (options.workerFactory) {
        return { worker: options.workerFactory(), resultValidation: 'full' };
    }
    if (options.workerUrl) {
        return {
            worker: createUrlWorker(options.workerUrl),
            resultValidation: 'full',
        };
    }
    return { worker: createDefaultWorker(), resultValidation: 'header' };
}
function createUrlWorker(url) {
    assertWorkerAvailable();
    return new Worker(url, { name: 'tinyjoin', type: 'module' });
}
function createDefaultWorker() {
    assertWorkerAvailable();
    return new Worker(new URL('../worker/default-entry.js', import.meta.url), {
        name: 'tinyjoin',
        type: 'module',
    });
}
function assertWorkerAvailable() {
    if (typeof Worker === 'undefined') {
        throw new Error('TinyJoin requires a browser Worker. Importing is SSR-safe, but create the client in the browser or provide a Worker-like implementation.');
    }
}
function clientError(code, message) {
    return new ClientError({ code, message });
}
function preparedStatementState(value) {
    const state = typeof value === 'object' && value !== null
        ? preparedStatementStates.get(value)
        : undefined;
    if (!state) {
        throw clientError('INVALID_PREPARED_STATEMENT', 'The value is not a TinyJoin prepared statement');
    }
    return state;
}
function assertPreparedStatementOpen(state) {
    if (state.closed) {
        throw clientError('PREPARED_STATEMENT_CLOSED', 'The TinyJoin prepared statement is closed');
    }
}
function trackPreparedExecution(state, operation) {
    state.inFlight.add(operation);
    void operation.then(() => state.inFlight.delete(operation), () => state.inFlight.delete(operation));
    return operation;
}
function assertClientOptions(options) {
    const supported = new Set([
        'dataDir',
        'worker',
        'workerFactory',
        'workerUrl',
    ]);
    const prototype = isRecord(options)
        ? Object.getPrototypeOf(options)
        : undefined;
    if (!isRecord(options) ||
        (prototype !== Object.prototype && prototype !== null) ||
        Reflect.ownKeys(options).some((key) => typeof key !== 'string' || !supported.has(key))) {
        throw new TypeError('TinyJoin client options support only dataDir, worker, workerFactory, and workerUrl');
    }
}
function storageFromDataDir(dataDir) {
    if (dataDir === undefined || dataDir === 'memory://') {
        return { kind: 'memory' };
    }
    if (typeof dataDir !== 'string' || !dataDir.startsWith('opfs://')) {
        throw new TypeError('TinyJoin dataDir must be memory:// or opfs:// followed by a database name');
    }
    const name = dataDir.slice('opfs://'.length);
    if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(name)) {
        throw new TypeError('A TinyJoin OPFS database name must be 1-64 ASCII letters, numbers, dots, underscores, or hyphens, and start with a letter or number');
    }
    return { kind: 'opfs', name };
}
function parameterize(strings, params) {
    if (!Array.isArray(strings) || strings.length !== params.length + 1) {
        throw new TypeError('TinyJoin sql must be used as a tagged template');
    }
    let sql = strings[0] ?? '';
    for (let index = 0; index < params.length; index++) {
        sql += `$${index + 1}${strings[index + 1] ?? ''}`;
    }
    return sql;
}
function toResults(result, options) {
    const rows = options?.rowMode === 'array'
        ? rowsAsArrays(result.rows, result.fields)
        : result.rows;
    return {
        rows: rows,
        fields: result.fields,
        affectedRows: affectedRows(result),
        command: result.command,
        ...(hasRowCount(result.command) ? { rowCount: result.rowCount } : {}),
        revision: result.revision,
        tables: result.tables,
    };
}
function rowsAsArrays(rows, fields) {
    if (rows.length > 0 && fields.length === 0) {
        throw clientError('ROW_METADATA_UNAVAILABLE', 'TinyJoin cannot return array rows without field metadata');
    }
    return rows.map((row) => fields.map((field) => row[field.name] ?? null));
}
function affectedRows(result) {
    return /^(?:DELETE|INSERT|UPDATE)$/.test(result.command)
        ? result.rowCount
        : 0;
}
function hasRowCount(command) {
    return /^(?:DELETE|INSERT|SELECT|UPDATE)$/.test(command);
}
function assertQueryOptions(options) {
    if (options === undefined) {
        return;
    }
    if (!isRecord(options) ||
        Reflect.ownKeys(options).some((key) => key !== 'rowMode') ||
        (options.rowMode !== undefined &&
            options.rowMode !== 'array' &&
            options.rowMode !== 'object')) {
        throw new TypeError('TinyJoin query options currently support only rowMode: object or array');
    }
}
