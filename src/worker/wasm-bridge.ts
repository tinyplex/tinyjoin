import {
  isRpcResult,
  type ApplyOutcome,
  type ChangeBatch,
  type JsonValue,
  type QueryPlan,
  type QueryResult,
  type Row,
  type SqlResult,
  type TableSchema,
} from '../protocol.js';
import type {WorkerEngine} from './engine.js';
import type {PageDevice} from './page-device.js';
import {
  WasmBridgeError,
  preflightApplyBatch,
  preflightClosePrepared,
  preflightDefineTables,
  preflightExecSql,
  preflightExecutePrepared,
  preflightExecuteSql,
  preflightPrepareSql,
  preflightQuery,
  preflightReplaceSnapshot,
} from './wasm-preflight.js';

export {WasmBridgeError} from './wasm-preflight.js';

export const WASM_OPERATION = {
  defineTables: 1,
  replaceSnapshot: 2,
  applyBatch: 3,
  query: 4,
  executeSql: 5,
  execSql: 6,
  begin: 7,
  commit: 8,
  rollback: 9,
  inTransaction: 10,
  revision: 11,
  close: 12,
  prepareSql: 13,
  executePrepared: 14,
  closePrepared: 15,
} as const;

const BRIDGE_VERSION = 1;
const SUCCESS = 0;
const FAILURE = 1;
const SAFE_RESPONSE = 0;
const DURABLE_RESPONSE = 1;
const MAX_U32 = 0xffff_ffff;

const arrayIsArray = Array.isArray;
const hasOwn = Object.hasOwn;
const numberIsSafeInteger = Number.isSafeInteger;

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

interface DecodedResponse<Result> {
  readonly value: Result;
  readonly disposition: StructuredResponseDisposition;
}

interface StructuredCall {
  readonly operation: number;
  readonly payload: unknown;
  readonly mayPublish: boolean;
}

function guardWasmPageDevice(device: PageDevice): PageDevice {
  return Object.freeze({
    pageCount: () => inPageDeviceCallback(() => device.pageCount()),
    readPage: (low: number, high: number, target: Uint8Array) =>
      inPageDeviceCallback(() => device.readPage(low, high, target)),
    writePage: (low: number, high: number, source: Uint8Array) =>
      inPageDeviceCallback(() => device.writePage(low, high, source)),
    flush: () => inPageDeviceCallback(() => device.flush()),
    close: () => inPageDeviceCallback(() => device.close()),
  });
}

function inPageDeviceCallback<Result>(callback: () => Result): Result {
  insidePageDeviceCallback += 1;
  try {
    return callback();
  } finally {
    insidePageDeviceCallback -= 1;
  }
}

function assertNotInPageDeviceCallback(): void {
  if (insidePageDeviceCallback !== 0) {
    throw new WasmBridgeError(
      'ENGINE_REENTRANT_CALL',
      'TinyGres cannot enter a WASM engine from a page-device callback',
      false,
    );
  }
}

/** Opens the direct structured WASM bridge behind the shared page guard. */
export function createStructuredWasmEngine(
  RawEngine: RawStructuredWasmEngineConstructor,
  device: PageDevice,
): StructuredWasmEngine {
  assertNotInPageDeviceCallback();
  try {
    return new StructuredWasmEngine(new RawEngine(guardWasmPageDevice(device)));
  } catch (error) {
    throw normalizeWasmConstructorError(error);
  }
}

/** Normalizes the direct error payload thrown while opening the raw engine. */
export function normalizeWasmConstructorError(error: unknown): unknown {
  if (!isBridgeErrorPayload(error)) {
    return error;
  }
  return new WasmBridgeError(error.code, error.message, error.retryable);
}

/** WorkerEngine adapter that passes structured values directly into WASM. */
export class StructuredWasmEngine implements WorkerEngine {
  readonly #raw: RawStructuredWasmEngine;
  #state: 'open' | 'poisoned' | 'closed' = 'open';
  #rawReleased = false;

