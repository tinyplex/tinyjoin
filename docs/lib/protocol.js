export const PROTOCOL_VERSION = 7;
const MAX_U32 = 0xffff_ffff;
export function isWorkerResponse(value) {
    if (!isRecord(value) ||
        value.v !== PROTOCOL_VERSION ||
        !isSafeNonNegativeInteger(value.id) ||
        Number(value.id) < 1) {
        return false;
    }
    if (value.ok === true) {
        return hasExactKeys(value, ['v', 'id', 'ok', 'result']);
    }
    return (value.ok === false &&
        hasExactKeys(value, ['v', 'id', 'ok', 'error']) &&
        isSerializedError(value.error));
}
export function isWorkerEvent(value) {
    if (!isRecord(value) ||
        !hasExactKeys(value, ['v', 'event', 'payload']) ||
        value.v !== PROTOCOL_VERSION) {
        return false;
    }
    if (value.event === 'tablesChanged') {
        return isApplyOutcome(value.payload);
    }
    return false;
}
export function isRpcResult(method, value) {
    switch (method) {
        case 'init':
            return (isRecord(value) &&
                hasExactKeys(value, ['revision']) &&
                isSafeNonNegativeInteger(value.revision));
        case 'closePrepared':
        case 'rollbackTransaction':
        case 'close':
            return value === undefined;
        case 'commitTransaction':
            return isApplyOutcome(value);
        case 'executeSql':
        case 'executePrepared':
            return isSqlResult(value);
        case 'prepareSql':
            return (isRecord(value) &&
                hasExactKeys(value, ['statementId']) &&
                isPreparedStatementId(value.statementId));
        case 'execSql':
            return isDenseArray(value, isSqlResult);
        case 'beginTransaction':
            return (isRecord(value) &&
                hasExactKeys(value, ['transactionId']) &&
                isTransactionId(value.transactionId));
        default: {
            const exhaustive = method;
            return exhaustive;
        }
    }
}
/**
 * Checks only the fixed result envelope produced by TinyJoin's bundled Worker.
 * The Worker has already validated the complete WASM result before posting it;
 * avoiding another walk here keeps large row sets off the UI thread's hot path.
 */
