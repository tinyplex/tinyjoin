import {
  arrayIsArray,
  errorDetail,
  isBoolean,
  isCount,
  isCountWithin,
  isRecord,
  isString,
  isUndefined,
  MAX_U32,
  objFreeze,
  objHasOwn,
  ownKeys,
  type CodedError,
} from '../common.js';
import {
  isRpcResultHeader,
  type ApplyOutcome,
  type JsonValue,
  type RowMode,
  type Schema,
  type SqlResult,
  type SqlResultText,
} from '../protocol.js';
import type {WorkerEngine} from './engine.js';
import type {PageDevice} from './page-device.js';
import {
  EMPTY_REQUEST,
  WasmBridgeError,
  encodeClosePrepared,
  encodeExecSql,
  encodeExecutePrepared,
  encodeExecuteSql,
  encodePrepareSql,
  encodeSetSchema,
} from './wasm-preflight.js';

export {WasmBridgeError} from './wasm-preflight.js';

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
  check: 12,
  schema: 13,
  setSchema: 14,
} as const;

const BRIDGE_VERSION = 5;
const SUCCESS = 0;
const FAILURE = 1;
const SAFE_RESPONSE = 0;
const DURABLE_RESPONSE = 1;
const ENVELOPE_SLOTS = [0, 1, 2, 3];

let insidePageDeviceCallback = 0;

export type StructuredResponseDisposition = 'safe' | 'durable';

export class WasmStructuredDecodeError extends WasmBridgeError {
  readonly disposition: StructuredResponseDisposition | undefined;

  constructor(message: string, disposition?: StructuredResponseDisposition) {
    super('BRIDGE_SERIALIZATION_ERROR', message, false);
    this.name = 'WasmStructuredDecodeError';
    this.disposition = disposition;
  }
}

export interface RawStructuredWasmEngine {
  callStructured(
    bridgeVersion: number,
    operation: number,
    request: Uint8Array,
  ): unknown;
  free?(): void;
}

export interface RawStructuredWasmEngineConstructor {
  new (device: PageDevice): RawStructuredWasmEngine;
}

/** Opens the direct structured WASM bridge behind the shared page guard. */
export const createStructuredWasmEngine = (
  RawEngine: RawStructuredWasmEngineConstructor,
  device: PageDevice,
): WorkerEngine => {
  assertNotInPageDeviceCallback();
  let raw: RawStructuredWasmEngine;
  try {
    raw = new RawEngine(guardWasmPageDevice(device));
  } catch (error) {
    throw normalizeWasmConstructorError(error);
  }
  return adaptStructuredWasmEngine(raw);
};

/**
 * Normalizes the error the raw engine throws while opening: the JSON text of a
 * bridge error payload.
 */
export const normalizeWasmConstructorError = (error: unknown): unknown => {
  const payload = isString(error) ? parseJson(error) : undefined;
  return isBridgeErrorPayload(payload)
    ? new WasmBridgeError(payload.code, payload.message, payload.retryable)
    : error;
};

/**
 * Adapts a raw engine to WorkerEngine, writing each request as WASM reads it.
 *
 * The engine is a frozen object over closure state rather than a class: only
 * the eleven operations are observable from outside, and the state machine that
 * decides whether the next call is even allowed stays entirely private.
 */
