import {describe, expect, it} from 'vitest';

import {
  PROTOCOL_VERSION,
  isStatementResponse,
  type JsonPrimitive,
  type JsonValue,
  type Row,
  type Schema,
  type SqlResult,
} from '../../src/protocol.ts';
import type {PageDevice} from '../../src/worker/page-device.ts';
import {
  WASM_OPERATION,
  WasmStructuredDecodeError,
  adaptStructuredWasmEngine,
  createStructuredWasmEngine,
  normalizeWasmConstructorError,
  type RawStructuredWasmEngine,
  type RawStructuredWasmEngineConstructor,
} from '../../src/worker/wasm-bridge.ts';
import {decodeRequest} from '../helpers/wasm-request.ts';
import {
  decodeStatementHeader,
  encodeStatementHeader,
  statementResult,
  type StatementHeader,
} from '../helpers/wasm-response.ts';

const VERSION = 6;
const SUCCESS = 0;
const FAILURE = 1;
const SAFE = 0;
const DURABLE = 1;

type RawCall = {
  bridgeVersion: number;
  operation: number;
  payload: unknown;
};

// A response is the JSON text of its envelope, and then a line for each
// statement result's fields and rows.
function envelope(
  version: number,
  status: number,
  disposition: number,
  payload: unknown,
  ...lines: string[]
): string {
  return [JSON.stringify([version, status, disposition, payload]), ...lines].join(
    '\n',
  );
}

function success(payload: unknown = null, disposition = SAFE): string {
  return envelope(VERSION, SUCCESS, disposition, payload);
}

function failure(code: string, message: string, retryable?: boolean): string {
  return envelope(VERSION, FAILURE, SAFE, {
    code,
    message,
    ...(retryable === undefined ? {} : {retryable}),
  });
}

function outcome(revision = 1, tables: string[] = ['items']) {
  return {revision, tables, keys: {}};
}

function sqlResult(rows: Row[] = []): SqlResult {
  return {
    command: 'SELECT',
    revision: 1,
    rowCount: rows.length,
    tables: [],
    keys: {},
    data: JSON.stringify({
      fields: rows.length > 0 ? [{name: 'id', dataTypeID: 20}] : [],
      rows,
    }),
  };
}

function sqlResponse(results: SqlResult[], list = false, disposition = SAFE) {
  const headers = results.map(({data: _data, ...header}) => header);
  return envelope(
    VERSION,
    SUCCESS,
    disposition,
    list ? headers : headers[0],
    ...results.map((result) => result.data),
  );
}

class FakeRawEngine implements RawStructuredWasmEngine {
  readonly calls: RawCall[] = [];
  closeCalls = 0;
  freeCalls = 0;
  response: unknown | Error = success();
  responseFor: ((call: RawCall) => unknown) | undefined;

  callStructured(
    bridgeVersion: number,
    operation: number,
    request: Uint8Array,
  ): unknown {
    // The request's bytes are reused for the next, so they are read at once.
    const call = {bridgeVersion, operation, payload: decodeRequest(operation, request)};
    this.calls.push(call);
    if (operation === WASM_OPERATION.close) {
      this.closeCalls += 1;
    }
    if (this.response instanceof Error) {
      throw this.response;
    }
    return this.responseFor?.(call) ?? this.response;
  }

  free(): void {
    this.freeCalls += 1;
  }
}

// The address a stand-in engine gives its header: any that leaves room after it.
const HEADER_AT = 4096;

/**
 * A raw engine with a memory, which answers a statement as the real one does:
 * with a header written into the memory, and `true` or a read's rows beside
 * it. Every call clears the header's first byte before anything else.
 */
class HeaderRawEngine extends FakeRawEngine {
  readonly wasmMemory = new WebAssembly.Memory({initial: 1, maximum: 8});
  headerAt = HEADER_AT;
  /** The bytes the next statements' calls leave at the header, if any. */
  header: Uint8Array | undefined;
  /** Whether every call leaves them there, and not only a statement's. */
  afterEveryCall = false;
  /** Runs inside each call, before its header is written. */
  duringCall: (() => void) | undefined;
  memoryCalls = 0;
  headerCalls = 0;

  memory(): WebAssembly.Memory {
    this.memoryCalls += 1;
    return this.wasmMemory;
  }

  resultHeader(): number {
    this.headerCalls += 1;
    return this.headerAt;
  }

  override callStructured(
    bridgeVersion: number,
    operation: number,
    request: Uint8Array,
  ): unknown {
    new Uint8Array(this.wasmMemory.buffer)[this.headerAt] = 0;
    const response = super.callStructured(bridgeVersion, operation, request);
    this.duringCall?.();
    if (
      this.header &&
      (this.afterEveryCall ||
        operation === WASM_OPERATION.executeSql ||
        operation === WASM_OPERATION.executePrepared)
    ) {
      new Uint8Array(this.wasmMemory.buffer).set(this.header, this.headerAt);
    }
    return response;
  }

  /** Answers the next statements with `header`, and `true` or `data` beside it. */
  answer(header: StatementHeader, data?: string): void {
    this.header = encodeStatementHeader(header);
    this.response = data ?? true;
  }
}

// A response as the page checks one that a Worker of its own posted: with
// its version and the id of its request in place.
function posted(result: SqlResult | JsonPrimitive[]): JsonPrimitive[] {
  expect(Array.isArray(result)).toBe(true);
  const response = [...(result as JsonPrimitive[])];
  expect(response.slice(0, 2)).toEqual([0, 0]);
  response[0] = PROTOCOL_VERSION;
  response[1] = 1;
  return response;
}