  constructor(raw: RawStructuredWasmEngine) {
    assertNotInPageDeviceCallback();
    this.#raw = raw;
  }

  defineTables(schemas: TableSchema[]): void {
    this.#assertCallable();
    preflightDefineTables(schemas);
    this.#invoke(
      structuredCall(WASM_OPERATION.defineTables, schemas, true),
      decodeUnitResponse,
    );
  }

  replaceTableSnapshot(schema: TableSchema, rows: Row[]): ApplyOutcome {
    this.#assertCallable();
    preflightReplaceSnapshot(schema, rows);
    return this.#invoke(
      structuredCall(WASM_OPERATION.replaceSnapshot, {schema, rows}, true),
      decodeApplyOutcomeResponse,
    );
  }

  applyBatch(batch: ChangeBatch): ApplyOutcome {
    this.#assertCallable();
    preflightApplyBatch(batch);
    return this.#invoke(
      structuredCall(WASM_OPERATION.applyBatch, batch, true),
      decodeApplyOutcomeResponse,
    );
  }

  query(plan: QueryPlan): QueryResult {
    this.#assertCallable();
    preflightQuery(plan);
    return this.#invoke(
      structuredCall(WASM_OPERATION.query, plan, false),
      decodeQueryResultResponse,
    );
  }

  executeSql(sql: string, params: JsonValue[]): SqlResult {
    this.#assertCallable();
    preflightExecuteSql(sql, params);
    return this.#invoke(
      structuredCall(WASM_OPERATION.executeSql, {sql, params}, true),
      decodeSqlResultResponse,
    );
  }

  prepareSql(sql: string): number {
    this.#assertCallable();
    preflightPrepareSql(sql);
    return this.#invoke(
      structuredCall(WASM_OPERATION.prepareSql, sql, false),
      decodePreparedStatementIdResponse,
    );
  }

  executePrepared(statementId: number, params: JsonValue[]): SqlResult {
    this.#assertCallable();
    preflightExecutePrepared(statementId, params);
    return this.#invoke(
      structuredCall(
        WASM_OPERATION.executePrepared,
        {statementId, params},
        true,
      ),
      decodeSqlResultResponse,
    );
  }

  closePrepared(statementId: number): void {
    this.#assertCallable();
    preflightClosePrepared(statementId);
    this.#invoke(
      structuredCall(WASM_OPERATION.closePrepared, statementId, false),
      decodeUnitResponse,
    );
  }

  execSql(sql: string): SqlResult[] {
    this.#assertCallable();
    preflightExecSql(sql);
    return this.#invoke(
      structuredCall(WASM_OPERATION.execSql, sql, true),
      decodeSqlResultsResponse,
    );
  }

  beginTransaction(): void {
    this.#assertCallable();
    this.#invoke(
      structuredCall(WASM_OPERATION.begin, undefined, false),
      decodeUnitResponse,
    );
  }

  commitTransaction(): ApplyOutcome {
    this.#assertCallable();
    return this.#invoke(
      structuredCall(WASM_OPERATION.commit, undefined, true),
      decodeApplyOutcomeResponse,
    );
  }

  rollbackTransaction(): void {
    this.#assertCallable();
    this.#invoke(
      structuredCall(WASM_OPERATION.rollback, undefined, false),
      decodeUnitResponse,
    );
  }

  inTransaction(): boolean {
    this.#assertCallable();
    return this.#invoke(
      structuredCall(WASM_OPERATION.inTransaction, undefined, false),
      decodeBooleanResponse,
    );
  }

  revision(): number {
    this.#assertCallable();
    return this.#invoke(
      structuredCall(WASM_OPERATION.revision, undefined, false),
      decodeRevisionResponse,
    );
  }

  close(): void {
    assertNotInPageDeviceCallback();
    if (this.#state === 'closed') {
      return;
    }
    const wasOpen = this.#state === 'open';
    this.#state = 'closed';
    try {
      if (wasOpen) {
        const response = this.#raw.callStructured(
          BRIDGE_VERSION,
          WASM_OPERATION.close,
          undefined,
        );
        decodeUnitResponse(response);
      }
    } finally {
      this.#releaseRaw();
    }
  }

  #invoke<Result>(
    request: StructuredCall,
    decode: (value: unknown) => DecodedResponse<Result>,
  ): Result {
    this.#assertCallable();
    let response: unknown;
    try {
      response = this.#raw.callStructured(
        BRIDGE_VERSION,
        request.operation,
        request.payload,
      );
    } catch (error) {
      if (request.mayPublish) {
        throw this.#poisonUnknown(error);
      }
      throw error;
    }
    try {
      return decode(response).value;
    } catch (error) {
      if (
        error instanceof WasmBridgeError &&
        !(error instanceof WasmStructuredDecodeError)
      ) {
        this.#closeOnFatalRemoteError(error);
        throw error;
      }
      const disposition =
        error instanceof WasmStructuredDecodeError
          ? error.disposition
          : undefined;
      if (
        disposition === 'durable' ||
        (disposition === undefined && request.mayPublish)
      ) {
        throw this.#poisonUnknown(error);
      }
      throw error;
    }
  }

  #assertCallable(): void {
    assertNotInPageDeviceCallback();
    if (this.#state === 'poisoned') {
      throw new WasmBridgeError(
        'STORAGE_ENGINE_POISONED',
        'The TinyGres engine cannot be used after an uncertain result',
        false,
      );
    }
    if (this.#state === 'closed') {
      throw new WasmBridgeError(
        'ENGINE_CLOSED',
        'The TinyGres engine is closed',
      );
    }
  }

  #closeOnFatalRemoteError(error: WasmBridgeError): void {
    if (
      error.code === 'RECOVERY_REQUIRED' ||
      error.code === 'STORAGE_COMMIT_OUTCOME_UNKNOWN' ||
      error.code === 'STORAGE_ENGINE_POISONED'
    ) {
      this.#closeRaw();
    }
  }

  #poisonUnknown(error: unknown): WasmBridgeError {
    this.#closeRaw();
    const detail =
      error instanceof Error && error.message ? `: ${error.message}` : '';
    return new WasmBridgeError(
      'STORAGE_COMMIT_OUTCOME_UNKNOWN',
      `TinyGres could not decode a result after a possible durable mutation${detail}`,
      false,
    );
  }

  #closeRaw(): void {
    if (this.#state !== 'open') {
      return;
    }
    this.#state = 'poisoned';
    try {
      this.#raw.callStructured(BRIDGE_VERSION, WASM_OPERATION.close, undefined);
    } catch {
      // The first uncertain/fatal result remains decisive.
    }
    this.#releaseRaw();
  }

  #releaseRaw(): void {
    if (this.#rawReleased) {
      return;
    }
    this.#rawReleased = true;
    try {
      this.#raw.free?.();
    } catch {
      // The first uncertain/fatal/close result remains decisive.
    }
  }
}

