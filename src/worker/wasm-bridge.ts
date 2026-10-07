import {
  arrayIsArray,
  errorDetail,
  isBoolean,
  isCount,
  isCountWithin,
  isRecord,
  isString,
  isUndefined,
  MAX_CHANGED_KEYS_PER_TABLE,
  MAX_U32,
  objFreeze,
  objHasOwn,
  ownKeys,
  type CodedError,
} from '../common.js';
import {
  isApplyOutcome,
  isRpcResultHeader,
  STATEMENT_SELECT,
  type ApplyOutcome,
  type JsonPrimitive,
  type JsonValue,
  type RowMode,
  type Schema,
  type SqlResult,
  type StatementResult,
} from '../protocol.js';
import type {WorkerEngine} from './engine.js';
import type {PageDevice} from './page-device.js';
import {
  EMPTY_REQUEST,
  JSON_F64,
  JSON_FALSE,
  JSON_I64,
  JSON_NULL,
  JSON_STRING,
  JSON_TRUE,
  WasmBridgeError,
  encodeClosePrepared,
  encodeExecSql,
  encodePrepareSql,
  encodeSetSchema,
  encodeStatement,
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

const BRIDGE_VERSION = 6;
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
  /**
   * Executes one request, and returns its response: JSON text, with one
   * exception, which an engine makes once it has been asked where its header
   * is. The result of a statement that published nothing, where it has the
   * shape of a statement's response, is written instead as a header in the
   * module's memory, and the call returns `true`, or for a `SELECT` the
   * result's fields and rows alone as JSON text. A header's first byte is
   * zero unless the call that just returned wrote one.
   */
  callStructured(
    bridgeVersion: number,
    operation: number,
    request: Uint8Array,
  ): unknown;
  free?(): void;
  /**
   * The module's memory, and where in it the engine writes a statement
   * result's header. Asking where is what has the engine write one at all,
   * and the bridge asks only an engine whose memory it can read. So an engine
   * that has neither, as a test's stand-in may not, answers every statement
   * in JSON text, and so does the real one behind something that passes on
   * its calls and nothing else.
   */
  memory?(): {readonly buffer: ArrayBufferLike};
  resultHeader?(): number;
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
  // Where the engine writes a statement result's header, asked once, and the
  // module's memory as bytes and as numbers. The engine is asked only where
  // there is a memory to read its header in, since from then on it answers
  // with headers. Both views are made when a header is first read, and again
  // whenever the memory has grown, which leaves its old buffer detached and
  // every view of it empty.
  const memory = raw.memory?.();
  const headerAt = (memory && raw.resultHeader?.()) ?? -1;
  let bytes: Uint8Array = EMPTY_REQUEST;
  let view: DataView = EMPTY_VIEW;
  // Where in the memory a header is being read.
  let cursor = 0;

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
    // Every call but a rare one may be made, which two comparisons tell.
    if (insidePageDeviceCallback !== 0 || state !== 'open') {
      assertNotInPageDeviceCallback();
      throw state === 'poisoned'
        ? new WasmBridgeError(
            'STORAGE_ENGINE_POISONED',
            'The TinyJoin engine cannot be used after an uncertain result',
            false,
          )
        : new WasmBridgeError('ENGINE_CLOSED', 'The TinyJoin engine is closed');
    }
  };

  // V8 optimizes a function once its calls have run through some multiple of
  // its length, and counts each call by how far into the function it returns.
  // So in what follows, which every statement runs, a call that succeeds
  // returns at its function's end, after whatever handles one that fails,
  // rather than from where its result was made. Returned from there, a call
  // counts for a fraction of the function, which then stays unoptimized that
  // much further into a page's first statements: three times as far, for the
  // function below.

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
      response = raw.callStructured(BRIDGE_VERSION, operation, request);
    } catch (error) {
      throw mayPublish ? poisonUnknown(error) : error;
    }
    let result: Result;
    try {
      result = decode(response);
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
    return result;
  };

  // Reads a text of a header: its length in bytes, and then its UTF-8 bytes.
  // The length's top bit is clear for a text of at most 64 bytes, all ASCII,
  // which is read as character codes, since a decoder costs more than such a
  // text does: one of up to three units here, outright, and a longer one by
  // the reader below. A text with the bit set is decoded, and refused unless
  // it is UTF-8.
  const readText = (): string => {
    const length = bytes[cursor]! | (bytes[cursor + 1]! << 8);
    const at = cursor + 2;
    cursor = at + (length & (DECODED_TEXT - 1));
    return length > 3
      ? length < DECODED_TEXT
        ? readPlainText(at, length)
        : textDecoder.decode(bytes.subarray(at, cursor))
      : length === 1
        ? fromCharCode(bytes[at]!)
        : length === 2
          ? fromCharCode(bytes[at]!, bytes[at + 1]!)
          : length === 3
            ? fromCharCode(bytes[at]!, bytes[at + 1]!, bytes[at + 2]!)
            : '';
  };

  // Reads a plain text of four units or more, eight units to a call. Once
  // this is optimized, as it is within a page's first few hundred statements,
  // making a string of the units a call is given costs less than making a
  // view of the text's bytes to give one call them all, however long the
  // text, and far less than a call for each. Until then a text of more than
  // eight units costs more this way, which those first statements pay for
  // all that follow them. What the eights leave, which is one to eight
  // units, is found in three tests whatever its length, and is all there is
  // of most names.
  //
  // It is a reader apart, so that the one above stays as short as the names
  // it reads: as one function, the two were optimized several times later
  // for a table and a key column of a few letters, whose reading returned
  // from near its top. It is written below the reader that calls it, because
  // the build puts a function with one caller in that caller's place, to be
  // made anew at every call, unless it is written after it.
  const readPlainText = (at: number, length: number): string => {
    let text = '';
    let rest = length;
    for (; rest > 8; rest -= 8, at += 8) {
      text += fromCharCode(
        bytes[at]!,
        bytes[at + 1]!,
        bytes[at + 2]!,
        bytes[at + 3]!,
        bytes[at + 4]!,
        bytes[at + 5]!,
        bytes[at + 6]!,
        bytes[at + 7]!,
      );
    }
    const last =
      rest > 4
        ? rest > 6
          ? rest === 7
            ? fromCharCode(
                bytes[at]!,
                bytes[at + 1]!,
                bytes[at + 2]!,
                bytes[at + 3]!,
                bytes[at + 4]!,
                bytes[at + 5]!,
                bytes[at + 6]!,
              )
            : fromCharCode(
                bytes[at]!,
                bytes[at + 1]!,
                bytes[at + 2]!,
                bytes[at + 3]!,
                bytes[at + 4]!,
                bytes[at + 5]!,
                bytes[at + 6]!,
                bytes[at + 7]!,
              )
          : rest === 5
            ? fromCharCode(
                bytes[at]!,
                bytes[at + 1]!,
                bytes[at + 2]!,
                bytes[at + 3]!,
                bytes[at + 4]!,
              )
            : fromCharCode(
                bytes[at]!,
                bytes[at + 1]!,
                bytes[at + 2]!,
                bytes[at + 3]!,
                bytes[at + 4]!,
                bytes[at + 5]!,
              )
        : rest > 2
          ? rest === 3
            ? fromCharCode(bytes[at]!, bytes[at + 1]!, bytes[at + 2]!)
            : fromCharCode(
                bytes[at]!,
                bytes[at + 1]!,
                bytes[at + 2]!,
                bytes[at + 3]!,
              )
          : rest === 2
            ? fromCharCode(bytes[at]!, bytes[at + 1]!)
            : fromCharCode(bytes[at]!);
    return length > 8 ? text + last : last;
  };

  // Reads one value of a key, tagged as a request's values are, or returns
  // `undefined` for one that is not as the engine writes it.
  const readValue = (): JsonPrimitive | undefined => {
    const tag = bytes[cursor++];
    let value: JsonPrimitive | undefined;
    if (tag === JSON_I64 || tag === JSON_F64) {
      value = view.getFloat64(cursor, true);
      cursor += 8;
      // A key is finite, and only the difference of a finite number from
      // itself is zero.
      if (value - value !== 0) {
        value = undefined;
      }
    } else {
      value =
        tag === JSON_STRING
          ? readText()
          : tag === JSON_NULL
            ? null
            : tag === JSON_TRUE
              ? true
              : tag === JSON_FALSE
                ? false
                : undefined;
    }
    return value;
  };

  /**
   * Reads a statement's result. One that published nothing, and has the shape
   * of a statement's response, is in the header the engine wrote, and becomes
   * that response as it stands, with no text to parse. Any other is the JSON
   * text of a result, which is read as every other response is.
   *
   * A header only ever follows a call that published nothing, so one that
   * cannot be read fails its statement, as a result in text that cannot be
   * read does, and leaves the engine as it was.
   */
  const decodeStatement = (response: unknown): SqlResult | StatementResult => {
    let result: SqlResult | StatementResult | undefined;
    if (headerAt >= 0) {
      let size = bytes.byteLength;
      if (size === 0) {
        bytes = new Uint8Array(memory!.buffer);
        view = new DataView(memory!.buffer);
        size = bytes.byteLength;
      }
      const kind = bytes[headerAt];
      if (kind) {
        // The header is read here, rather than by a function of its own, which
        // the build would make anew for every statement. Each form the engine
        // writes leaves the response it stands for. Anything else leaves
        // none, as does a read past the end of the memory, or text that is
        // not UTF-8, which is thrown.
        try {
          const width = bytes[headerAt + 1]!;
          const count = bytes[headerAt + 2]! | (bytes[headerAt + 3]! << 8);
          const end =
            headerAt + (bytes[headerAt + 4]! | (bytes[headerAt + 5]! << 8));
          // Each is a count whatever its bits, as the page requires of a response's.
          const revision = view.getUint32(headerAt + 8, true);
          const rowCount = view.getUint32(headerAt + 12, true);
          cursor = headerAt + HEADER_FIXED_BYTES;
          if (kind === SELECT_HEADER) {
            // A read's header is its fixed bytes, and its call returns its rows.
            if (
              typeof response === 'string' &&
              end === cursor &&
              width === 0 &&
              count === 0
            ) {
              result = [0, 0, STATEMENT_SELECT, revision, rowCount, response];
            }
          } else if (
            response === true &&
            kind < SELECT_HEADER &&
            // A header lies wholly in the memory. The bytes of a text are
            // read without asking whether they are there, and past the
            // memory's end there are none to be found wrong.
            end <= size
          ) {
            // A write's call returns `true`. One that changed no table ends
            // with its fixed bytes, one that changed a table whose keys are
            // not reported ends with the table's name, and any other goes on
            // with the names of its key's columns and then each key's values.
            if (end === cursor) {
              if (width === 0 && count === 0) {
                result = [0, 0, kind - 1, revision, rowCount];
              }
            } else {
              const table = readText();
              if (width === 0) {
                if (count === 0 && cursor === end) {
                  result = [0, 0, kind - 1, revision, rowCount, table];
                }
              } else if (width === 1 && count === 1) {
                // One key of one column, as a statement by key reports: its
                // response is made whole, which costs less than one grown a
                // slot at a time.
                const column = readText();
                const value = readValue();
                if (value !== undefined && cursor === end) {
                  result = [
                    0, 0, kind - 1, revision, rowCount, table, 1, column, value,
                  ];
                }
              } else if (count <= MAX_CHANGED_KEYS_PER_TABLE) {
                const keyed: StatementResult = [
                  0, 0, kind - 1, revision, rowCount, table, width,
                ];
                for (let column = 0; column < width; column++) {
                  keyed.push(readText());
                }
                let values = count * width;
                for (; values > 0; values--) {
                  const value = readValue();
                  if (value === undefined) {
                    break;
                  }
                  keyed.push(value);
                }
                if (values === 0 && cursor === end) {
                  result = keyed;
                }
              }
            }
          }
        } catch {
          // Left without a response, to the failure below.
        }
        if (!result) {
          throw invalidStructured(SQL_RESULT, 'safe');
        }
      }
    }
    return result ?? decodeSqlResult(response);
  };

  // Every operation asserts that the engine is still callable, writes its
  // request, and then invokes. A statement first makes for itself the test
  // the assertion begins with, which nearly every statement passes, and which
  // costs less where it stands than the call to have it made.
  return objFreeze({
    executeSql: (
      sql: string,
      params: readonly JsonValue[],
      rowMode?: RowMode,
      from?: number,
    ): SqlResult | StatementResult => {
      if (insidePageDeviceCallback !== 0 || state !== 'open') {
        assertCallable();
      }
      return invoke(
        WASM_OPERATION.executeSql,
        encodeStatement(false, sql, params, rowMode === 'array', from),
        true,
        decodeStatement,
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
      params: readonly JsonValue[],
      rowMode?: RowMode,
      from?: number,
    ): SqlResult | StatementResult => {
      if (insidePageDeviceCallback !== 0 || state !== 'open') {
        assertCallable();
      }
      return invoke(
        WASM_OPERATION.executePrepared,
        encodeStatement(true, statementId, params, rowMode === 'array', from),
        true,
        decodeStatement,
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
// What a statement's result is called when it cannot be read, in a header or
// in text.
const SQL_RESULT = 'SQL result';

// A statement result's header: the bytes every one has, the first byte of a
// read's, which is its command's number and one more as every header's is, and
// what in a text's length says that the text is to be decoded.
const HEADER_FIXED_BYTES = 16;
const SELECT_HEADER = STATEMENT_SELECT + 1;
const DECODED_TEXT = 0x8000;
const EMPTY_VIEW = new DataView(new ArrayBuffer(0));
const fromCharCode = String.fromCharCode;
// A byte order mark is a character of a name like any other.
const textDecoder = new TextDecoder('utf-8', {fatal: true, ignoreBOM: true});

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
const decodeApplyOutcome = decoder('apply outcome', single(isApplyOutcome));
// A statement's result in JSON text: one that published, whose changes the
// Worker announces, or one that has not the shape of a statement's response.
const decodeSqlResult = decoder(SQL_RESULT, (payload, rest) => {
  const result = withData(payload, rest);
  return isRpcResultHeader('executeSql', result) ? result : INVALID;
});
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
