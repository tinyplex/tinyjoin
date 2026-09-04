const JSON_NULL = 0;
const JSON_FALSE = 1;
const JSON_TRUE = 2;
const JSON_I64 = 3;
const JSON_F64 = 5;
const JSON_STRING = 6;
const JSON_ARRAY = 7;
const JSON_OBJECT = 8;
const MAX_BYTES = 16 * 1024 * 1024;
const MAX_NODES = 1_000_000;
const MAX_OPERATIONS = 1_000_000;
const MAX_DEPTH = 64;
const MAX_SAFE_INTEGER = BigInt(Number.MAX_SAFE_INTEGER);
const MAX_U32 = 0xffff_ffff;
// Deterministic estimates aligned with the wasm32 retained model. They bound
// work and allocation independently of a particular JavaScript engine's RSS.
const STRING_OVERHEAD = 12;
const VECTOR_OVERHEAD = 12;
const RUST_VALUE_BYTES = 24;
const RUST_MAP_ENTRY_OVERHEAD = 128;
const arrayIsArray = Array.isArray;
const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
const hasOwn = Object.hasOwn;
const numberIsFinite = Number.isFinite;
const numberIsSafeInteger = Number.isSafeInteger;
const ownKeys = Reflect.ownKeys;
export class WasmBridgeError extends Error {
    code;
    retryable;
    constructor(code, message, retryable) {
        super(message);
        this.name = 'WasmBridgeError';
        this.code = code;
        if (retryable !== undefined) {
            this.retryable = retryable;
        }
    }
}
class PreflightBudget {
    retained = 0;
    nodes = 0;
    operations = 0;
    retain(bytes) {
        if (!numberIsSafeInteger(bytes) || bytes < 0) {
            throw resourceLimit();
        }
        const next = this.retained + bytes;
        if (!numberIsSafeInteger(next) || next > MAX_BYTES) {
            throw resourceLimit();
        }
        this.retained = next;
    }
    string(bytes) {
        this.retain(bytes + STRING_OVERHEAD);
    }
    vector(length, elementBytes) {
        this.retain(length * elementBytes + VECTOR_OVERHEAD);
    }
    node(depth) {
        if (depth > MAX_DEPTH) {
            throw invalidBridgeValue('A bridge value is too deeply nested');
        }
        this.nodes = checkedIncrement(this.nodes, MAX_NODES);
    }
    operation(count = 1) {
        if (!numberIsSafeInteger(count) || count < 0) {
            throw resourceLimit();
        }
        const next = this.operations + count;
        if (!numberIsSafeInteger(next) || next > MAX_OPERATIONS) {
            throw resourceLimit();
        }
        this.operations = next;
    }
}
class ModelSink {
    budget = new PreflightBudget();
    #bytes = 0;
    u8(value) {
        assertUint(value, 0xff);
        this.#add(1);
    }
    u32(value) {
        assertUint(value, MAX_U32);
        this.#add(4);
    }
    i64(value) {
        if (value < -MAX_SAFE_INTEGER || value > MAX_SAFE_INTEGER) {
            throw invalidBridgeValue('A JSON integer is not JavaScript-safe');
        }
        this.#add(8);
    }
    f64(value) {
        if (!numberIsFinite(value)) {
            throw invalidBridgeValue('A JSON number must be finite');
        }
        this.#add(8);
    }
    string(value) {
        if (typeof value !== 'string') {
            throw invalidBridgeValue('A bridge string must be a string');
        }
        const bytes = utf8Length(value);
        this.budget.string(bytes);
        this.u32(bytes);
        this.#add(bytes);
    }
    #add(bytes) {
        const next = this.#bytes + bytes;
        if (!numberIsSafeInteger(next) || next > MAX_BYTES) {
            throw resourceLimit();
        }
        this.#bytes = next;
    }
}
function preflight(write) {
    write(new ModelSink());
}
/** Validates and bounds a request once before passing it unchanged to WASM. */
export function preflightExecuteSql(sql, params) {
    preflight((sink) => {
        sink.string(sql);
        writeJsonValues(sink, params, 0, 'SQL parameters');
    });
}
export function preflightPrepareSql(sql) {
    preflight((sink) => sink.string(sql));
}
export function preflightExecutePrepared(statementId, params) {
    preflight((sink) => {
        sink.u32(preparedStatementId(statementId));
        writeJsonValues(sink, params, 0, 'SQL parameters');
    });
}
export function preflightClosePrepared(statementId) {
    preflight((sink) => sink.u32(preparedStatementId(statementId)));
}
export function preflightExecSql(sql) {
    preflight((sink) => sink.string(sql));
}
function writeJsonValues(sink, input, depth, label) {
    writeArray(sink, input, label, RUST_VALUE_BYTES, (target, value) => writeJsonValue(target, value, depth));
}
function writeJsonValue(sink, value, depth) {
    sink.budget.operation();
    sink.budget.node(depth);
    if (value === null) {
        sink.u8(JSON_NULL);
    }
    else if (value === false) {
        sink.u8(JSON_FALSE);
    }
    else if (value === true) {
        sink.u8(JSON_TRUE);
    }
    else if (typeof value === 'number') {
        if (!numberIsFinite(value)) {
            throw invalidBridgeValue('A JSON number must be finite');
        }
        if (numberIsSafeInteger(value)) {
            sink.u8(JSON_I64);
            // BigInt(-0) is 0n, preserving the prior bridge's canonical spelling.
            sink.i64(BigInt(value));
        }
        else {
            sink.u8(JSON_F64);
            sink.f64(value);
        }
    }
    else if (typeof value === 'string') {
        sink.u8(JSON_STRING);
        sink.string(value);
    }
    else if (isArray(value)) {
        sink.u8(JSON_ARRAY);
        const length = denseArrayLength(value, 'JSON array');
        sink.budget.vector(length, RUST_VALUE_BYTES);
        writeCount(sink, length);
        for (let index = 0; index < length; index += 1) {
            writeJsonValue(sink, indexedDataValue(value, index, 'JSON array'), depth + 1);
        }
    }
    else if (isRecord(value)) {
        sink.u8(JSON_OBJECT);
        const count = enumerableDataCount(value);
        writeCount(sink, count);
        let visited = 0;
        forEachEnumerableDataEntry(value, (key, child) => {
            visited += 1;
            sink.string(key);
            sink.budget.retain(RUST_VALUE_BYTES + RUST_MAP_ENTRY_OVERHEAD);
            writeJsonValue(sink, child, depth + 1);
        });
        if (visited !== count) {
            throw invalidBridgeValue('A bridge object changed while it was inspected');
        }
    }
    else {
        throw invalidBridgeValue('A value is not JSON-compatible');
    }
}
function writeArray(sink, input, label, retainedElementBytes, write) {
    const length = denseArrayLength(input, label);
    sink.budget.vector(length, retainedElementBytes);
    writeCount(sink, length);
    for (let index = 0; index < length; index += 1) {
        write(sink, indexedDataValue(input, index, label));
    }
    if (denseArrayLength(input, label) !== length) {
        throw invalidBridgeValue('A bridge array changed while it was inspected');
    }
}
function writeCount(sink, count) {
    if (!numberIsSafeInteger(count) || count < 0 || count > MAX_OPERATIONS) {
        throw resourceLimit();
    }
    sink.u32(count);
}
const MISSING = Symbol('missing');
function isRecord(value) {
    return typeof value === 'object' && value !== null && !isArray(value);
}
function isArray(value) {
    try {
        return arrayIsArray(value);
    }
    catch {
        throw invalidBridgeValue('A bridge array could not be inspected');
    }
}
function ownDataField(value, name) {
    let descriptor;
    try {
        descriptor = getOwnPropertyDescriptor(value, name);
    }
    catch {
        throw invalidBridgeValue('A bridge property descriptor could not be read');
    }
    if (descriptor === undefined) {
        return MISSING;
    }
    if (!hasOwn(descriptor, 'value')) {
        throw invalidBridgeValue('Bridge accessors are not supported');
    }
    return descriptor.value;
}
function enumerableDataCount(value) {
    let count = 0;
    forEachEnumerableDataEntry(value, () => {
        count += 1;
        if (count > MAX_OPERATIONS) {
            throw resourceLimit();
        }
    });
    return count;
}
function forEachEnumerableDataEntry(value, visit) {
    let keys;
    try {
        keys = ownKeys(value);
    }
    catch {
        throw invalidBridgeValue('Bridge object keys could not be read');
    }
    if (keys.length > MAX_OPERATIONS) {
        throw resourceLimit();
    }
    for (const key of keys) {
        if (typeof key !== 'string') {
            continue;
        }
        let descriptor;
        try {
            descriptor = getOwnPropertyDescriptor(value, key);
        }
        catch {
            throw invalidBridgeValue('A bridge property descriptor could not be read');
        }
        if (descriptor === undefined || !hasOwn(descriptor, 'value')) {
            throw invalidBridgeValue('Bridge accessors are not supported');
        }
        if (descriptor.enumerable) {
            visit(key, descriptor.value);
        }
    }
}
function denseArrayLength(value, label) {
    if (!isArray(value)) {
        throw invalidBridgeValue(`${label} must be an array`);
    }
    const length = ownDataField(value, 'length');
    if (typeof length !== 'number' ||
        !numberIsSafeInteger(length) ||
        length < 0 ||
        length > MAX_OPERATIONS) {
        throw resourceLimit();
    }
    return length;
}
function indexedDataValue(value, index, label) {
    const item = ownDataField(value, String(index));
    if (item === MISSING || item === undefined) {
        throw invalidBridgeValue(`${label} cannot be sparse`);
    }
    return item;
}
function preparedStatementId(value) {
    if (!numberIsSafeInteger(value) ||
        Number(value) < 1 ||
        Number(value) > MAX_U32) {
        throw invalidBridgeValue('A prepared statement ID must be a nonzero unsigned 32-bit integer');
    }
    return Number(value);
}
function assertUint(value, maximum) {
    if (!numberIsSafeInteger(value) || value < 0 || value > maximum) {
        throw invalidBridgeValue('A bridge unsigned integer is out of range');
    }
}
function checkedIncrement(value, maximum) {
    const next = value + 1;
    if (!numberIsSafeInteger(next) || next > maximum) {
        throw resourceLimit();
    }
    return next;
}
function utf8Length(value) {
    let bytes = 0;
    for (let index = 0; index < value.length; index += 1) {
        const code = value.charCodeAt(index);
        if (code < 0x80) {
            bytes += 1;
        }
        else if (code < 0x800) {
            bytes += 2;
        }
        else if (code >= 0xd800 &&
            code <= 0xdbff &&
            index + 1 < value.length &&
            value.charCodeAt(index + 1) >= 0xdc00 &&
            value.charCodeAt(index + 1) <= 0xdfff) {
            bytes += 4;
            index += 1;
        }
        else {
            // TextEncoder replaces lone surrogates with U+FFFD (three bytes).
            bytes += 3;
        }
        if (bytes > MAX_BYTES) {
            throw resourceLimit();
        }
    }
    return bytes;
}
function invalidBridgeValue(message) {
    return new WasmBridgeError('INVALID_BRIDGE_VALUE', message);
}
function resourceLimit() {
    return new WasmBridgeError('RESOURCE_LIMIT', 'A bridge call exceeded its resource limit');
}