export const adaptStructuredWasmEngine = (
  raw: RawStructuredWasmEngine,
): WorkerEngine => {
  assertNotInPageDeviceCallback();
  let state: 'open' | 'poisoned' | 'closed' = 'open';
  let rawReleased = false;

  const call = (operation: number, request: Uint8Array): unknown =>
    raw.callStructured(BRIDGE_VERSION, operation, request);

  const releaseRaw = (): void => {
    if (rawReleased) {
      return;
    }
    rawReleased = true;
    try {
      raw.free?.();
    } catch {
      // The first uncertain/fatal/close result remains decisive.
    }
  };

  const closeRaw = (): void => {
    if (state !== 'open') {
      return;
    }
    state = 'poisoned';
    try {
      call(WASM_OPERATION.close, EMPTY_REQUEST);
    } catch {
      // The first uncertain/fatal result remains decisive.
    }
    releaseRaw();
  };

  const poisonUnknown = (error: unknown): WasmBridgeError => {
    closeRaw();
    return new WasmBridgeError(
      'STORAGE_COMMIT_OUTCOME_UNKNOWN',
      'TinyJoin could not decode a result after a possible durable mutation' +
        errorDetail(error),
      false,
    );
  };

  const assertCallable = (): void => {
    assertNotInPageDeviceCallback();
    if (state === 'poisoned') {
      throw new WasmBridgeError(
        'STORAGE_ENGINE_POISONED',
        'The TinyJoin engine cannot be used after an uncertain result',
        false,
      );
    }
    if (state === 'closed') {
      throw new WasmBridgeError('ENGINE_CLOSED', 'The TinyJoin engine is closed');
    }
  };

  // Callers assert first, so that a closed engine is reported before a request
  // is written. `mayPublish` marks the operations whose failure could have
  // already changed durable state, and which therefore poison the engine when
  // their outcome cannot be read back.
  const invoke = <Result>(
    operation: number,
    request: Uint8Array,
    mayPublish: boolean,
    decode: (value: unknown) => Result,
  ): Result => {
    let response: unknown;
    try {
      response = call(operation, request);
    } catch (error) {
      throw mayPublish ? poisonUnknown(error) : error;
    }
    try {
      return decode(response);
    } catch (error) {
      if (
        error instanceof WasmBridgeError &&
        !(error instanceof WasmStructuredDecodeError)
      ) {
        if (FATAL_REMOTE_CODES.includes(error.code)) {
          closeRaw();
        }
        throw error;
      }
      const disposition =
        error instanceof WasmStructuredDecodeError
          ? error.disposition
          : undefined;
      throw disposition === 'durable' ||
        (isUndefined(disposition) && mayPublish)
        ? poisonUnknown(error)
        : error;
    }
  };

  // Every operation asserts that the engine is still callable, writes its
  // request, and then invokes.
  return objFreeze({
    executeSql: (
      sql: string,
      params: JsonValue[],
      rowMode?: RowMode,
    ): SqlResult | SqlResultText => {
      assertCallable();
      return invoke(
        WASM_OPERATION.executeSql,
        encodeExecuteSql(sql, params, rowMode === 'array'),
        true,
        decodeSqlResult,
      );
    },

    prepareSql: (sql: string): number => {
      assertCallable();
      return invoke(
        WASM_OPERATION.prepareSql,
        encodePrepareSql(sql),
        false,
        decodePreparedStatementId,
      );
    },

    executePrepared: (
      statementId: number,
      params: JsonValue[],
      rowMode?: RowMode,
    ): SqlResult | SqlResultText => {
      assertCallable();
      return invoke(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(statementId, params, rowMode === 'array'),
        true,
        decodeSqlResult,
      );
    },

    closePrepared: (statementId: number): void => {
      assertCallable();
      invoke(
        WASM_OPERATION.closePrepared,
        encodeClosePrepared(statementId),
        false,
        decodeUnit,
      );
    },

    execSql: (sql: string, rowMode?: RowMode): SqlResult[] => {
      assertCallable();
      return invoke(
        WASM_OPERATION.execSql,
        encodeExecSql(sql, rowMode === 'array'),
        true,
        decodeSqlResults,
      );
    },

    beginTransaction: (): void => {
      assertCallable();
      invoke(WASM_OPERATION.begin, EMPTY_REQUEST, false, decodeUnit);
    },

    commitTransaction: (): ApplyOutcome => {
      assertCallable();
      return invoke(
        WASM_OPERATION.commit,
        EMPTY_REQUEST,
        true,
        decodeApplyOutcome,
      );
    },

    rollbackTransaction: (): void => {
      assertCallable();
      invoke(WASM_OPERATION.rollback, EMPTY_REQUEST, false, decodeUnit);
    },

    inTransaction: (): boolean => {
      assertCallable();
      return invoke(
        WASM_OPERATION.inTransaction,
        EMPTY_REQUEST,
        false,
        decodeBoolean,
      );
    },

    revision: (): number => {
      assertCallable();
      return invoke(
        WASM_OPERATION.revision,
        EMPTY_REQUEST,
        false,
        decodeRevision,
      );
    },

    check: (): void => {
      assertCallable();
      invoke(WASM_OPERATION.check, EMPTY_REQUEST, false, decodeUnit);
    },

    schema: (): Schema => {
      assertCallable();
      return invoke(WASM_OPERATION.schema, EMPTY_REQUEST, false, decodeSchema);
    },

    setSchema: (schema: Schema, drop: boolean): ApplyOutcome => {
      assertCallable();
      return invoke(
        WASM_OPERATION.setSchema,
        encodeSetSchema(schema as unknown as JsonValue, drop),
        true,
        decodeApplyOutcome,
      );
    },

    close: (): void => {
      assertNotInPageDeviceCallback();
      if (state === 'closed') {
        return;
      }
      const wasOpen = state === 'open';
      state = 'closed';
      try {
        if (wasOpen) {
          decodeUnit(call(WASM_OPERATION.close, EMPTY_REQUEST));
        }
      } finally {
        releaseRaw();
      }
    },
  });
};