function structuredCall(
  operation: number,
  payload: unknown,
  mayPublish: boolean,
): StructuredCall {
  return {operation, payload, mayPublish};
}

function decodeUnitResponse(value: unknown): DecodedResponse<void> {
  return decodeResponse(value, 'unit result', (payload) =>
    payload === undefined ? undefined : INVALID_RESULT,
  );
}

function decodeBooleanResponse(value: unknown): DecodedResponse<boolean> {
  return decodeResponse(value, 'boolean result', (payload) =>
    typeof payload === 'boolean' ? payload : INVALID_RESULT,
  );
}

function decodeRevisionResponse(value: unknown): DecodedResponse<number> {
  return decodeResponse(value, 'revision', (payload) =>
    numberIsSafeInteger(payload) && Number(payload) >= 0
      ? Number(payload)
      : INVALID_RESULT,
  );
}

function decodePreparedStatementIdResponse(
  value: unknown,
): DecodedResponse<number> {
  return decodeResponse(value, 'prepared statement ID', (payload) =>
    numberIsSafeInteger(payload) &&
    Number(payload) > 0 &&
    Number(payload) <= MAX_U32
      ? Number(payload)
      : INVALID_RESULT,
  );
}

function decodeApplyOutcomeResponse(
  value: unknown,
): DecodedResponse<ApplyOutcome> {
  return decodeResponse(value, 'apply outcome', (payload) =>
    isRpcResult('applyBatch', payload) ? payload : INVALID_RESULT,
  );
}