export function isRpcResultHeader(method, value) {
    switch (method) {
        case 'executeSql':
        case 'executePrepared':
            return isSqlResultHeader(value);
        case 'execSql':
            return Array.isArray(value) && value.every(isSqlResultHeader);
        default:
            return isRpcResult(method, value);
    }
}
export function isWorkerRequest(value) {
    if (!isRecord(value) ||
        !hasExactKeys(value, ['v', 'id', 'method', 'params']) ||
        value.v !== PROTOCOL_VERSION ||
        !Number.isSafeInteger(value.id) ||
        Number(value.id) < 1 ||
        typeof value.method !== 'string') {
        return false;
    }
    switch (value.method) {
        case 'init':
            return (isRecord(value.params) &&
                hasExactKeys(value.params, ['storage']) &&
                isStorageOptions(value.params.storage));
        case 'executeSql':
            return (isRecord(value.params) &&
                hasOnlyKeys(value.params, ['sql', 'params', 'transactionId']) &&
                typeof value.params.sql === 'string' &&
                isDenseArray(value.params.params, (param) => isJsonValue(param)) &&
                isOptionalTransactionId(value.params.transactionId));
        case 'prepareSql':
            return (isRecord(value.params) &&
                hasExactKeys(value.params, ['sql']) &&
                typeof value.params.sql === 'string');
        case 'executePrepared':
            return (isRecord(value.params) &&
                hasOnlyKeys(value.params, [
                    'statementId',
                    'params',
                    'transactionId',
                ]) &&
                isPreparedStatementId(value.params.statementId) &&
                isDenseArray(value.params.params, (param) => isJsonValue(param)) &&
                isOptionalTransactionId(value.params.transactionId));
        case 'closePrepared':
            return (isRecord(value.params) &&
                hasExactKeys(value.params, ['statementId']) &&
                isPreparedStatementId(value.params.statementId));
        case 'execSql':
            return (isRecord(value.params) &&
                hasOnlyKeys(value.params, ['sql', 'transactionId']) &&
                typeof value.params.sql === 'string' &&
                isOptionalTransactionId(value.params.transactionId));
        case 'beginTransaction':
            return value.params === undefined;
        case 'commitTransaction':
        case 'rollbackTransaction':
            return (isRecord(value.params) &&
                hasOnlyKeys(value.params, ['transactionId']) &&
                isTransactionId(value.params.transactionId));
        case 'close':
            return value.params === undefined;
        default:
            return false;
    }
}
function isOptionalTransactionId(value) {
    return value === undefined || isTransactionId(value);
}
function isTransactionId(value) {
    return typeof value === 'string' && value.length > 0 && value.length <= 128;
}
function isPreparedStatementId(value) {
    return (Number.isSafeInteger(value) &&
        Number(value) > 0 &&
        Number(value) <= MAX_U32);
}
function isStorageOptions(value) {
    if (!isRecord(value)) {
        return false;
    }
    return value.kind === 'memory'
        ? hasExactKeys(value, ['kind'])
        : value.kind === 'opfs' &&
            hasExactKeys(value, ['kind', 'name']) &&
            typeof value.name === 'string';
}
function hasOnlyKeys(value, allowedKeys) {
    return Reflect.ownKeys(value).every((key) => typeof key === 'string' && allowedKeys.includes(key));
}
function hasExactKeys(value, expectedKeys) {
    const keys = Reflect.ownKeys(value);
    return (keys.length === expectedKeys.length &&
        keys.every((key) => typeof key === 'string' && expectedKeys.includes(key)));
}
function isDenseArray(value, isItem) {
    if (!Array.isArray(value) || value.length > MAX_PROTOCOL_ARRAY_ITEMS) {
        return false;
    }
    for (let index = 0; index < value.length; index++) {
        if (!Object.hasOwn(value, index) || !isItem(value[index])) {
            return false;
        }
    }
    return true;
}
const MAX_PROTOCOL_ARRAY_ITEMS = 1_000_000;
export function isSerializedError(value) {
    return (isRecord(value) &&
        hasOnlyKeys(value, ['code', 'message', 'details', 'retryable']) &&
        Object.hasOwn(value, 'code') &&
        Object.hasOwn(value, 'message') &&
        typeof value.code === 'string' &&
        typeof value.message === 'string' &&
        (value.details === undefined || isJsonValue(value.details)) &&
        (value.retryable === undefined || typeof value.retryable === 'boolean'));
}
export function isRecord(value) {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
}
function isApplyOutcome(value) {
    return (isRecord(value) &&
        hasExactKeys(value, ['revision', 'tables']) &&
        isSafeNonNegativeInteger(value.revision) &&
        isDenseArray(value.tables, (table) => typeof table === 'string'));
}
function isRow(value) {
    return (isJsonRecord(value) &&
        Object.values(value).every((cell) => isJsonValue(cell)));
}
function isResultField(value) {
    return (isRecord(value) &&
        hasExactKeys(value, ['name', 'dataTypeID']) &&
        typeof value.name === 'string' &&
        isSafeNonNegativeInteger(value.dataTypeID) &&
        Number(value.dataTypeID) <= MAX_U32);
}
function isSqlResult(value) {
    return (isRecord(value) &&
        hasExactKeys(value, [
            'command',
            'fields',
            'revision',
            'rowCount',
            'rows',
            'tables',
        ]) &&
        typeof value.command === 'string' &&
        isDenseArray(value.fields, isResultField) &&
        isSafeNonNegativeInteger(value.revision) &&
        isSafeNonNegativeInteger(value.rowCount) &&
        isDenseArray(value.rows, isRow) &&
        isDenseArray(value.tables, (table) => typeof table === 'string'));
}
function isSqlResultHeader(value) {
    return (isRecord(value) &&
        hasExactKeys(value, [
            'command',
            'fields',
            'revision',
            'rowCount',
            'rows',
            'tables',
        ]) &&
        typeof value.command === 'string' &&
        Array.isArray(value.fields) &&
        isSafeNonNegativeInteger(value.revision) &&
        isSafeNonNegativeInteger(value.rowCount) &&
        Array.isArray(value.rows) &&
        isDenseArray(value.tables, (table) => typeof table === 'string'));
}
function isSafeNonNegativeInteger(value) {
    return Number.isSafeInteger(value) && Number(value) >= 0;
}
function isJsonRecord(value) {
    if (!isRecord(value)) {
        return false;
    }
    const prototype = Object.getPrototypeOf(value);
    return ((prototype === Object.prototype || prototype === null) &&
        Reflect.ownKeys(value).every((key) => typeof key === 'string'));
}
function isJsonValue(value, seen = new WeakSet(), depth = 0) {
    if (depth > 64) {
        return false;
    }
    if (value === null ||
        typeof value === 'boolean' ||
        typeof value === 'string') {
        return true;
    }
    if (typeof value === 'number') {
        return Number.isFinite(value);
    }
    if (typeof value !== 'object' || seen.has(value)) {
        return false;
    }
    seen.add(value);
    const valid = Array.isArray(value)
        ? isDenseArray(value, (item) => isJsonValue(item, seen, depth + 1))
        : isJsonRecord(value) &&
            Object.values(value).every((item) => isJsonValue(item, seen, depth + 1));
    seen.delete(value);
    return valid;
}