// A remote failure with one of these codes means the engine is already beyond
// use, so the raw handle is closed rather than left for the next call.
const FATAL_REMOTE_CODES = [
  'RECOVERY_REQUIRED',
  'STORAGE_COMMIT_OUTCOME_UNKNOWN',
  'STORAGE_ENGINE_POISONED',
];

// WASM only ever sees a frozen device whose every method runs inside the
// reentrancy guard, so that page storage cannot call back into the engine.
const guardWasmPageDevice = (device: PageDevice): PageDevice =>
  objFreeze({
    pageCount: () => inPageDeviceCallback(() => device.pageCount()),
    readPage: (low: number, high: number, target: Uint8Array) =>
      inPageDeviceCallback(() => device.readPage(low, high, target)),
    writePages: (low: number, high: number, source: Uint8Array) =>
      inPageDeviceCallback(() => device.writePages(low, high, source)),
    flush: () => inPageDeviceCallback(() => device.flush()),
    close: () => inPageDeviceCallback(() => device.close()),
  });

const inPageDeviceCallback = <Result>(callback: () => Result): Result => {
  insidePageDeviceCallback += 1;
  try {
    return callback();
  } finally {
    insidePageDeviceCallback -= 1;
  }
};

const assertNotInPageDeviceCallback = (): void => {
  if (insidePageDeviceCallback !== 0) {
    throw new WasmBridgeError(
      'ENGINE_REENTRANT_CALL',
      'TinyJoin cannot enter a WASM engine from a page-device callback',
      false,
    );
  }
};

/**
 * Reads one response, whose first line is the JSON text of its `[version,
 * status, disposition, payload]` envelope. The disposition says whether a
 * failure the bridge could not read might still have been published, which is
 * what decides between an error and a poisoned engine. Statement results
 * follow the envelope with a line each, holding their fields and rows, which
 * `read` attaches to their headers unread.
 */
const decoder =
  <Result>(
    label: string,
    read: (payload: unknown, rest: string | undefined) => Result | typeof INVALID,
  ) =>
  (value: unknown): Result => {
    if (!isString(value)) {
      throw invalidStructured('response');
    }
    const end = value.indexOf('\n');
    const envelope = parseJson(end < 0 ? value : value.slice(0, end));
    const rest = end < 0 ? undefined : value.slice(end + 1);
    if (!isDenseEnvelope(envelope)) {
      throw invalidStructured('response envelope');
    }
    if (envelope[0] !== BRIDGE_VERSION) {
      throw new WasmStructuredDecodeError(
        'WASM returned an unsupported structured bridge version',
      );
    }
    const status = envelope[1];
    if (status !== SUCCESS && status !== FAILURE) {
      throw invalidStructured('response status');
    }
    const dispositionTag = envelope[2];
    if (
      dispositionTag !== SAFE_RESPONSE &&
      dispositionTag !== DURABLE_RESPONSE
    ) {
      throw invalidStructured('response disposition');
    }
    const disposition: StructuredResponseDisposition =
      dispositionTag === DURABLE_RESPONSE ? 'durable' : 'safe';
    const payload = envelope[3];
    if (status === FAILURE) {
      if (disposition !== 'safe') {
        throw new WasmStructuredDecodeError(
          'WASM returned a durable structured failure envelope',
          disposition,
        );
      }
      if (!isBridgeErrorPayload(payload) || !isUndefined(rest)) {
        throw invalidStructured('error', disposition);
      }
      throw new WasmBridgeError(
        payload.code,
        payload.message,
        payload.retryable,
      );
    }
    const result = read(payload, rest);
    if (result === INVALID) {
      throw invalidStructured(label, disposition);
    }
    return result;
  };

