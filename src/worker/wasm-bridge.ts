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
  isRpcResult,
  type ApplyOutcome,
  type JsonValue,
  type SqlResult,
} from '../protocol.js';
import type {WorkerEngine} from './engine.js';
import type {PageDevice} from './page-device.js';
import {
  WasmBridgeError,
  preflightClosePrepared,
  preflightExecSql,
  preflightExecutePrepared,
  preflightExecuteSql,
  preflightPrepareSql,
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
} as const;

const BRIDGE_VERSION = 2;
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
    payload: unknown,
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

/** Normalizes the direct error payload thrown while opening the raw engine. */
export const normalizeWasmConstructorError = (error: unknown): unknown =>
  isBridgeErrorPayload(error)
    ? new WasmBridgeError(error.code, error.message, error.retryable)
    : error;

/**
 * Adapts a raw engine to WorkerEngine, passing structured values straight into
 * WASM.
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

  const call = (operation: number, payload: unknown): unknown =>
    raw.callStructured(BRIDGE_VERSION, operation, payload);

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
      call(WASM_OPERATION.close, undefined);
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
  // is measured. `mayPublish` marks the operations whose failure could have
  // already changed durable state, and which therefore poison the engine when
  // their outcome cannot be read back.
  const invoke = <Result>(
    operation: number,
    payload: unknown,
    mayPublish: boolean,
    decode: (value: unknown) => Result,
  ): Result => {
    let response: unknown;
    try {
      response = call(operation, payload);
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

  // Every operation asserts that the engine is still callable, measures its
  // request, and then invokes.
  return objFreeze({
    executeSql: (sql: string, params: JsonValue[]): SqlResult => {
      assertCallable();
      preflightExecuteSql(sql, params);
      return invoke(
        WASM_OPERATION.executeSql,
        {sql, params},
        true,
        decodeSqlResult,
      );
    },

    prepareSql: (sql: string): number => {
      assertCallable();
      preflightPrepareSql(sql);
      return invoke(
        WASM_OPERATION.prepareSql,
        sql,
        false,
        decodePreparedStatementId,
      );
    },

    executePrepared: (statementId: number, params: JsonValue[]): SqlResult => {
      assertCallable();
      preflightExecutePrepared(statementId, params);
      return invoke(
        WASM_OPERATION.executePrepared,
        {statementId, params},
        true,
        decodeSqlResult,
      );
    },

    closePrepared: (statementId: number): void => {
      assertCallable();
      preflightClosePrepared(statementId);
      invoke(WASM_OPERATION.closePrepared, statementId, false, decodeUnit);
    },

    execSql: (sql: string): SqlResult[] => {
      assertCallable();
      preflightExecSql(sql);
      return invoke(WASM_OPERATION.execSql, sql, true, decodeSqlResults);
    },

    beginTransaction: (): void => {
      assertCallable();
      invoke(WASM_OPERATION.begin, undefined, false, decodeUnit);
    },

    commitTransaction: (): ApplyOutcome => {
      assertCallable();
      return invoke(WASM_OPERATION.commit, undefined, true, decodeApplyOutcome);
    },

    rollbackTransaction: (): void => {
      assertCallable();
      invoke(WASM_OPERATION.rollback, undefined, false, decodeUnit);
    },

    inTransaction: (): boolean => {
      assertCallable();
      return invoke(
        WASM_OPERATION.inTransaction,
        undefined,
        false,
        decodeBoolean,
      );
    },

    revision: (): number => {
      assertCallable();
      return invoke(WASM_OPERATION.revision, undefined, false, decodeRevision);
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
          decodeUnit(call(WASM_OPERATION.close, undefined));
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
    writePage: (low: number, high: number, source: Uint8Array) =>
      inPageDeviceCallback(() => device.writePage(low, high, source)),
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
 * Reads one `[version, status, disposition, payload]` envelope. The disposition
 * says whether a failure the bridge could not read might still have been
 * published, which is what decides between an error and a poisoned engine.
 */
const decoder =
  <Result>(label: string, isValid: (payload: unknown) => payload is Result) =>
  (value: unknown): Result => {
    if (!isDenseEnvelope(value)) {
      throw invalidStructured('response envelope');
    }
    if (value[0] !== BRIDGE_VERSION) {
      throw new WasmStructuredDecodeError(
        'WASM returned an unsupported structured bridge version',
      );
    }
    const status = value[1];
    if (status !== SUCCESS && status !== FAILURE) {
      throw invalidStructured('response status');
    }
    const dispositionTag = value[2];
    if (
      dispositionTag !== SAFE_RESPONSE &&
      dispositionTag !== DURABLE_RESPONSE
    ) {
      throw invalidStructured('response disposition');
    }
    const disposition: StructuredResponseDisposition =
      dispositionTag === DURABLE_RESPONSE ? 'durable' : 'safe';
    const payload = value[3];
    if (status === FAILURE) {
      if (disposition !== 'safe') {
        throw new WasmStructuredDecodeError(
          'WASM returned a durable structured failure envelope',
          disposition,
        );
      }
      if (!isBridgeErrorPayload(payload)) {
        throw invalidStructured('error', disposition);
      }
      throw new WasmBridgeError(
        payload.code,
        payload.message,
        payload.retryable,
      );
    }
    if (!isValid(payload)) {
      throw invalidStructured(label, disposition);
    }
    return payload;
  };

const invalidStructured = (
  what: string,
  disposition?: StructuredResponseDisposition,
): WasmStructuredDecodeError =>
  new WasmStructuredDecodeError(
    `WASM returned an invalid structured ${what}`,
    disposition,
  );

const decodeUnit = decoder('unit result', isUndefined);
const decodeBoolean = decoder('boolean result', isBoolean);
const decodeRevision = decoder('revision', isCount);
const decodePreparedStatementId = decoder(
  'prepared statement ID',
  (payload): payload is number => isCountWithin(payload, 1, MAX_U32),
);
const decodeApplyOutcome = decoder(
  'apply outcome',
  (payload): payload is ApplyOutcome => isRpcResult('commitTransaction', payload),
);
const decodeSqlResult = decoder(
  'SQL result',
  (payload): payload is SqlResult => isRpcResult('executeSql', payload),
);
const decodeSqlResults = decoder(
  'SQL results',
  (payload): payload is SqlResult[] => isRpcResult('execSql', payload),
);

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
