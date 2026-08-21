import {describe, expect, it} from 'vitest';

import type {
  QueryPlan,
  Row,
  SqlResult,
  TableSchema,
} from '../../src/protocol.ts';
import type {PageDevice} from '../../src/worker/page-device.ts';
import {
  WASM_OPERATION,
  StructuredWasmEngine,
  WasmStructuredDecodeError,
  createStructuredWasmEngine,
  normalizeWasmConstructorError,
  type RawStructuredWasmEngine,
  type RawStructuredWasmEngineConstructor,
} from '../../src/worker/wasm-bridge.ts';

const VERSION = 1;
const SUCCESS = 0;
const FAILURE = 1;
const SAFE = 0;
const DURABLE = 1;

type RawCall = {
  bridgeVersion: number;
  operation: number;
  payload: unknown;
};

const query: QueryPlan = {table: 'items', filters: []};
const schema: TableSchema = {name: 'items', primaryKey: ['id']};

function success(payload: unknown = undefined, disposition = SAFE): unknown[] {
  return [VERSION, SUCCESS, disposition, payload];
}

function failure(
  code: string,
  message: string,
  retryable?: boolean,
): unknown[] {
  return [
    VERSION,
    FAILURE,
    SAFE,
    {code, message, ...(retryable === undefined ? {} : {retryable})},
  ];
}

function outcome(revision = 1, tables: string[] = ['items']) {
  return {revision, tables};
}

function queryResult(rows: Row[] = []): unknown {
  return {
    revision: 1,
    fields: rows.length > 0 ? [{name: 'id', dataTypeID: 20}] : [],
    rows,
  };
}