const INVALID = Symbol('invalid');

/** Reads a payload that stands alone, which `isValid` checks. */
const single =
  <Result>(isValid: (payload: unknown) => payload is Result) =>
  (payload: unknown, rest: string | undefined): Result | typeof INVALID =>
    isUndefined(rest) && isValid(payload) ? payload : INVALID;

// A result's header is checked, and its rows' JSON text passed on unread:
// both come from TinyJoin's own engine, which tests check in full.
const withData = (header: unknown, data: string | undefined): unknown => {
  if (isRecord(header) && isString(data)) {
    header.data = data;
  }
  return header;
};

const invalidStructured = (
  what: string,
  disposition?: StructuredResponseDisposition,
): WasmStructuredDecodeError =>
  new WasmStructuredDecodeError(
    `WASM returned an invalid structured ${what}`,
    disposition,
  );

const decodeUnit = decoder(
  'unit result',
  single((payload): payload is null => payload === null),
);
const decodeBoolean = decoder('boolean result', single(isBoolean));
const decodeRevision = decoder('revision', single(isCount));
const decodeSchema = decoder(
  'schema',
  single((payload): payload is Schema => isRpcResultHeader('schema', payload)),
);
const decodePreparedStatementId = decoder(
  'prepared statement ID',
  single((payload): payload is number => isCountWithin(payload, 1, MAX_U32)),
);
const decodeApplyOutcome = decoder(
  'apply outcome',
  single((payload): payload is ApplyOutcome =>
    isRpcResultHeader('commitTransaction', payload),
  ),
);
const decodeParsedSqlResult = decoder('SQL result', (payload, rest) => {
  const result = withData(payload, rest);
  return isRpcResultHeader('executeSql', result) ? result : INVALID;
});

// The envelope of a success that published nothing, which a statement's header
// then follows, closed by `]` at the end of the first line.
const SAFE_SUCCESS = `[${BRIDGE_VERSION},${SUCCESS},${SAFE_RESPONSE},`;

/**
 * Reads a statement's result. One that published nothing durable, and so has
 * no changes for the Worker to announce, passes on as the text WASM wrote,
 * which only the page parses and checks. Any other is read here.
 */
const decodeSqlResult = (value: unknown): SqlResult | SqlResultText => {
  if (isString(value) && value.startsWith(SAFE_SUCCESS)) {
    const end = value.indexOf('\n');
    if (end > SAFE_SUCCESS.length && value.charCodeAt(end - 1) === 0x5d) {
      return [value.slice(SAFE_SUCCESS.length, end - 1), value.slice(end + 1)];
    }
  }
  return decodeParsedSqlResult(value);
};
const decodeSqlResults = decoder('SQL results', (payload, rest) => {
  const lines = rest?.split('\n') ?? [];
  if (!arrayIsArray(payload) || payload.length !== lines.length) {
    return INVALID;
  }
  const results = payload.map((header, index) =>
    withData(header, lines[index]),
  );
  return isRpcResultHeader('execSql', results) ? results : INVALID;
});

const parseJson = (text: string): unknown => {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
};

const isDenseEnvelope = (value: unknown): value is unknown[] =>
  arrayIsArray(value) &&
  value.length === 4 &&
  ENVELOPE_SLOTS.every((slot) => objHasOwn(value, slot));

// A bridge error payload is exactly its code, message and optional retryable
// flag: anything else is a result that WASM mislabelled as a failure.
const isBridgeErrorPayload = (value: unknown): value is CodedError => {
  if (!isRecord(value)) {
    return false;
  }
  let keys: (string | symbol)[];
  try {
    keys = ownKeys(value);
  } catch {
    return false;
  }
  return (
    isCountWithin(keys.length, 2, 3) &&
    keys.every((key) => BRIDGE_ERROR_KEYS.includes(key as string)) &&
    objHasOwn(value, 'code') &&
    objHasOwn(value, 'message') &&
    isString(value.code) &&
    isString(value.message) &&
    (!objHasOwn(value, 'retryable') || isBoolean(value.retryable))
  );
};

const BRIDGE_ERROR_KEYS = ['code', 'message', 'retryable'];
