import {describe, expect, it} from 'vitest';

import {
  isRpcResultHeader,
  isSqlResultText,
  readSqlResult,
  type Row,
  type SqlResult,
  type SqlResultText,
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

const VERSION = 4;
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

// A statement's result as the page reads it: as text, when it published nothing.
function read(result: SqlResult | SqlResultText): unknown {
  return isSqlResultText(result) ? readSqlResult(result) : result;
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

    expect(read(engine.executeSql('SELECT $1, $2', params))).toEqual(
      sqlResult([{id: 1}]),
    );
    expect(read(engine.executeSql('SELECT $1', [5], 'array'))).toEqual(
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
    expect(read(engine.executePrepared(9, preparedParams))).toEqual(
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
  });

  it('passes the rows of each result on as the JSON text WASM wrote', () => {
    const raw = new FakeRawEngine();
    const data = '{"fields":[{"name":"id","dataTypeID":20}],"rows":[{"id":1}]}';
    const header = {command: 'SELECT', revision: 1, rowCount: 1, tables: [], keys: {}};
    raw.response = envelope(VERSION, SUCCESS, SAFE, header, data);
    const engine = adaptStructuredWasmEngine(raw);

    // A result that published nothing passes on unread, header and all.
    expect(engine.executeSql('SELECT id FROM items', [])).toEqual([
      JSON.stringify(header),
      data,
    ]);
    // One that did is read, to announce its changes, and its rows passed on.
    const changed = {...header, command: 'INSERT', tables: ['items'], keys: {items: [{id: 1}]}};
    raw.response = envelope(VERSION, SUCCESS, DURABLE, changed, data);
    expect(engine.executeSql('INSERT INTO items VALUES (1)', [])).toEqual({
      ...changed,
      data,
    });
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
    // A result that published nothing passes on unread, for the page to check.
    for (const response of [
      envelope(VERSION, SUCCESS, SAFE, {...header, rowCount: -1}, data),
      envelope(VERSION, SUCCESS, SAFE, {...header, extra: true}, data),
      envelope(VERSION, SUCCESS, SAFE, [header], data, data),
      `${success()}\n${data}`,
    ]) {
      raw.response = response;
      const result = engine.executeSql('UPDATE items SET id = id', []);
      expect(isSqlResultText(result)).toBe(true);
      expect(isRpcResultHeader('executeSql', read(result))).toBe(false);
    }
    // One without its rows is not a statement's result at all.
    raw.response = envelope(VERSION, SUCCESS, SAFE, header);
    expect(() => engine.executeSql('UPDATE items SET id = id', [])).toThrow(
      WasmStructuredDecodeError,
    );
    raw.response = envelope(VERSION, SUCCESS, SAFE, [header, header], data);
    expect(() => engine.execSql('SELECT 1; SELECT 2')).toThrow(
      WasmStructuredDecodeError,
    );
    raw.response = `${failure('CONSTRAINT', 'no')}\n${data}`;
    expect(() => engine.executeSql('UPDATE items SET id = id', [])).toThrow(
      WasmStructuredDecodeError,
    );
    expect(raw.closeCalls).toBe(0);
  });

  it('poisons durable and unknown malformed mutation results', () => {
    const {data, ...header} = sqlResult();
    for (const response of [
      success(null, DURABLE),
      envelope(VERSION, SUCCESS, DURABLE, {...header, rowCount: -1}, data),
      envelope(99, SUCCESS, SAFE, null),
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
    }
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
      expect(read(first.executeSql('SELECT id FROM items', []))).toEqual(
        sqlResult(),
      );
      expect(errors).toHaveLength(5);
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
    expect(read(second.executeSql('SELECT id FROM items', []))).toEqual(
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
