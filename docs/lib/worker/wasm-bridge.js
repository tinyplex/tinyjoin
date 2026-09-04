import { isRpcResult, } from '../protocol.js';
import { WasmBridgeError, preflightClosePrepared, preflightExecSql, preflightExecutePrepared, preflightExecuteSql, preflightPrepareSql, } from './wasm-preflight.js';
export { WasmBridgeError } from './wasm-preflight.js';
export const WASM_OPERATION = {
    executeSql: 1,
    execSql: 2,
    prepareSql: 3,
    executePrepared: 4,
    closePrepared: 5,
    begin: 6,
    commit: 7,
    rollback: 8,
    inTransaction: 9,
    revision: 10,
    close: 11,
};
const BRIDGE_VERSION = 2;
const SUCCESS = 0;
const FAILURE = 1;
const SAFE_RESPONSE = 0;
const DURABLE_RESPONSE = 1;
const MAX_U32 = 0xffff_ffff;
const arrayIsArray = Array.isArray;
const hasOwn = Object.hasOwn;
const numberIsSafeInteger = Number.isSafeInteger;
let insidePageDeviceCallback = 0;
export class WasmStructuredDecodeError extends WasmBridgeError {
    disposition;
    constructor(message, disposition) {
        super('BRIDGE_SERIALIZATION_ERROR', message, false);
        this.name = 'WasmStructuredDecodeError';
        this.disposition = disposition;
    }
}
function guardWasmPageDevice(device) {
    return Object.freeze({
        pageCount: () => inPageDeviceCallback(() => device.pageCount()),
        readPage: (low, high, target) => inPageDeviceCallback(() => device.readPage(low, high, target)),
        writePage: (low, high, source) => inPageDeviceCallback(() => device.writePage(low, high, source)),
        flush: () => inPageDeviceCallback(() => device.flush()),
        close: () => inPageDeviceCallback(() => device.close()),
    });
}
function inPageDeviceCallback(callback) {
    insidePageDeviceCallback += 1;
    try {
        return callback();
    }
    finally {
        insidePageDeviceCallback -= 1;
    }
}
function assertNotInPageDeviceCallback() {
    if (insidePageDeviceCallback !== 0) {
        throw new WasmBridgeError('ENGINE_REENTRANT_CALL', 'TinyJoin cannot enter a WASM engine from a page-device callback', false);
    }
}
/** Opens the direct structured WASM bridge behind the shared page guard. */
export function createStructuredWasmEngine(RawEngine, device) {
    assertNotInPageDeviceCallback();
    try {
        return new StructuredWasmEngine(new RawEngine(guardWasmPageDevice(device)));
    }
    catch (error) {
        throw normalizeWasmConstructorError(error);
    }
}
/** Normalizes the direct error payload thrown while opening the raw engine. */
export function normalizeWasmConstructorError(error) {
    if (!isBridgeErrorPayload(error)) {
        return error;
    }
    return new WasmBridgeError(error.code, error.message, error.retryable);
}
/** WorkerEngine adapter that passes structured values directly into WASM. */
export class StructuredWasmEngine {
    #raw;
    #state = 'open';
    #rawReleased = false;
    constructor(raw) {
        assertNotInPageDeviceCallback();
        this.#raw = raw;
    }
    executeSql(sql, params) {
        this.#assertCallable();
        preflightExecuteSql(sql, params);
        return this.#invoke(WASM_OPERATION.executeSql, { sql, params }, true, decodeSqlResultResponse);
    }
    prepareSql(sql) {
        this.#assertCallable();
        preflightPrepareSql(sql);
        return this.#invoke(WASM_OPERATION.prepareSql, sql, false, decodePreparedStatementIdResponse);
    }
    executePrepared(statementId, params) {
        this.#assertCallable();
        preflightExecutePrepared(statementId, params);
        return this.#invoke(WASM_OPERATION.executePrepared, { statementId, params }, true, decodeSqlResultResponse);
    }
    closePrepared(statementId) {
        this.#assertCallable();
        preflightClosePrepared(statementId);
        this.#invoke(WASM_OPERATION.closePrepared, statementId, false, decodeUnitResponse);
    }
    execSql(sql) {
        this.#assertCallable();
        preflightExecSql(sql);
        return this.#invoke(WASM_OPERATION.execSql, sql, true, decodeSqlResultsResponse);
    }
    beginTransaction() {
        this.#assertCallable();
        this.#invoke(WASM_OPERATION.begin, undefined, false, decodeUnitResponse);
    }
    commitTransaction() {
        this.#assertCallable();
        return this.#invoke(WASM_OPERATION.commit, undefined, true, decodeApplyOutcomeResponse);
    }
    rollbackTransaction() {
        this.#assertCallable();
        this.#invoke(WASM_OPERATION.rollback, undefined, false, decodeUnitResponse);
    }
    inTransaction() {
        this.#assertCallable();
        return this.#invoke(WASM_OPERATION.inTransaction, undefined, false, decodeBooleanResponse);
    }
    revision() {
        this.#assertCallable();
        return this.#invoke(WASM_OPERATION.revision, undefined, false, decodeRevisionResponse);
    }
    close() {
        assertNotInPageDeviceCallback();
        if (this.#state === 'closed') {
            return;
        }
        const wasOpen = this.#state === 'open';
        this.#state = 'closed';
        try {
            if (wasOpen) {
                const response = this.#raw.callStructured(BRIDGE_VERSION, WASM_OPERATION.close, undefined);
                decodeUnitResponse(response);
            }
        }
        finally {
            this.#releaseRaw();
        }
    }
    #invoke(operation, payload, mayPublish, decode) {
        this.#assertCallable();
        let response;
        try {
            response = this.#raw.callStructured(BRIDGE_VERSION, operation, payload);
        }
        catch (error) {
            if (mayPublish) {
                throw this.#poisonUnknown(error);
            }
            throw error;
        }
        try {
            return decode(response);
        }
        catch (error) {
            if (error instanceof WasmBridgeError &&
                !(error instanceof WasmStructuredDecodeError)) {
                this.#closeOnFatalRemoteError(error);
                throw error;
            }
            const disposition = error instanceof WasmStructuredDecodeError
                ? error.disposition
                : undefined;
            if (disposition === 'durable' ||
                (disposition === undefined && mayPublish)) {
                throw this.#poisonUnknown(error);
            }
            throw error;
        }
    }
    #assertCallable() {
        assertNotInPageDeviceCallback();
        if (this.#state === 'poisoned') {
            throw new WasmBridgeError('STORAGE_ENGINE_POISONED', 'The TinyJoin engine cannot be used after an uncertain result', false);
        }
        if (this.#state === 'closed') {
            throw new WasmBridgeError('ENGINE_CLOSED', 'The TinyJoin engine is closed');
        }
    }
    #closeOnFatalRemoteError(error) {
        if (error.code === 'RECOVERY_REQUIRED' ||
            error.code === 'STORAGE_COMMIT_OUTCOME_UNKNOWN' ||
            error.code === 'STORAGE_ENGINE_POISONED') {
            this.#closeRaw();
        }
    }
    #poisonUnknown(error) {
        this.#closeRaw();
        const detail = error instanceof Error && error.message ? `: ${error.message}` : '';
        return new WasmBridgeError('STORAGE_COMMIT_OUTCOME_UNKNOWN', `TinyJoin could not decode a result after a possible durable mutation${detail}`, false);
    }
    #closeRaw() {
        if (this.#state !== 'open') {
            return;
        }
        this.#state = 'poisoned';
        try {
            this.#raw.callStructured(BRIDGE_VERSION, WASM_OPERATION.close, undefined);
        }
        catch {
            // The first uncertain/fatal result remains decisive.
        }
        this.#releaseRaw();
    }
    #releaseRaw() {
        if (this.#rawReleased) {
            return;
        }
        this.#rawReleased = true;
        try {
            this.#raw.free?.();
        }
        catch {
            // The first uncertain/fatal/close result remains decisive.
        }
    }
}
function decodeUnitResponse(value) {
    return decodeResponse(value, 'unit result', (payload) => payload === undefined ? undefined : INVALID_RESULT);
}
function decodeBooleanResponse(value) {
    return decodeResponse(value, 'boolean result', (payload) => typeof payload === 'boolean' ? payload : INVALID_RESULT);
}
function decodeRevisionResponse(value) {
    return decodeResponse(value, 'revision', (payload) => numberIsSafeInteger(payload) && Number(payload) >= 0
        ? Number(payload)
        : INVALID_RESULT);
}
function decodePreparedStatementIdResponse(value) {
    return decodeResponse(value, 'prepared statement ID', (payload) => numberIsSafeInteger(payload) &&
        Number(payload) > 0 &&
        Number(payload) <= MAX_U32
        ? Number(payload)
        : INVALID_RESULT);
}
function decodeApplyOutcomeResponse(value) {
    return decodeResponse(value, 'apply outcome', (payload) => isRpcResult('commitTransaction', payload) ? payload : INVALID_RESULT);
}
function decodeSqlResultResponse(value) {
    return decodeResponse(value, 'SQL result', (payload) => isRpcResult('executeSql', payload) ? payload : INVALID_RESULT);
}
function decodeSqlResultsResponse(value) {
    return decodeResponse(value, 'SQL results', (payload) => isRpcResult('execSql', payload) ? payload : INVALID_RESULT);
}
const INVALID_RESULT = Symbol('invalid structured result');
function decodeResponse(value, label, decode) {
    if (!isDenseEnvelope(value)) {
        throw new WasmStructuredDecodeError('WASM returned an invalid structured response envelope');
    }
    if (value[0] !== BRIDGE_VERSION) {
        throw new WasmStructuredDecodeError('WASM returned an unsupported structured bridge version');
    }
    const status = value[1];
    if (status !== SUCCESS && status !== FAILURE) {
        throw new WasmStructuredDecodeError('WASM returned an invalid structured response status');
    }
    const dispositionTag = value[2];
    if (dispositionTag !== SAFE_RESPONSE && dispositionTag !== DURABLE_RESPONSE) {
        throw new WasmStructuredDecodeError('WASM returned an invalid structured response disposition');
    }
    const disposition = dispositionTag === DURABLE_RESPONSE ? 'durable' : 'safe';
    const payload = value[3];
    if (status === FAILURE) {
        if (disposition !== 'safe') {
            throw new WasmStructuredDecodeError('WASM returned a durable structured failure envelope', disposition);
        }
        if (!isBridgeErrorPayload(payload)) {
            throw new WasmStructuredDecodeError('WASM returned an invalid structured error', disposition);
        }
        throw new WasmBridgeError(payload.code, payload.message, payload.retryable);
    }
    const result = decode(payload);
    if (result === INVALID_RESULT) {
        throw new WasmStructuredDecodeError(`WASM returned an invalid structured ${label}`, disposition);
    }
    return result;
}
function isDenseEnvelope(value) {
    return (arrayIsArray(value) &&
        value.length === 4 &&
        hasOwn(value, 0) &&
        hasOwn(value, 1) &&
        hasOwn(value, 2) &&
        hasOwn(value, 3));
}
function isBridgeErrorPayload(value) {
    if (typeof value !== 'object' || value === null || arrayIsArray(value)) {
        return false;
    }
    let keys;
    try {
        keys = Reflect.ownKeys(value);
    }
    catch {
        return false;
    }
    if (keys.length < 2 ||
        keys.length > 3 ||
        !keys.every((key) => key === 'code' || key === 'message' || key === 'retryable')) {
        return false;
    }
    const record = value;
    return (hasOwn(record, 'code') &&
        hasOwn(record, 'message') &&
        typeof record.code === 'string' &&
        typeof record.message === 'string' &&
        (!hasOwn(record, 'retryable') || typeof record.retryable === 'boolean'));
}