describe('WASM engine bridge', () => {
  it('pins the private operation ABI densely', () => {
    expect(WASM_OPERATION).toEqual({
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
    });
  });

  it('normalizes the direct constructor error payload', () => {
    const ThrowingRaw = class {
      constructor(_device: PageDevice) {
        throw JSON.stringify({
          code: 'STORAGE_LOCKED',
          message: 'locked',
          retryable: true,
        });
      }
    } as unknown as RawStructuredWasmEngineConstructor;

    expect(() =>
      createStructuredWasmEngine(ThrowingRaw, new CallbackPageDevice()),
    ).toThrow(
      expect.objectContaining({
        code: 'STORAGE_LOCKED',
        message: 'locked',
        retryable: true,
      }),
    );

    for (const malformed of [
      JSON.stringify({
        code: 'STORAGE_LOCKED',
        message: 'locked',
        details: 'not part of the constructor ABI',
      }),
      'not JSON',
      {code: 'STORAGE_LOCKED', message: 'an object, not its text'},
    ]) {
      expect(normalizeWasmConstructorError(malformed)).toBe(malformed);
    }
  });
  it('passes the exact structured payload shapes through every operation', () => {
    const raw = new FakeRawEngine();
    raw.responseFor = ({operation}) => responseForOperation(operation);
    const engine = adaptStructuredWasmEngine(raw);

    const params = [3, 'three'];
    const preparedParams = [4];

    expect(engine.executeSql('SELECT $1, $2', params)).toEqual(
      sqlResult([{id: 1}]),
    );
    expect(engine.executeSql('SELECT $1', [5], 'array')).toEqual(
      sqlResult([{id: 1}]),
    );
    expect(engine.execSql('SELECT 1; SELECT 2')).toEqual([
      sqlResult([{id: 1}]),
      sqlResult(),
    ]);
    expect(engine.execSql('SELECT 3', 'object')).toEqual([
      sqlResult([{id: 1}]),
      sqlResult(),
    ]);
    engine.beginTransaction();
    expect(engine.commitTransaction()).toEqual(outcome());
    engine.rollbackTransaction();
    expect(engine.inTransaction()).toBe(false);
    expect(engine.revision()).toBe(3);
    expect(engine.prepareSql('SELECT $1')).toBe(9);
    expect(engine.executePrepared(9, preparedParams)).toEqual(
      sqlResult([{id: 1}]),
    );
    engine.closePrepared(9);
    engine.close();

    expect(raw.calls).toEqual([
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.executeSql,
        payload: {sql: 'SELECT $1, $2', params},
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.executeSql,
        payload: {sql: 'SELECT $1', params: [5], arrayRows: true},
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.execSql,
        payload: {sql: 'SELECT 1; SELECT 2'},
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.execSql,
        payload: {sql: 'SELECT 3'},
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.begin,
        payload: undefined,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.commit,
        payload: undefined,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.rollback,
        payload: undefined,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.inTransaction,
        payload: undefined,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.revision,
        payload: undefined,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.prepareSql,
        payload: 'SELECT $1',
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.executePrepared,
        payload: {statementId: 9, params: preparedParams},
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.closePrepared,
        payload: 9,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.close,
        payload: undefined,
      },
    ]);
    expect(raw.freeCalls).toBe(1);

    // Closed, it runs no statement of either kind, and nothing more is asked
    // of WASM.
    const calls = raw.calls.length;
    for (const run of [
      () => engine.executeSql('SELECT $1', params),
      () => engine.executePrepared(9, preparedParams),
    ]) {
      expect(run).toThrow(expect.objectContaining({code: 'ENGINE_CLOSED'}));
    }
    expect(raw.calls).toHaveLength(calls);
  });

  it('reads a result in JSON text, and passes its rows on as the text WASM wrote', () => {
    const raw = new FakeRawEngine();
    const data = '{"fields":[{"name":"id","dataTypeID":20}],"rows":[{"id":1}]}';
    // Two tables changed, of which one reports its keys: more than a
    // statement's response holds.
    const header = {
      command: 'DELETE',
      revision: 1,
      rowCount: 1,
      tables: ['a', 'b'],
      keys: {a: [{id: 1}]},
    };
    const engine = adaptStructuredWasmEngine(raw);

    // A result that published nothing, but has not the shape of a statement's
    // response, is read as one that published is, and so is every result of
    // an engine that writes no header.
    for (const disposition of [SAFE, DURABLE]) {
      raw.response = envelope(VERSION, SUCCESS, disposition, header, data);
      expect(engine.executeSql('DELETE FROM a RETURNING id', [])).toEqual({
        ...header,
        data,
      });
      expect(engine.executePrepared(3, [])).toEqual({...header, data});
    }
    // The rows are passed on unread: they are the engine's own, which only
    // the page parses.
    raw.response = envelope(VERSION, SUCCESS, SAFE, header, 'not rows');
    expect(engine.executeSql('DELETE FROM a RETURNING id', [])).toEqual({
      ...header,
      data: 'not rows',
    });
    expect(raw.closeCalls).toBe(0);
  });

  it('writes the parameters of a statement from where they begin', () => {
    const raw = new FakeRawEngine();
    raw.response = sqlResponse([sqlResult()]);
    const engine = adaptStructuredWasmEngine(raw);
    // A statement request's parameters follow its fixed slots.
    const request: JsonValue[] = [PROTOCOL_VERSION, 7, 4, 9, 'tx-1', 1, 'a', 2, null];
    engine.executePrepared(9, request, 'array', 6);
    engine.executeSql('SELECT $1', request, undefined, 6);
    engine.executeSql('SELECT 1', request, 'object', request.length);
    engine.executePrepared(9, request, undefined, 0);
    expect(raw.calls.map(({payload}) => payload)).toEqual([
      {statementId: 9, params: ['a', 2, null], arrayRows: true},
      {sql: 'SELECT $1', params: ['a', 2, null]},
      {sql: 'SELECT 1', params: []},
      {statementId: 9, params: request},
    ]);
  });

  it('reads a statement result that published nothing from the header WASM wrote', () => {
    const raw = new HeaderRawEngine();
    const engine = adaptStructuredWasmEngine(raw);
    // The memory and the header's place are asked once, as the engine opens.
    expect([raw.memoryCalls, raw.headerCalls]).toEqual([1, 1]);

    const long = 'long name '.repeat(10);
    const headers: [StatementHeader, JsonPrimitive[]][] = [
      // A write that changed nothing.
      [{command: 'UPDATE', revision: 7, rowCount: 0}, [0, 0, 1, 7, 0]],
      [{command: 'INSERT', revision: 0, rowCount: 0}, [0, 0, 0, 0, 0]],
      // The greatest revision and row count a header holds.
      [
        {command: 'DELETE', revision: 0xffff_ffff, rowCount: 0xffff_ffff},
        [0, 0, 2, 0xffff_ffff, 0xffff_ffff],
      ],
      // A table whose keys are not reported, and one that reports none.
      [
        {command: 'DELETE', revision: 7, rowCount: 1500, table: 'items'},
        [0, 0, 2, 7, 1500, 'items'],
      ],
      [
        {command: 'UPDATE', revision: 7, rowCount: 0, table: 'items', columns: ['id'], keys: []},
        [0, 0, 1, 7, 0, 'items', 1, 'id'],
      ],
      // A key of one column, as a statement by key reports.
      [
        {command: 'INSERT', revision: 7, rowCount: 1, table: 't', columns: ['id'], keys: [[42]]},
        [0, 0, 0, 7, 1, 't', 1, 'id', 42],
      ],
      // Names of every length and kind: read a unit at a time, in one call,
      // and decoded.
      [
        {
          command: 'UPDATE',
          revision: 8,
          rowCount: 2,
          table: 'naïve café',
          columns: ['', 'abc', 'abcd', 'x'.repeat(64), 'y'.repeat(65), long, 'é😀', '\ufeffmarked'],
          keys: [
            [-3, 'x"y', null, -0, 'é😀', true, 1.5, ''],
            [Number.MIN_SAFE_INTEGER, long, false, 5e-324, '\ufeff', 'abc', -1e300, '\u0000\u007f'],
          ],
        },
        [
          0, 0, 1, 8, 2, 'naïve café', 8,
          '', 'abc', 'abcd', 'x'.repeat(64), 'y'.repeat(65), long, 'é😀', '\ufeffmarked',
          -3, 'x"y', null, -0, 'é😀', true, 1.5, '',
          Number.MIN_SAFE_INTEGER, long, false, 5e-324, '\ufeff', 'abc', -1e300, '\u0000\u007f',
        ],
      ],
      // As many keys as a table reports.
      [
        {
          command: 'INSERT',
          revision: 9,
          rowCount: 1000,
          table: 'items',
          columns: ['id'],
          keys: Array.from({length: 1000}, (_, id) => [id]),
        },
        [0, 0, 0, 9, 1000, 'items', 1, 'id', ...Array.from({length: 1000}, (_, id) => id)],
      ],
    ];
    for (const [header, expected] of headers) {
      raw.answer(header);
      // The stand-in's header is one the engine could have written.
      expect(decodeStatementHeader(raw.header!)).toEqual(header);
      expect(statementResult(header)).toEqual(expected);
      for (const result of [
        engine.executeSql('UPDATE items SET id = id', []),
        engine.executePrepared(3, []),
      ]) {
        expect(result).toEqual(expected);
        // Equal to the last bit, which toEqual does not tell of a zero.
        expect(
          (result as JsonPrimitive[]).every((value, at) => Object.is(value, expected[at])),
        ).toBe(true);
        // What the page's full check of a response accepts.
        expect(isStatementResponse(posted(result), true)).toBe(true);
      }
    }

    // A plain text of every length, as a table's name, as a column's, and as
    // a key's value: the shortest are read outright, the rest eight units to
    // a call with what is left over read by its length, and one past the
    // longest plain text is decoded. No two units of a text are the same, so
    // that one read out of its place shows.
    const letters =
      'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_~!@#$';
    for (let length = 0; length <= 66; length++) {
      const forward = letters.slice(0, length);
      const backward = [...forward].reverse().join('');
      const header: StatementHeader = {
        command: 'UPDATE',
        revision: 7,
        rowCount: 2,
        table: forward,
        columns: [backward, forward],
        keys: [
          [forward, length],
          [backward, forward],
        ],
      };
      raw.answer(header);
      expect(decodeStatementHeader(raw.header!)).toEqual(header);
      const expected = [
        0, 0, 1, 7, 2, forward, 2, backward, forward, forward, length, backward, forward,
      ];
      expect(engine.executePrepared(3, []), `${length}`).toEqual(expected);
      // The same, with a single key of one column, which is read apart.
      const single: StatementHeader = {
        command: 'DELETE',
        revision: 7,
        rowCount: 1,
        table: backward,
        columns: [forward],
        keys: [[backward]],
      };
      raw.answer(single);
      expect(engine.executeSql('DELETE FROM t', []), `${length}`).toEqual([
        0, 0, 2, 7, 1, backward, 1, forward, backward,
      ]);
    }

    // A read's header says its command, revision and row count, and its call
    // returns its fields and rows, which pass on as the text they are.
    const data = '{"fields":[{"name":"id","dataTypeID":20}],"rows":[{"id":1},{"id":2}]}';
    raw.answer({command: 'SELECT', revision: 7, rowCount: 2}, data);
    expect(engine.executeSql('SELECT id FROM items', [])).toEqual([0, 0, 3, 7, 2, data]);
    raw.answer({command: 'SELECT', revision: 7, rowCount: 0}, 'not rows');
    const read = engine.executePrepared(3, []);
    expect(read).toEqual([0, 0, 3, 7, 0, 'not rows']);
    expect(isStatementResponse(posted(read), true)).toBe(true);
    expect(raw.closeCalls).toBe(0);
    expect([raw.memoryCalls, raw.headerCalls]).toEqual([1, 1]);
  });

  it('reads generated headers as the responses they stand for', () => {
    const raw = new HeaderRawEngine();
    const engine = adaptStructuredWasmEngine(raw);
    // Fixed integer arithmetic makes the generated headers the same on every run.
    let state = 0xbead;
    const below = (bound: number): number => {
      state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
      return Math.floor((state / 0x1_0000_0000) * bound);
    };
    // Texts of every length around those a text is read differently at, with
    // and without characters that are not ASCII.
    const text = (): string => {
      const length =
        below(3) === 0
          ? [24, 32, 33, 36, 40, 48, 56, 57, 63, 64, 65, 200][below(12)]!
          : below(18);
      const plain = Array.from({length}, () =>
        String.fromCharCode(below(20) === 0 ? below(0x80) : 0x61 + below(26)),
      ).join('');
      switch (below(6)) {
        case 0:
          return `${plain}é`;
        case 1:
          return `\ufeff${plain}`;
        case 2:
          return `😀${plain}`;
        default:
          return plain;
      }
    };
    const key = (): JsonPrimitive => {
      switch (below(8)) {
        case 0:
          return null;
        case 1:
          return below(2) === 0;
        case 2:
          return [-0, 0.5, -1e300, 5e-324, 2 ** 53, -(2 ** 53) - 2][below(6)]!;
        case 3:
        case 4:
          return text();
        default:
          return below(2) === 0 ? below(1000) : -below(2 ** 31) * 4096 - 1;
      }
    };
    let keyed = 0;
    for (let run = 0; run < 1500; run++) {
      const header: StatementHeader = {
        command: (['INSERT', 'UPDATE', 'DELETE'] as const)[below(3)]!,
        revision: [0, 1, below(1000), 0xffff_ffff][below(4)]!,
        rowCount: [0, 1, below(100_000), 0xffff_ffff][below(4)]!,
      };
      if (below(8) > 0) {
        header.table = text();
        if (below(6) > 0) {
          const width = [1, 1, 1, 2, 3, 7][below(6)]!;
          header.columns = Array.from({length: width}, text);
          header.keys = Array.from({length: [0, 1, 1, 1, 2, 5, 40][below(7)]!}, () =>
            Array.from({length: width}, key),
          );
          keyed += 1;
        }
      }
      raw.answer(header);
      // The stand-in's bytes are a header the test's own reader reads back.
      expect(decodeStatementHeader(raw.header!), `run ${run}`).toEqual(header);
      const expected = statementResult(header);
      const result = engine.executePrepared(3, []) as JsonPrimitive[];
      expect(result, `run ${run}`).toEqual(expected);
      expect(
        result.every((value, at) => Object.is(value, expected[at])),
        `run ${run}`,
      ).toBe(true);
      expect(isStatementResponse(posted(result), true), `run ${run}`).toBe(true);
    }
    expect(keyed).toBeGreaterThan(900);
    expect(raw.closeCalls).toBe(0);
  });

  it('reads JSON text when WASM wrote no header, and never a header that is not a statement\'s', () => {
    const raw = new HeaderRawEngine();
    const engine = adaptStructuredWasmEngine(raw);
    const {data, ...header} = sqlResult([{id: 1}]);

    // No header stands: the response is JSON text, read as ever.
    for (const disposition of [SAFE, DURABLE]) {
      raw.response = envelope(VERSION, SUCCESS, disposition, header, data);
      expect(engine.executeSql('SELECT id FROM items', [])).toEqual({...header, data});
      expect(engine.executePrepared(3, [])).toEqual({...header, data});
    }
    raw.response = failure('CONSTRAINT', 'no');
    expect(() => engine.executePrepared(3, [])).toThrow(
      expect.objectContaining({code: 'CONSTRAINT', message: 'no'}),
    );

    // A header is read only for a statement: bytes standing there after any
    // other call are not looked at.
    raw.header = encodeStatementHeader({command: 'UPDATE', revision: 1, rowCount: 1});
    raw.afterEveryCall = true;
    raw.responseFor = ({operation}) => responseForOperation(operation);
    engine.beginTransaction();
    expect(engine.inTransaction()).toBe(false);
    expect(engine.revision()).toBe(3);
    expect(engine.prepareSql('SELECT 1')).toBe(9);
    engine.closePrepared(9);
    expect(engine.execSql('SELECT 1; SELECT 2')).toHaveLength(2);
    expect(engine.commitTransaction()).toEqual(outcome());
    engine.rollbackTransaction();
    expect(raw.closeCalls).toBe(0);
  });

  it('reads JSON text from an engine that has a memory, or a place for a header, but not both', () => {
    // A header is read only where there is both a place for one and a memory
    // to read it in. The place is not even asked of an engine whose memory
    // cannot be read, since asking is what has the real engine write headers.
    class MemoryOnly extends FakeRawEngine {
      readonly wasmMemory = new WebAssembly.Memory({initial: 1});
      memoryCalls = 0;
      memory(): WebAssembly.Memory {
        this.memoryCalls += 1;
        return this.wasmMemory;
      }
    }
    class PlaceOnly extends FakeRawEngine {
      headerCalls = 0;
      resultHeader(): number {
        this.headerCalls += 1;
        return HEADER_AT;
      }
    }
    const memoryOnly = new MemoryOnly();
    // What stands in a memory that has no place for a header is not one,
    // wherever it stands.
    const stray = encodeStatementHeader({command: 'UPDATE', revision: 1, rowCount: 1});
    new Uint8Array(memoryOnly.wasmMemory.buffer).set(stray, 0);
    new Uint8Array(memoryOnly.wasmMemory.buffer).set(stray, HEADER_AT);
    const placeOnly = new PlaceOnly();
    const {data, ...header} = sqlResult([{id: 1}]);
    for (const raw of [memoryOnly, placeOnly]) {
      const engine = adaptStructuredWasmEngine(raw);
      for (const disposition of [SAFE, DURABLE]) {
        raw.response = envelope(VERSION, SUCCESS, disposition, header, data);
        expect(engine.executeSql('SELECT id FROM items', [])).toEqual({...header, data});
        expect(engine.executePrepared(3, [])).toEqual({...header, data});
      }
      expect(raw.closeCalls).toBe(0);
      // The answer that stands beside a header means nothing to it.
      raw.response = true;
      expect(() => engine.executePrepared(3, [])).toThrow(
        expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
      );
    }
    expect(memoryOnly.memoryCalls).toBe(1);
    expect(placeOnly.headerCalls).toBe(0);
  });

  it('fails a statement whose header cannot be read, and leaves the engine as it was', () => {
    const raw = new HeaderRawEngine();
    const engine = adaptStructuredWasmEngine(raw);
    const keyed: StatementHeader = {
      command: 'UPDATE',
      revision: 7,
      rowCount: 2,
      table: 'items',
      columns: ['id', 'name'],
      keys: [
        [1, 'one'],
        [2, 'é'],
      ],
    };
    const valid = encodeStatementHeader(keyed);
    const expected = [0, 0, 1, 7, 2, 'items', 2, 'id', 'name', 1, 'one', 2, 'é'];
    // Where the parts of that header are.
    const TABLE = 16;
    const FIRST_VALUE = TABLE + 7 + 4 + 6;
    expect(valid[FIRST_VALUE]).toBe(3);
    expect(valid.length).toBe(FIRST_VALUE + 9 + 6 + 9 + 5);

    const altered = (change: (bytes: Uint8Array, view: DataView) => void, from = valid) => {
      const bytes = from.slice();
      change(bytes, new DataView(bytes.buffer));
      return bytes;
    };
    const resized = (length: number, from = valid) => {
      const bytes = new Uint8Array(Math.max(length, from.length));
      bytes.set(from);
      bytes[4] = length & 0xff;
      bytes[5] = length >> 8;
      return bytes.subarray(0, Math.max(length, 6));
    };
    const single = encodeStatementHeader({
      command: 'DELETE',
      revision: 7,
      rowCount: 1,
      table: 't',
      columns: ['id'],
      keys: [[42]],
    });
    const SINGLE_VALUE = TABLE + 3 + 4;
    expect(single[SINGLE_VALUE]).toBe(3);
    expect(single.length).toBe(SINGLE_VALUE + 9);
    // Keys whose values each take a byte, and keys that end with a number: a
    // value that is refused in either leaves what follows it where it was.
    const flags = encodeStatementHeader({
      command: 'UPDATE',
      revision: 7,
      rowCount: 2,
      table: 't',
      columns: ['a', 'b'],
      keys: [
        [true, null],
        [false, true],
      ],
    });
    const FLAGS = TABLE + 3 + 3 + 3;
    expect([...flags.subarray(FLAGS)]).toEqual([2, 0, 1, 2]);
    const flag = encodeStatementHeader({
      command: 'DELETE',
      revision: 7,
      rowCount: 1,
      table: 't',
      columns: ['a'],
      keys: [[true]],
    });
    expect(flag[flag.length - 1]).toBe(2);
    const numbers = encodeStatementHeader({
      command: 'UPDATE',
      revision: 7,
      rowCount: 2,
      table: 't',
      columns: ['a', 'b'],
      keys: [
        [1, 2],
        [3, 4],
      ],
    });
    const read = encodeStatementHeader({command: 'SELECT', revision: 7, rowCount: 2});
    const unchanged = encodeStatementHeader({command: 'DELETE', revision: 7, rowCount: 0});
    const unkeyed = encodeStatementHeader({command: 'DELETE', revision: 7, rowCount: 3, table: 'items'});
    // Each is answered `true`, as a write is, unless it says otherwise.
    const NOTHING = Symbol('nothing');
    const malformed: [what: string, header: Uint8Array, response?: unknown][] = [
      ['a command past the last', altered((bytes) => (bytes[0] = 5))],
      ['a command far past the last', altered((bytes) => (bytes[0] = 255))],
      ['a write answered with text', valid, 'text'],
      ['a write answered with nothing', valid, NOTHING],
      ['a write answered false', valid, false],
      ['a write answered with a number', valid, 1],
      ['a read answered true', read, true],
      ['a read answered with nothing', read, NOTHING],
      ['a read that changed a table', altered((bytes) => (bytes[0] = 4), unkeyed), 'rows'],
      ['a read with a key width', altered((bytes) => (bytes[1] = 1), read), 'rows'],
      ['a read with a key count', altered((bytes) => (bytes[2] = 1), read), 'rows'],
      ['a length short of the fixed bytes', resized(15)],
      ['no length', resized(0)],
      ['a length short of the values', resized(valid.length - 1)],
      ['a length past the values', resized(valid.length + 1)],
      ['a length that ends inside the names', resized(TABLE + 3)],
      ['a key width without a table', altered((bytes) => (bytes[1] = 1), unchanged)],
      ['a key count without a table', altered((bytes) => (bytes[2] = 1), unchanged)],
      ['a key count without a key width', altered((bytes) => (bytes[2] = 1), unkeyed)],
      ['bytes after the name of a table without keys', resized(unkeyed.length + 1, unkeyed)],
      ['more keys than its values', altered((bytes) => (bytes[2] = 3))],
      ['fewer keys than its values', altered((bytes) => (bytes[2] = 1))],
      ['more columns than its values', altered((bytes) => (bytes[1] = 3))],
      ['more keys than a table reports', altered((_, view) => view.setUint16(2, 1001, true))],
      [
        'more keys than a table reports, each with its values',
        encodeStatementHeader({
          command: 'INSERT',
          revision: 7,
          rowCount: 1001,
          table: 'items',
          columns: ['id'],
          keys: Array.from({length: 1001}, (_, id) => [id]),
        }),
      ],
      ['more keys than its bytes could hold', altered((bytes) => {
        bytes[1] = 255;
        bytes[2] = 0xe8;
        bytes[3] = 0x03;
      })],
      ['a value tagged as an array', altered((bytes) => (bytes[FIRST_VALUE] = 7))],
      ['a value tagged as an object', altered((bytes) => (bytes[FIRST_VALUE] = 8))],
      ['a value with an unknown tag', altered((bytes) => (bytes[FIRST_VALUE] = 4))],
      ['a value with a tag past every tag', altered((bytes) => (bytes[FIRST_VALUE] = 200))],
      ['a key that is not a number', altered((_, view) => view.setFloat64(FIRST_VALUE + 1, Number.NaN, true))],
      ['a key that is not finite', altered((_, view) => view.setFloat64(FIRST_VALUE + 1, -Infinity, true))],
      // A single key of one column is read by a path of its own.
      ['a single key with an unknown tag', altered((bytes) => (bytes[SINGLE_VALUE] = 4), single)],
      ['a single key tagged as an array', altered((bytes) => (bytes[SINGLE_VALUE] = 7), single)],
      ['a single key that is not a number', altered((_, view) => view.setFloat64(SINGLE_VALUE + 1, Number.NaN, true), single)],
      ['a single key that is not finite', altered((_, view) => view.setFloat64(SINGLE_VALUE + 1, Infinity, true), single)],
      ['a single key cut short', resized(single.length - 1, single)],
      ['a single key with bytes after it', resized(single.length + 1, single)],
      ['a single key without its value', resized(SINGLE_VALUE, single)],
      // A value of one byte whose tag is none: everything after it is where
      // it would be, so only the tag tells.
      ...[4, 7, 8, 9, 255].flatMap((tag): [string, Uint8Array][] => [
        [`a first key tagged ${tag}`, altered((bytes) => (bytes[FLAGS] = tag), flags)],
        [`a key tagged ${tag}`, altered((bytes) => (bytes[FLAGS + 2] = tag), flags)],
        [`a last key tagged ${tag}`, altered((bytes) => (bytes[bytes.length - 1] = tag), flags)],
        [`a single key of one byte tagged ${tag}`, altered((bytes) => (bytes[bytes.length - 1] = tag), flag)],
      ]),
      // A last value that is refused ends the header where it should end.
      ['a last key that is not a number', altered((bytes, view) => view.setFloat64(bytes.length - 8, Number.NaN, true), numbers)],
      ['a last key that is not finite', altered((bytes, view) => view.setFloat64(bytes.length - 8, -Infinity, true), numbers)],
      // The last value is the two bytes of an "é", which no longer decode.
      ['text that is not UTF-8', altered((bytes) => (bytes[bytes.length - 1] = 0x28))],
      ['a name that is not UTF-8', altered((bytes) => {
        bytes[TABLE + 1] = 0x80;
        bytes[TABLE + 2] = 0xff;
      })],
    ];
    for (const [what, header, response = true] of malformed) {
      raw.header = header;
      raw.response = response === NOTHING ? undefined : response;
      for (const run of [
        () => engine.executeSql('UPDATE items SET id = id', []),
        () => engine.executePrepared(3, []),
      ]) {
        let thrown: unknown;
        try {
          run();
        } catch (error) {
          thrown = error;
        }
        expect(thrown, what).toBeInstanceOf(WasmStructuredDecodeError);
        expect(thrown, what).toMatchObject({
          code: 'BRIDGE_SERIALIZATION_ERROR',
          message: 'WASM returned an invalid structured SQL result',
          disposition: 'safe',
        });
      }
      // The engine was not closed, and reads the next header.
      expect(raw.closeCalls, what).toBe(0);
      raw.header = valid;
      raw.response = true;
      expect(engine.executePrepared(3, []), what).toEqual(expected);
    }

    // A header at the end of the memory, whose bytes run out as it is read.
    const memoryBytes = raw.wasmMemory.buffer.byteLength;
    for (const short of [1, 2, 5, 9, 13, 16, 18, 22, 30, 40]) {
      const edge = new HeaderRawEngine();
      edge.headerAt = memoryBytes - short;
      const edged = adaptStructuredWasmEngine(edge);
      edge.header = valid.subarray(0, short);
      edge.response = true;
      expect(() => edged.executePrepared(3, []), `${short}`).toThrow(
        expect.objectContaining({
          message: 'WASM returned an invalid structured SQL result',
          disposition: 'safe',
        }),
      );
      expect(edge.closeCalls).toBe(0);
    }
    // One whose last bytes are those of a text is refused alike, though
    // nothing is thrown in reading a text that is not all there: a table's
    // name, and a key's value.
    const named = encodeStatementHeader({command: 'DELETE', revision: 7, rowCount: 3, table: 'a_table'});
    const textual = encodeStatementHeader({
      command: 'DELETE',
      revision: 7,
      rowCount: 1,
      table: 't',
      columns: ['id'],
      keys: [['a key of text']],
    });
    for (const whole of [named, textual]) {
      for (const missing of [1, 3, 7]) {
        const edge = new HeaderRawEngine();
        edge.headerAt = memoryBytes - (whole.length - missing);
        const edged = adaptStructuredWasmEngine(edge);
        edge.header = whole.subarray(0, whole.length - missing);
        edge.response = true;
        expect(() => edged.executePrepared(3, []), `${missing}`).toThrow(
          expect.objectContaining({
            message: 'WASM returned an invalid structured SQL result',
            disposition: 'safe',
          }),
        );
        expect(edge.closeCalls).toBe(0);
      }
      // All of it, ending where the memory does, is read.
      const last = new HeaderRawEngine();
      last.headerAt = memoryBytes - whole.length;
      last.header = whole;
      last.response = true;
      expect(adaptStructuredWasmEngine(last).executePrepared(3, [])).toEqual(
        statementResult(decodeStatementHeader(whole)),
      );
    }
    // And one whose place is past the memory altogether holds nothing.
    const outside = new HeaderRawEngine();
    outside.headerAt = memoryBytes + 8;
    outside.response = sqlResponse([sqlResult()]);
    expect(adaptStructuredWasmEngine(outside).executePrepared(3, [])).toEqual(sqlResult());
  });

  it('treats `true` without a header as a result it cannot read', () => {
    // The answer beside a header means nothing without one: the response is
    // unreadable, and may be that of a statement that published.
    for (const raw of [new FakeRawEngine(), new HeaderRawEngine()]) {
      raw.response = true;
      const engine = adaptStructuredWasmEngine(raw);
      expect(() => engine.executePrepared(3, [])).toThrow(
        expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
      );
      expect(raw.closeCalls).toBe(1);
    }
  });

  it('reads headers from the memory as it is after growing', () => {
    const raw = new HeaderRawEngine();
    const engine = adaptStructuredWasmEngine(raw);
    const header: StatementHeader = {
      command: 'INSERT',
      revision: 7,
      rowCount: 1,
      table: 'items',
      columns: ['id', 'name'],
      keys: [[1, 'one']],
    };
    const expected = [0, 0, 0, 7, 1, 'items', 2, 'id', 'name', 1, 'one'];
    raw.answer(header);
    expect(engine.executePrepared(3, [])).toEqual(expected);
    for (let grown = 0; grown < 3; grown++) {
      // Growing detaches the buffer the bridge was reading.
      const before = raw.wasmMemory.buffer;
      raw.wasmMemory.grow(1);
      expect(before.byteLength).toBe(0);
      expect(engine.executePrepared(3, [])).toEqual(expected);
      expect(engine.executeSql('INSERT INTO items VALUES (1)', [])).toEqual(expected);
      // A response in JSON text is read as before, too.
      raw.header = undefined;
      raw.response = sqlResponse([sqlResult()]);
      expect(engine.executePrepared(3, [])).toEqual(sqlResult());
      raw.answer(header);
    }

    // So is one that grows inside the very call that writes the header.
    raw.duringCall = () => raw.wasmMemory.grow(1);
    expect(engine.executePrepared(3, [])).toEqual(expected);
    expect(engine.executePrepared(3, [])).toEqual(expected);
    expect(raw.memoryCalls).toBe(1);
  });

  it('preflights structured requests before entering WASM', () => {
    const raw = new FakeRawEngine();
    const engine = adaptStructuredWasmEngine(raw);

    expect(() =>
      engine.executeSql('SELECT $1', [{value: Number.NaN} as unknown as Row]),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
    expect(raw.calls).toHaveLength(0);
  });

  it('validates result headers while preserving SAFE failures', () => {
    const raw = new FakeRawEngine();
    const engine = adaptStructuredWasmEngine(raw);
    const {data, ...header} = sqlResult();
    // A result that published nothing is checked as one that published is,
    // and one that fails the check fails its statement alone.
    for (const response of [
      envelope(VERSION, SUCCESS, SAFE, {...header, rowCount: -1}, data),
      envelope(VERSION, SUCCESS, SAFE, {...header, extra: true}, data),
      envelope(VERSION, SUCCESS, SAFE, [header], data, data),
      `${success()}\n${data}`,
      // One without its rows is not a statement's result at all,
      envelope(VERSION, SUCCESS, SAFE, header),
      // and nor is the array a statement's response is, which only a header
      // is ever read into.
      envelope(VERSION, SUCCESS, SAFE, [0, 0, 1, 7, 1], data),
      envelope(VERSION, SUCCESS, SAFE, [PROTOCOL_VERSION, 1, 1, 7, 1, 'items', 1, 'id', 42], data),
      envelope(VERSION, SUCCESS, SAFE, ['UPDATE', 7, 1], data),
    ]) {
      raw.response = response;
      for (const run of [
        () => engine.executeSql('UPDATE items SET id = id', []),
        () => engine.executePrepared(3, []),
      ]) {
        let thrown: unknown;
        try {
          run();
        } catch (error) {
          thrown = error;
        }
        expect(thrown, response).toBeInstanceOf(WasmStructuredDecodeError);
        expect(thrown, response).toMatchObject({
          code: 'BRIDGE_SERIALIZATION_ERROR',
          message: 'WASM returned an invalid structured SQL result',
          disposition: 'safe',
        });
      }
    }
    raw.response = envelope(VERSION, SUCCESS, SAFE, [header, header], data);
    expect(() => engine.execSql('SELECT 1; SELECT 2')).toThrow(
      WasmStructuredDecodeError,
    );
    raw.response = `${failure('CONSTRAINT', 'no')}\n${data}`;
    expect(() => engine.executeSql('UPDATE items SET id = id', [])).toThrow(
      WasmStructuredDecodeError,
    );
    expect(raw.closeCalls).toBe(0);
    // The engine goes on reading results.
    raw.response = envelope(VERSION, SUCCESS, SAFE, header, data);
    expect(engine.executeSql('UPDATE items SET id = id', [])).toEqual({
      ...header,
      data,
    });
  });

  it('poisons durable and unknown malformed mutation results', () => {
    const {data, ...header} = sqlResult();
    for (const response of [
      success(null, DURABLE),
      envelope(VERSION, SUCCESS, DURABLE, {...header, rowCount: -1}, data),
      // A statement's response as an array is never a durable result's.
      envelope(VERSION, SUCCESS, DURABLE, [0, 0, 1, 7, 1], data),
      envelope(VERSION, SUCCESS, DURABLE, [0, 0, 1, 7, 1, 'items', 1, 'id', 42], data),
      envelope(99, SUCCESS, SAFE, null),
      envelope(5, SUCCESS, SAFE, header, data),
      'not JSON',
      [VERSION, SUCCESS, SAFE, null],
      new Error('structured call trapped'),
    ]) {
      const raw = new FakeRawEngine();
      raw.response = response;
      const engine = adaptStructuredWasmEngine(raw);

      expect(() =>
        engine.executeSql('INSERT INTO items VALUES (1)', []),
      ).toThrow(
        expect.objectContaining({
          code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN',
          retryable: false,
        }),
      );
      expect(raw.closeCalls).toBe(1);
      expect(raw.freeCalls).toBe(1);
      expect(() => engine.revision()).toThrow(
        expect.objectContaining({code: 'STORAGE_ENGINE_POISONED'}),
      );
      // Nor does it run a statement of either kind, which is refused before
      // anything of it is written.
      const calls = raw.calls.length;
      for (const run of [
        () => engine.executeSql('INSERT INTO items VALUES (1)', []),
        () => engine.executePrepared(3, []),
      ]) {
        expect(run).toThrow(
          expect.objectContaining({code: 'STORAGE_ENGINE_POISONED'}),
        );
      }
      expect(raw.calls).toHaveLength(calls);
    }
  });

  it('reads the outcome of a commit in full, and poisons on one it cannot read', () => {
    // The engine reports the revision a commit published with the tables and
    // keys it changed, which is more than the page is then told.
    const committed = {revision: 4, tables: ['items'], keys: {items: [{id: 1}]}};
    const read = new FakeRawEngine();
    read.response = success(committed, DURABLE);
    const reader = adaptStructuredWasmEngine(read);
    expect(reader.commitTransaction()).toEqual(committed);
    expect(reader.setSchema({version: 1, tables: []}, false)).toEqual(committed);
    expect(read.closeCalls).toBe(0);

    for (const malformed of [
      {revision: 4},
      {revision: 4, tables: ['items']},
      {revision: 4, keys: {}},
      {revision: -1, tables: [], keys: {}},
      {revision: 4, tables: 'items', keys: {}},
      {revision: 4, tables: [1], keys: {}},
      {revision: 4, tables: [], keys: []},
      {revision: 4, tables: [], keys: {}, extra: true},
      [4, [], {}],
      null,
    ]) {
      for (const run of [
        (engine: ReturnType<typeof adaptStructuredWasmEngine>) => engine.commitTransaction(),
        (engine: ReturnType<typeof adaptStructuredWasmEngine>) =>
          engine.setSchema({version: 1, tables: []}, false),
      ]) {
        // One that says it published may have, so the engine is not used again.
        const raw = new FakeRawEngine();
        raw.response = success(malformed, DURABLE);
        const engine = adaptStructuredWasmEngine(raw);
        expect(() => run(engine), JSON.stringify(malformed)).toThrow(
          expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
        );
        expect(raw.closeCalls).toBe(1);
        // One that says it published nothing fails alone.
        const safe = new FakeRawEngine();
        safe.response = success(malformed, SAFE);
        const kept = adaptStructuredWasmEngine(safe);
        expect(() => run(kept), JSON.stringify(malformed)).toThrow(
          WasmStructuredDecodeError,
        );
        expect(safe.closeCalls).toBe(0);
      }
    }
  });

  it('reads a schema, and keeps a malformed one nonfatal', () => {
    const raw = new FakeRawEngine();
    const schema = {
      version: 0,
      tables: [
        {
          name: 'notes',
          columns: [
            {name: 'id', type: 'integer', nullable: false},
            {name: 'body', type: 'text', nullable: true, default: null},
          ],
          primaryKey: ['id'],
          indexes: [{name: 'notes_body', columns: ['body'], unique: false}],
          foreignKeys: [],
        },
      ],
    };
    raw.response = success(schema);
    const engine = adaptStructuredWasmEngine(raw);

    expect(engine.schema()).toEqual(schema);
    expect(raw.calls).toEqual([
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.schema,
        payload: undefined,
      },
    ]);
    raw.response = success({tables: [{name: 'notes'}]});
    expect(() => engine.schema()).toThrow(WasmStructuredDecodeError);
    expect(raw.closeCalls).toBe(0);

    raw.response = success({revision: 4, tables: ['notes'], keys: {}}, DURABLE);
    expect(engine.setSchema(schema as Schema, true)).toEqual({
      revision: 4,
      tables: ['notes'],
      keys: {},
    });
    expect(raw.calls.at(-1)).toEqual({
      bridgeVersion: VERSION,
      operation: WASM_OPERATION.setSchema,
      payload: {schema, drop: true},
    });
  });

  it('keeps malformed nonmutating control results nonfatal', () => {
    const raw = new FakeRawEngine();
    raw.response = envelope(99, SUCCESS, SAFE, null);
    const engine = adaptStructuredWasmEngine(raw);

    expect(() => engine.revision()).toThrow(WasmStructuredDecodeError);
    expect(raw.closeCalls).toBe(0);
    raw.response = success(3);
    expect(engine.revision()).toBe(3);
  });

  it('closes immediately on fatal structured engine errors', () => {
    for (const code of [
      'RECOVERY_REQUIRED',
      'STORAGE_COMMIT_OUTCOME_UNKNOWN',
      'STORAGE_ENGINE_POISONED',
    ]) {
      const raw = new FakeRawEngine();
      raw.response = failure(code, 'reopen', false);
      const engine = adaptStructuredWasmEngine(raw);

      expect(() => engine.revision()).toThrow(
        expect.objectContaining({code, retryable: false}),
      );
      expect(raw.closeCalls).toBe(1);
      expect(raw.freeCalls).toBe(1);
    }
  });

  it('blocks same-engine, cross-engine, close, and constructor page-callback reentry', () => {
    CallbackRawEngine.instances.length = 0;
    const firstDevice = new CallbackPageDevice();
    const secondDevice = new CallbackPageDevice();
    const first = createStructuredWasmEngine(CallbackRawEngine, firstDevice);
    const second = createStructuredWasmEngine(CallbackRawEngine, secondDevice);
    const firstRaw = CallbackRawEngine.instances[0]!;
    const errors: unknown[] = [];
    firstDevice.callback = () => {
      for (const reenter of [
        () => first.executeSql('SELECT id FROM items', []),
        () => second.executeSql('SELECT id FROM items', []),
        () => first.executePrepared(3, []),
        () => second.executePrepared(3, []),
        () => first.close(),
        () => second.close(),
        () =>
          createStructuredWasmEngine(
            CallbackRawEngine,
            new CallbackPageDevice(),
          ),
      ]) {
        try {
          reenter();
          errors.push(new Error('reentrant operation unexpectedly succeeded'));
        } catch (error) {
          errors.push(error);
        }
      }
    };

    for (const transfer of ['read', 'write', 'flush'] as const) {
      firstRaw.transfer = transfer;
      expect(first.executeSql('SELECT id FROM items', [])).toEqual(
        sqlResult(),
      );
      expect(errors).toHaveLength(7);
      for (const error of errors) {
        expect(error).toEqual(
          expect.objectContaining({
            code: 'ENGINE_REENTRANT_CALL',
            retryable: false,
          }),
        );
      }
      errors.length = 0;
    }

    expect(firstRaw.observedRead).toBe(7);
    expect(firstDevice.written[0]?.[0]).toBe(9);
    expect(CallbackRawEngine.instances).toHaveLength(2);

    // Rejected nested closes did not mutate either bridge lifecycle.
    firstDevice.callback = undefined;
    expect(second.executeSql('SELECT id FROM items', [])).toEqual(
      sqlResult(),
    );
    first.close();
    first.close();
    second.close();
    expect(firstDevice.closed).toBe(1);
    expect(secondDevice.closed).toBe(1);
    expect(firstRaw.freeCalls).toBe(1);
    expect(CallbackRawEngine.instances[1]?.freeCalls).toBe(1);
  });
});