function sqlResult(rows: Row[] = []): SqlResult {
  return {
    command: 'SELECT',
    fields: rows.length > 0 ? [{name: 'id', dataTypeID: 20}] : [],
    revision: 1,
    rowCount: rows.length,
    rows,
    tables: [],
  };
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
    payload: unknown,
  ): unknown {
    const call = {bridgeVersion, operation, payload};
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
    });
  });

  it('normalizes the direct constructor error payload', () => {
    const ThrowingRaw = class {
      constructor(_device: PageDevice) {
        throw Object.assign(Object.create(null), {
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

    const malformed = {
      code: 'STORAGE_LOCKED',
      message: 'locked',
      details: 'not part of the constructor ABI',
    };
    expect(normalizeWasmConstructorError(malformed)).toBe(malformed);
  });
  it('passes the exact structured payload shapes through every operation', () => {
    const raw = new FakeRawEngine();
    raw.responseFor = ({operation}) => responseForOperation(operation);
    const engine = new StructuredWasmEngine(raw);

    const schemas = [schema];
    const replacementRows = [{id: 1}];
    const batch = {
      changes: [{type: 'upsert' as const, table: 'items', row: {id: 2}}],
    };
    const params = [3, 'three'];
    const preparedParams = [4];

    engine.defineTables(schemas);
    expect(engine.replaceTableSnapshot(schema, replacementRows)).toEqual(
      outcome(),
    );
    expect(engine.applyBatch(batch)).toEqual(outcome());
    expect(engine.query(query)).toEqual(queryResult([{id: 1}]));
    expect(engine.executeSql('SELECT $1, $2', params)).toEqual(
      sqlResult([{id: 1}]),
    );
    expect(engine.execSql('SELECT 1; SELECT 2')).toEqual([]);
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
        operation: WASM_OPERATION.defineTables,
        payload: schemas,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.replaceSnapshot,
        payload: {schema, rows: replacementRows},
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.applyBatch,
        payload: batch,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.query,
        payload: query,
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.executeSql,
        payload: {sql: 'SELECT $1, $2', params},
      },
      {
        bridgeVersion: VERSION,
        operation: WASM_OPERATION.execSql,
        payload: 'SELECT 1; SELECT 2',
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

  it('returns validated result graphs without rebuilding them', () => {
    const raw = new FakeRawEngine();
    const result = queryResult([{id: 1}]);
    raw.response = success(result);
    const engine = new StructuredWasmEngine(raw);

    expect(engine.query(query)).toBe(result);
  });

  it('preflights structured requests before entering WASM', () => {
    const raw = new FakeRawEngine();
    const engine = new StructuredWasmEngine(raw);
    const accessor = {} as {value?: number};
    Object.defineProperty(accessor, 'value', {
      enumerable: true,
      get: () => 1,
    });

    expect(() =>
      engine.executeSql('SELECT $1', [accessor as unknown as Row]),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
    expect(raw.calls).toHaveLength(0);
  });

  it('fully validates structured results while preserving SAFE failures', () => {
    const raw = new FakeRawEngine();
    const engine = new StructuredWasmEngine(raw);
    raw.response = success({
      ...sqlResult(),
      rows: [{created: new Date()}],
      rowCount: 1,
    });

    expect(() => engine.executeSql('UPDATE items SET id = id', [])).toThrow(
      WasmStructuredDecodeError,
    );
    expect(raw.closeCalls).toBe(0);
  });

  it('poisons durable and unknown malformed mutation results', () => {
    for (const response of [
      success(undefined, DURABLE),
      [99, SUCCESS, SAFE, undefined],
      new Error('structured call trapped'),
    ]) {
      const raw = new FakeRawEngine();
      raw.response = response;
      const engine = new StructuredWasmEngine(raw);

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

  it('keeps malformed read results nonfatal', () => {
    const raw = new FakeRawEngine();
    raw.response = [99, SUCCESS, SAFE, undefined];
    const engine = new StructuredWasmEngine(raw);

    expect(() => engine.query(query)).toThrow(WasmStructuredDecodeError);
    expect(raw.closeCalls).toBe(0);
    raw.response = success(queryResult());
    expect(engine.query(query)).toEqual(queryResult());
  });

  it('closes immediately on fatal structured engine errors', () => {
    for (const code of [
      'RECOVERY_REQUIRED',
      'STORAGE_COMMIT_OUTCOME_UNKNOWN',
      'STORAGE_ENGINE_POISONED',
    ]) {
      const raw = new FakeRawEngine();
      raw.response = failure(code, 'reopen', false);
      const engine = new StructuredWasmEngine(raw);

      expect(() => engine.query(query)).toThrow(
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
        () => first.query(query),
        () => second.query(query),
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
      expect(first.query(query)).toEqual(queryResult());
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
    expect(second.query(query)).toEqual(queryResult());
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
    case WASM_OPERATION.defineTables:
      return success(undefined, DURABLE);
    case WASM_OPERATION.replaceSnapshot:
    case WASM_OPERATION.applyBatch:
    case WASM_OPERATION.commit:
      return success(outcome(), DURABLE);
    case WASM_OPERATION.query:
      return success(queryResult([{id: 1}]));
    case WASM_OPERATION.executeSql:
    case WASM_OPERATION.executePrepared:
      return success(sqlResult([{id: 1}]));
    case WASM_OPERATION.execSql:
      return success([]);
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

  writePage(_low: number, _high: number, source: Uint8Array): number {
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
    _payload: unknown,
  ): unknown {
    expect(bridgeVersion).toBe(VERSION);
    if (operation === WASM_OPERATION.close) {
      this.device.close();
      return success();
    }
    if (operation === WASM_OPERATION.query) {
      if (this.transfer === 'read') {
        const target = new Uint8Array(4096);
        this.device.readPage(0, 0, target);
        this.observedRead = target[0];
      } else if (this.transfer === 'write') {
        this.device.writePage(0, 0, new Uint8Array(4096).fill(9));
      } else {
        this.device.flush();
      }
      return success(queryResult());
    }
    return success(1);
  }

  free(): void {
    this.freeCalls += 1;
  }
}

CallbackRawEngine satisfies RawStructuredWasmEngineConstructor;