function decodeQueryResultResponse(
  value: unknown,
): DecodedResponse<QueryResult> {
  return decodeResponse(value, 'query result', (payload) =>
    isRpcResult('query', payload) ? payload : INVALID_RESULT,
  );
}

function decodeSqlResultResponse(value: unknown): DecodedResponse<SqlResult> {
  return decodeResponse(value, 'SQL result', (payload) =>
    isRpcResult('executeSql', payload) ? payload : INVALID_RESULT,
  );
}

function decodeSqlResultsResponse(
  value: unknown,
): DecodedResponse<SqlResult[]> {
  return decodeResponse(value, 'SQL results', (payload) =>
    isRpcResult('execSql', payload) ? payload : INVALID_RESULT,
  );
}

const INVALID_RESULT = Symbol('invalid structured result');

function decodeResponse<Result>(
  value: unknown,
  label: string,
  decode: (payload: unknown) => Result | typeof INVALID_RESULT,
): DecodedResponse<Result> {
  if (!isDenseEnvelope(value)) {
    throw new WasmStructuredDecodeError(
      'WASM returned an invalid structured response envelope',
    );
  }
  if (value[0] !== BRIDGE_VERSION) {
    throw new WasmStructuredDecodeError(
      'WASM returned an unsupported structured bridge version',
    );
  }
  const status = value[1];
  if (status !== SUCCESS && status !== FAILURE) {
    throw new WasmStructuredDecodeError(
      'WASM returned an invalid structured response status',
    );
  }
  const dispositionTag = value[2];
  if (dispositionTag !== SAFE_RESPONSE && dispositionTag !== DURABLE_RESPONSE) {
    throw new WasmStructuredDecodeError(
      'WASM returned an invalid structured response disposition',
    );
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
      throw new WasmStructuredDecodeError(
        'WASM returned an invalid structured error',
        disposition,
      );
    }
    throw new WasmBridgeError(payload.code, payload.message, payload.retryable);
  }

  const result = decode(payload);
  if (result === INVALID_RESULT) {
    throw new WasmStructuredDecodeError(
      `WASM returned an invalid structured ${label}`,
      disposition,
    );
  }
  return {value: result, disposition};
}

function isDenseEnvelope(value: unknown): value is unknown[] {
  return (
    arrayIsArray(value) &&
    value.length === 4 &&
    hasOwn(value, 0) &&
    hasOwn(value, 1) &&
    hasOwn(value, 2) &&
    hasOwn(value, 3)
  );
}

interface BridgeErrorPayload {
  readonly code: string;
  readonly message: string;
  readonly retryable?: boolean;
}

function isBridgeErrorPayload(value: unknown): value is BridgeErrorPayload {
  if (typeof value !== 'object' || value === null || arrayIsArray(value)) {
    return false;
  }
  let keys: (string | symbol)[];
  try {
    keys = Reflect.ownKeys(value);
  } catch {
    return false;
  }
  if (
    keys.length < 2 ||
    keys.length > 3 ||
    !keys.every(
      (key) => key === 'code' || key === 'message' || key === 'retryable',
    )
  ) {
    return false;
  }
  const record = value as Record<string, unknown>;
  return (
    hasOwn(record, 'code') &&
    hasOwn(record, 'message') &&
    typeof record.code === 'string' &&
    typeof record.message === 'string' &&
    (!hasOwn(record, 'retryable') || typeof record.retryable === 'boolean')
  );
}