function responseForOperation(operation: number): unknown {
  switch (operation) {
    case WASM_OPERATION.commit:
      return success(outcome(), DURABLE);
    case WASM_OPERATION.executeSql:
    case WASM_OPERATION.executePrepared:
      return sqlResponse([sqlResult([{id: 1}])]);
    case WASM_OPERATION.execSql:
      return sqlResponse([sqlResult([{id: 1}]), sqlResult()], true);
    case WASM_OPERATION.inTransaction:
      return success(false);
    case WASM_OPERATION.revision:
      return success(3);
    case WASM_OPERATION.prepareSql:
      return success(9);
    case WASM_OPERATION.begin:
    case WASM_OPERATION.rollback:
    case WASM_OPERATION.close:
    case WASM_OPERATION.closePrepared:
      return success();
    default:
      throw new Error(`Unexpected structured test operation ${operation}`);
  }
}

class CallbackPageDevice implements PageDevice {
  callback: (() => void) | undefined;
  readonly page = new Uint8Array(4096).fill(7);
  readonly written: Uint8Array[] = [];
  closed = 0;

  pageCount(): number {
    return 1;
  }

  readPage(_low: number, _high: number, target: Uint8Array): number {
    this.callback?.();
    target.set(this.page);
    return target.byteLength;
  }

  writePages(_low: number, _high: number, source: Uint8Array): number {
    this.callback?.();
    this.written.push(source.slice());
    return source.byteLength;
  }

  flush(): void {
    this.callback?.();
  }

  close(): void {
    this.callback?.();
    this.closed += 1;
  }
}

class CallbackRawEngine implements RawStructuredWasmEngine {
  static readonly instances: CallbackRawEngine[] = [];
  freeCalls = 0;
  observedRead: number | undefined;
  transfer: 'read' | 'write' | 'flush' = 'read';

  constructor(readonly device: PageDevice) {
    device.pageCount();
    CallbackRawEngine.instances.push(this);
  }

  callStructured(
    bridgeVersion: number,
    operation: number,
    _request: Uint8Array,
  ): unknown {
    expect(bridgeVersion).toBe(VERSION);
    if (operation === WASM_OPERATION.close) {
      this.device.close();
      return success();
    }
    if (operation === WASM_OPERATION.executeSql) {
      if (this.transfer === 'read') {
        const target = new Uint8Array(4096);
        this.device.readPage(0, 0, target);
        this.observedRead = target[0];
      } else if (this.transfer === 'write') {
        this.device.writePages(0, 0, new Uint8Array(8192).fill(9));
      } else {
        this.device.flush();
      }
      return sqlResponse([sqlResult()]);
    }
    return success(1);
  }

  free(): void {
    this.freeCalls += 1;
  }
}

CallbackRawEngine satisfies RawStructuredWasmEngineConstructor;
