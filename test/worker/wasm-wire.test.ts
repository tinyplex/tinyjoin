import {describe, expect, it} from 'vitest';

import type {
  ChangeBatch,
  JsonValue,
  QueryPlan,
  Row,
  TableSchema,
} from '../../src/protocol.ts';
import type {PageDevice} from '../../src/worker/page-device.ts';
import {
  BinaryWasmEngine,
  WASM_OPERATION,
  WasmBridgeError,
  WasmWireDecodeError,
  decodeApplyOutcomeResponse,
  decodePreparedStatementIdResponse,
  decodeQueryResultResponse,
  decodeRevisionResponse,
  decodeSqlResultsResponse,
  decodeUnitResponse,
  createBinaryWasmEngine,
  encodeApplyBatch,
  encodeClosePrepared,
  encodeDefineTables,
  encodeExecuteSql,
  encodeExecutePrepared,
  encodeExecSql,
  encodeQuery,
  encodePrepareSql,
  encodeReplaceSnapshot,
  normalizeWasmConstructorError,
  type RawBinaryWasmEngine,
  type RawBinaryWasmEngineConstructor,
  type WireTableSchema,
} from '../../src/worker/wasm-wire.ts';

const VERSION = 1;
const SUCCESS = 0;
const FAILURE = 1;
const SAFE = 0;
const DURABLE = 1;

const schema: TableSchema = {name: 'items', primaryKey: ['id']};
const query: QueryPlan = {
  table: 'items',
  columns: ['id', 'title'],
  filters: [{column: 'id', operator: 'gte', value: 1}],
  orderBy: [{column: 'id', direction: 'desc', nulls: 'last'}],
  limit: 10,
  offset: 2,
};

class Bytes {
  readonly bytes: number[] = [];

  u8(value: number): this {
    this.bytes.push(value);
    return this;
  }

  u32(value: number): this {
    this.bytes.push(
      value & 0xff,
      (value >>> 8) & 0xff,
      (value >>> 16) & 0xff,
      (value >>> 24) & 0xff,
    );
    return this;
  }

  u64(value: bigint): this {
    let remaining = BigInt.asUintN(64, value);
    for (let index = 0; index < 8; index += 1) {
      this.bytes.push(Number(remaining & 0xffn));
      remaining >>= 8n;
    }
    return this;
  }

  i64(value: bigint): this {
    return this.u64(BigInt.asUintN(64, value));
  }

  f64(value: number): this {
    const bytes = new Uint8Array(8);
    new DataView(bytes.buffer).setFloat64(0, value, true);
    this.bytes.push(...bytes);
    return this;
  }

  string(value: string): this {
    const bytes = new TextEncoder().encode(value);
    this.u32(bytes.byteLength);
    this.bytes.push(...bytes);
    return this;
  }

  strings(values: readonly string[]): this {
    this.u32(values.length);
    for (const value of values) {
      this.string(value);
    }
    return this;
  }

  fields(values: ReadonlyArray<{name: string; dataTypeID: number}>): this {
    this.u32(values.length);
    for (const value of values) {
      this.string(value.name).u32(value.dataTypeID);
    }
    return this;
  }

  json(value: JsonValue): this {
    if (value === null) {
      return this.u8(0);
    }
    if (value === false) {
      return this.u8(1);
    }
    if (value === true) {
      return this.u8(2);
    }
    if (typeof value === 'number') {
      return Number.isSafeInteger(value)
        ? this.u8(3).i64(BigInt(value))
        : this.u8(5).f64(value);
    }
    if (typeof value === 'string') {
      return this.u8(6).string(value);
    }
    if (Array.isArray(value)) {
      this.u8(7).u32(value.length);
      for (const item of value) {
        this.json(item);
      }
      return this;
    }
    this.u8(8);
    return this.row(value);
  }

  row(value: Row): this {
    const entries = Object.entries(value);
    this.u32(entries.length);
    for (const [key, item] of entries) {
      this.string(key).json(item);
    }
    return this;
  }

  rows(values: readonly Row[]): this {
    this.u32(values.length);
    for (const value of values) {
      this.row(value);
    }
    return this;
  }

  done(): Uint8Array {
    return new Uint8Array(this.bytes);
  }
}

function success(
  disposition: 0 | 1,
  write: (bytes: Bytes) => void = () => {},
): Uint8Array {
  const bytes = new Bytes().u8(VERSION).u8(SUCCESS).u8(disposition);
  write(bytes);
  return bytes.done();
}

function failure(
  code: string,
  message: string,
  retryable: 0 | 1 | 2 = 0,
): Uint8Array {
  return new Bytes()
    .u8(VERSION)
    .u8(FAILURE)
    .u8(SAFE)
    .string(code)
    .string(message)
    .u8(retryable)
    .done();
}

function outcome(
  revision = 1n,
  tables: readonly string[] = [],
  disposition: 0 | 1 = SAFE,
): Uint8Array {
  return success(disposition, (bytes) => bytes.u64(revision).strings(tables));
}

class FakeRawEngine implements RawBinaryWasmEngine {
  readonly calls: {operation: number; payload: Uint8Array}[] = [];
  closeCalls = 0;
  freeCalls = 0;
  response: Uint8Array | Error = success(SAFE);

  call(operation: number, payload: Uint8Array): Uint8Array {
    this.calls.push({operation, payload: payload.slice()});
    if (operation === WASM_OPERATION.close) {
      this.closeCalls += 1;
    }
    if (this.response instanceof Error) {
      throw this.response;
    }
    return this.response.slice();
  }

  free(): void {
    this.freeCalls += 1;
  }
}

class CallbackPageDevice {
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

class CallbackRawEngine implements RawBinaryWasmEngine {
  static readonly instances: CallbackRawEngine[] = [];
  readonly device: PageDevice;
  transfer: 'read' | 'write' | undefined;
  observedRead: number | undefined;

  constructor(device: PageDevice) {
    this.device = device;
    device.pageCount();
    CallbackRawEngine.instances.push(this);
  }

  call(operation: number, _payload: Uint8Array): Uint8Array {
    if (operation === WASM_OPERATION.close) {
      this.device.close();
      return success(SAFE);
    }
    if (this.transfer === 'read') {
      const page = new Uint8Array(4096);
      this.device.readPage(0, 0, page);
      this.observedRead = page[0];
    } else if (this.transfer === 'write') {
      this.device.writePage(0, 0, new Uint8Array(4096).fill(9));
    }
    return success(SAFE, (bytes) => bytes.u64(1n).fields([]).rows([]));
  }

  free(): void {}
}

describe('binary WASM request codec', () => {
  it('pins the unpublished operation ABI densely', () => {
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

  it('encodes the complete hidden typed schema catalog positionally', () => {
    const typed: WireTableSchema = {
      name: 'typed',
      primaryKey: ['id'],
      columns: [
        {name: 'id', dataType: 'integer', nullable: false},
        {
          name: 'payload',
          dataType: 'json',
          default: {enabled: true, threshold: 1.5},
        },
        {name: 'optional', dataType: 'text', default: null},
      ],
    };
    const actual = encodeDefineTables([typed]);

    const expected = new Bytes()
      .u8(VERSION)
      .u32(1)
      .string('typed')
      .strings(['id'])
      .u32(3)
      .string('id')
      .u8(1)
      .u8(0)
      .u8(0)
      .string('payload')
      .u8(4)
      .u8(1)
      .u8(1)
      .json({enabled: true, threshold: 1.5})
      .string('optional')
      .u8(3)
      .u8(1)
      .u8(0)
      .done();

    expect(actual.operation).toBe(WASM_OPERATION.defineTables);
    expect(actual.mayPublish).toBe(true);
    expect(actual.payload).toEqual(expected);
  });

  it('encodes snapshots, batches, queries, and SQL parameters in Rust order', () => {
    const snapshot = encodeReplaceSnapshot(schema, [
      {id: 1, nested: {zero: -0, list: [true, null]}},
    ]);
    const expectedSnapshot = new Bytes()
      .u8(VERSION)
      .string('items')
      .strings(['id'])
      .u32(0)
      .u32(1)
      .row({id: 1, nested: {zero: -0, list: [true, null]}})
      .done();
    expect(snapshot.payload).toEqual(expectedSnapshot);

    const batch: ChangeBatch = {
      changes: [
        {type: 'upsert', table: 'items', row: {id: 2}},
        {type: 'delete', table: 'items', key: {id: 1}},
      ],
    };
    const expectedBatch = new Bytes()
      .u8(VERSION)
      .u32(2)
      .u8(0)
      .string('items')
      .row({id: 2})
      .u8(1)
      .string('items')
      .row({id: 1})
      .done();
    expect(encodeApplyBatch(batch).payload).toEqual(expectedBatch);

    const expectedQuery = new Bytes()
      .u8(VERSION)
      .string('items')
      .u8(1)
      .strings(['id', 'title'])
      .u32(1)
      .string('id')
      .u8(5)
      .json(1)
      .u32(1)
      .string('id')
      .u8(1)
      .u8(2)
      .u8(1)
      .u32(10)
      .u32(2)
      .done();
    expect(encodeQuery(query).payload).toEqual(expectedQuery);

    const sql = encodeExecuteSql('SELECT $1, $2', [-0, '\ud800']);
    const expectedSql = new Bytes()
      .u8(VERSION)
      .string('SELECT $1, $2')
      .u32(2)
      .json(-0)
      .json('\ud800')
      .done();
    expect(sql.payload).toEqual(expectedSql);

    const prepared = encodePrepareSql('SELECT $1');
    expect(prepared.operation).toBe(WASM_OPERATION.prepareSql);
    expect(prepared.mayPublish).toBe(false);
    expect(prepared.payload).toEqual(
      new Bytes().u8(VERSION).string('SELECT $1').done(),
    );

    const execution = encodeExecutePrepared(7, [1, 'two']);
    expect(execution.operation).toBe(WASM_OPERATION.executePrepared);
    expect(execution.mayPublish).toBe(true);
    expect(execution.payload).toEqual(
      new Bytes().u8(VERSION).u32(7).u32(2).json(1).json('two').done(),
    );

    const close = encodeClosePrepared(7);
    expect(close.operation).toBe(WASM_OPERATION.closePrepared);
    expect(close.mayPublish).toBe(false);
    expect(close.payload).toEqual(new Bytes().u8(VERSION).u32(7).done());

    const script = encodeExecSql('CREATE TABLE items; SELECT * FROM items');
    expect(script.payload).toEqual(
      new Bytes()
        .u8(VERSION)
        .string('CREATE TABLE items; SELECT * FROM items')
        .done(),
    );
  });

  it('rejects sparse arrays, accessors, revoked proxies, and oversized work', () => {
    const sparse = new Array<JsonValue>(1);
    expect(() => encodeExecuteSql('SELECT $1', sparse)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );

    let getterCalls = 0;
    const accessor = Object.defineProperty({}, 'id', {
      enumerable: true,
      get() {
        getterCalls += 1;
        return 1;
      },
    });
    expect(() => encodeReplaceSnapshot(schema, [accessor as Row])).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
    expect(getterCalls).toBe(0);

    const revoked = Proxy.revocable([], {});
    revoked.revoke();
    expect(() => encodeExecuteSql('SELECT 1', revoked.proxy)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );

    expect(() => encodeExecuteSql('SELECT 1', new Array(1_000_001))).toThrow(
      expect.objectContaining({code: 'RESOURCE_LIMIT'}),
    );
    expect(() =>
      encodeExecuteSql('x'.repeat(16 * 1024 * 1024), []),
    ).toThrow(
      expect.objectContaining({code: 'RESOURCE_LIMIT'}),
    );
    for (const invalidId of [0, -1, 1.5, 0x1_0000_0000]) {
      expect(() => encodeExecutePrepared(invalidId, [])).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
      expect(() => encodeClosePrepared(invalidId)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
  });

  it(
    'checks the decoded Rust-model estimate before allocating the wire buffer',
    () => {
      // wasm32 Vec<Value> is 12 bytes and each serde_json::Value slot is 24.
      // Including the retained SQL string, 699,049 nulls are the exact accepted
      // side and the next slot crosses the independent 16 MiB model cap.
      expect(
        encodeExecuteSql('SELECT', new Array<JsonValue>(699_049).fill(null))
          .payload.byteLength,
      ).toBeLessThan(16 * 1024 * 1024);
      expect(() =>
        encodeExecuteSql('SELECT', new Array<JsonValue>(699_050).fill(null)),
      ).toThrow(expect.objectContaining({code: 'RESOURCE_LIMIT'}));
    },
    15_000,
  );

  it('revalidates the second pass and never exposes a wrongly sized allocation', () => {
    let descriptors = 0;
    const target = {id: 'a'};
    const changing = new Proxy(target, {
      ownKeys: () => ['id'],
      getOwnPropertyDescriptor: () => {
        descriptors += 1;
        return {
          configurable: true,
          enumerable: true,
          writable: true,
          value: descriptors <= 2 ? 'a' : 'a much larger second-pass value',
        };
      },
    });
    expect(() => encodeReplaceSnapshot(schema, [changing])).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
  });

  it('accepts JSON depth 64 and rejects the first deeper node', () => {
    const nested = (depth: number): JsonValue => {
      let value: JsonValue = null;
      for (let index = 0; index < depth; index += 1) {
        value = [value];
      }
      return value;
    };
    expect(encodeExecuteSql('SELECT $1', [nested(64)]).payload[0]).toBe(
      VERSION,
    );
    expect(() => encodeExecuteSql('SELECT $1', [nested(65)])).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
  });

  it('preserves missing/null default semantics and rejects null optionals', () => {
    const withNullDefault = {
      name: 'typed',
      primaryKey: ['id'],
      columns: [{name: 'id', dataType: 'integer', default: null}],
    } satisfies WireTableSchema;
    const bytes = encodeDefineTables([withNullDefault]).payload;
    expect(bytes.at(-1)).toBe(0);

    expect(() =>
      encodeQuery({...query, columns: null} as unknown as QueryPlan),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
    expect(() =>
      encodeQuery({...query, unknown: true} as unknown as QueryPlan),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
  });
});

describe('binary WASM response codec', () => {
  it('decodes safe and durable outcomes and requires exact trailing length', () => {
    expect(decodeApplyOutcomeResponse(outcome(7n, ['items'], SAFE))).toEqual({
      disposition: 'safe',
      value: {revision: 7, tables: ['items']},
    });
    expect(decodeApplyOutcomeResponse(outcome(8n, ['items'], DURABLE))).toEqual(
      {
        disposition: 'durable',
        value: {revision: 8, tables: ['items']},
      },
    );

    const trailing = new Uint8Array([...outcome(), 99]);
    expect(() => decodeApplyOutcomeResponse(trailing)).toThrow(
      expect.objectContaining({
        code: 'BRIDGE_SERIALIZATION_ERROR',
        disposition: 'safe',
      }),
    );
  });

  it("keeps revisions within JavaScript's safe integer range", () => {
    const maximumSafe = BigInt(Number.MAX_SAFE_INTEGER);
    expect(
      decodeRevisionResponse(success(SAFE, (bytes) => bytes.u64(maximumSafe)))
        .value,
    ).toBe(Number.MAX_SAFE_INTEGER);
    expect(() =>
      decodeRevisionResponse(
        success(SAFE, (bytes) => bytes.u64(maximumSafe + 1n)),
      ),
    ).toThrow(expect.objectContaining({code: 'BRIDGE_SERIALIZATION_ERROR'}));
  });

  it('decodes only nonzero prepared statement IDs', () => {
    expect(
      decodePreparedStatementIdResponse(
        success(SAFE, (bytes) => bytes.u32(0xffff_ffff)),
      ).value,
    ).toBe(0xffff_ffff);
    expect(() =>
      decodePreparedStatementIdResponse(success(SAFE, (bytes) => bytes.u32(0))),
    ).toThrow(expect.objectContaining({code: 'BRIDGE_SERIALIZATION_ERROR'}));
  });

  it('decodes null-prototype rows, negative zero, and nested JSON exactly', () => {
    const response = success(SAFE, (bytes) =>
      bytes
        .u64(3n)
        .fields([
          {name: '__proto__', dataTypeID: 25},
          {name: 'zero', dataTypeID: 701},
          {name: 'nested', dataTypeID: 114},
        ])
        .rows([
          {
            ['__proto__']: 'safe',
            zero: -0.5,
            nested: [true, {value: null}],
          },
        ]),
    );
    const decoded = decodeQueryResultResponse(response).value;
    expect(decoded.fields).toEqual([
      {name: '__proto__', dataTypeID: 25},
      {name: 'zero', dataTypeID: 701},
      {name: 'nested', dataTypeID: 114},
    ]);
    expect(Object.getPrototypeOf(decoded.rows[0])).toBeNull();
    expect(decoded.rows[0]?.['__proto__']).toBe('safe');
    expect(decoded.rows[0]?.zero).toBe(-0.5);
    expect(decoded.rows[0]?.nested).toEqual([true, {value: null}]);
    expect(
      Object.getPrototypeOf((decoded.rows[0]?.nested as JsonValue[])[1]),
    ).toBeNull();
  });

  it('decodes every result in a multi-statement SQL response', () => {
    const decoded = decodeSqlResultsResponse(
      success(SAFE, (bytes) =>
        bytes
          .u32(2)
          .string('CREATE')
          .u64(1n)
          .u64(0n)
          .fields([])
          .rows([])
          .strings(['items'])
          .string('SELECT')
          .u64(1n)
          .u64(1n)
          .fields([{name: 'id', dataTypeID: 20}])
          .rows([{id: 1}])
          .strings([]),
      ),
    ).value;

    expect(decoded).toEqual([
      {
        command: 'CREATE',
        revision: 1,
        rowCount: 0,
        fields: [],
        rows: [],
        tables: ['items'],
      },
      {
        command: 'SELECT',
        revision: 1,
        rowCount: 1,
        fields: [{name: 'id', dataTypeID: 20}],
        rows: [{id: 1}],
        tables: [],
      },
    ]);
  });

  it('preserves a leading U+FEFF as ordinary string data', () => {
    const decoded = decodeQueryResultResponse(
      success(SAFE, (bytes) =>
        bytes
          .u64(1n)
          .fields([{name: 'leadingBom', dataTypeID: 25}])
          .rows([{leadingBom: '\ufeffdata'}]),
      ),
    ).value;
    expect(decoded.rows[0]?.leadingBom).toBe('\ufeffdata');
  });

  it('bounds the decoded model prospectively and retains disposition', () => {
    const oversized = (disposition: 0 | 1): Uint8Array => {
      const rowCount = 1_000_000;
      const bytes = new Uint8Array(3 + 8 + 4 + 4 + rowCount * 4);
      bytes.set([VERSION, SUCCESS, disposition]);
      new DataView(bytes.buffer).setUint32(15, rowCount, true);
      // Every remaining u32 is an empty row. The 4 MiB wire is valid, but the
      // prospective vector plus one million JS objects exceeds 16 MiB.
      return bytes;
    };
    for (const disposition of [SAFE, DURABLE] as const) {
      expect(() => decodeQueryResultResponse(oversized(disposition))).toThrow(
        expect.objectContaining({
          code: 'BRIDGE_SERIALIZATION_ERROR',
          disposition: disposition === DURABLE ? 'durable' : 'safe',
        }),
      );
    }
  });

  it.each([
    [0, undefined],
    [1, false],
    [2, true],
  ] as const)('decodes retryability tag %i', (tag, retryable) => {
    expect(() => decodeUnitResponse(failure('TEST', 'message', tag))).toThrow(
      expect.objectContaining({code: 'TEST', message: 'message', retryable}),
    );
  });

  it('rejects malformed versions, UTF-8, duplicate keys, and durable errors', () => {
    expect(() => decodeUnitResponse(new Uint8Array([2, 0, 0]))).toThrow(
      WasmWireDecodeError,
    );
    const invalidUtf8 = new Bytes()
      .u8(VERSION)
      .u8(FAILURE)
      .u8(SAFE)
      .u32(1)
      .u8(0xff)
      .string('message')
      .u8(0)
      .done();
    expect(() => decodeUnitResponse(invalidUtf8)).toThrow(WasmWireDecodeError);

    const duplicate = success(SAFE, (bytes) =>
      bytes
        .u64(1n)
        .fields([])
        .u32(1)
        .u32(2)
        .string('id')
        .json(1)
        .string('id')
        .json(2),
    );
    expect(() => decodeQueryResultResponse(duplicate)).toThrow(
      WasmWireDecodeError,
    );

    const durableFailure = failure('TEST', 'message');
    durableFailure[2] = DURABLE;
    expect(() => decodeUnitResponse(durableFailure)).toThrow(
      expect.objectContaining({disposition: 'durable'}),
    );
  });

  it('normalizes constructor Uint8Array failures without losing retryability', () => {
    const normalized = normalizeWasmConstructorError(
      failure('STORAGE_LOCKED', 'locked', 2),
    );
    expect(normalized).toEqual(
      expect.objectContaining({
        code: 'STORAGE_LOCKED',
        message: 'locked',
        retryable: true,
      }),
    );
  });
});

describe('binary WorkerEngine adapter', () => {
  it('normalizes constructor envelopes through the guarded production factory', () => {
    const ThrowingRaw = class {
      constructor(_device: PageDevice) {
        throw failure('STORAGE_LOCKED', 'locked', 2);
      }
    } as unknown as RawBinaryWasmEngineConstructor;
    expect(() =>
      createBinaryWasmEngine(ThrowingRaw, new CallbackPageDevice()),
    ).toThrow(
      expect.objectContaining({
        code: 'STORAGE_LOCKED',
        message: 'locked',
        retryable: true,
      }),
    );
  });

  it('blocks same-engine, cross-engine, close, and constructor reentry from every page callback', () => {
    CallbackRawEngine.instances.length = 0;
    const firstDevice = new CallbackPageDevice();
    const secondDevice = new CallbackPageDevice();
    const first = createBinaryWasmEngine(CallbackRawEngine, firstDevice);
    const second = createBinaryWasmEngine(CallbackRawEngine, secondDevice);
    const firstRaw = CallbackRawEngine.instances[0]!;
    const errors: unknown[] = [];
    firstDevice.callback = () => {
      for (const reenter of [
        () => first.query(query),
        () => second.query(query),
        () => first.close(),
        () => second.close(),
        () =>
          createBinaryWasmEngine(CallbackRawEngine, new CallbackPageDevice()),
      ]) {
        try {
          reenter();
          errors.push(new Error('reentrant operation unexpectedly succeeded'));
        } catch (error) {
          errors.push(error);
        }
      }
    };

    firstRaw.transfer = 'read';
    expect(first.query(query).rows).toEqual([]);
    expect(firstRaw.observedRead).toBe(7);
    expect(CallbackRawEngine.instances).toHaveLength(2);
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
    firstRaw.transfer = 'write';
    expect(first.query(query).rows).toEqual([]);
    expect(firstDevice.written).toHaveLength(1);
    expect(firstDevice.written[0]?.[0]).toBe(9);
    expect(errors).toHaveLength(5);

    // Rejected nested closes did not mutate either adapter lifecycle.
    firstDevice.callback = undefined;
    expect(second.query(query).rows).toEqual([]);
    first.close();
    second.close();
    expect(firstDevice.closed).toBe(1);
    expect(secondDevice.closed).toBe(1);
  });

  it('preserves the public WorkerEngine surface and operation mapping', () => {
    const raw = new FakeRawEngine();
    const engine = new BinaryWasmEngine(raw);

    raw.response = success(SAFE);
    engine.defineTables([]);
    expect(raw.calls.at(-1)?.operation).toBe(WASM_OPERATION.defineTables);

    raw.response = outcome(1n, ['items'], DURABLE);
    expect(engine.applyBatch({changes: []})).toEqual({
      revision: 1,
      tables: ['items'],
    });
    expect(raw.calls.at(-1)?.operation).toBe(WASM_OPERATION.applyBatch);

    raw.response = success(SAFE, (bytes) => bytes.u8(1));
    expect(engine.inTransaction()).toBe(true);

    raw.response = success(SAFE, (bytes) => bytes.u64(3n));
    expect(engine.revision()).toBe(3);

    raw.response = success(SAFE, (bytes) => bytes.u32(0));
    expect(engine.execSql('SELECT 1; SELECT 2')).toEqual([]);
    expect(raw.calls.at(-1)?.operation).toBe(WASM_OPERATION.execSql);

    raw.response = success(SAFE, (bytes) => bytes.u32(9));
    expect(engine.prepareSql('SELECT $1')).toBe(9);
    expect(raw.calls.at(-1)?.operation).toBe(WASM_OPERATION.prepareSql);

    raw.response = success(SAFE, (bytes) =>
      bytes
        .string('SELECT')
        .u64(3n)
        .u64(1n)
        .fields([{name: 'id', dataTypeID: 20}])
        .rows([{id: 3}])
        .strings([]),
    );
    expect(engine.executePrepared(9, [3]).rows).toEqual([{id: 3}]);
    expect(raw.calls.at(-1)?.operation).toBe(WASM_OPERATION.executePrepared);

    raw.response = success(SAFE);
    engine.closePrepared(9);
    expect(raw.calls.at(-1)?.operation).toBe(WASM_OPERATION.closePrepared);
  });

  it('does not poison a potential mutation when a trusted SAFE payload is malformed', () => {
    const raw = new FakeRawEngine();
    const engine = new BinaryWasmEngine(raw);
    raw.response = success(SAFE); // Missing SqlResult payload.

    expect(() => engine.executeSql('UPDATE items SET id = id', [])).toThrow(
      WasmWireDecodeError,
    );
    expect(raw.closeCalls).toBe(0);
  });

  it('poisons on DURABLE or unknown malformed results after potential publication', () => {
    for (const response of [success(DURABLE), new Uint8Array([99])]) {
      const raw = new FakeRawEngine();
      raw.response = response;
      const engine = new BinaryWasmEngine(raw);
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
      expect(raw.calls.at(-1)).toEqual({
        operation: WASM_OPERATION.close,
        payload: new Uint8Array([VERSION]),
      });
      expect(() => engine.revision()).toThrow(
        expect.objectContaining({
          code: 'STORAGE_ENGINE_POISONED',
          retryable: false,
        }),
      );
      engine.close();
      expect(() => engine.revision()).toThrow(
        expect.objectContaining({code: 'ENGINE_CLOSED'}),
      );
    }
  });

  it('keeps malformed read results nonfatal but conservatively poisons defineTables', () => {
    const queryRaw = new FakeRawEngine();
    queryRaw.response = new Uint8Array([99]);
    const queryEngine = new BinaryWasmEngine(queryRaw);
    expect(() => queryEngine.query(query)).toThrow(WasmWireDecodeError);
    expect(queryRaw.closeCalls).toBe(0);

    const defineRaw = new FakeRawEngine();
    defineRaw.response = new Uint8Array([99]);
    const defineEngine = new BinaryWasmEngine(defineRaw);
    expect(() => defineEngine.defineTables([])).toThrow(
      expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
    );
    expect(defineRaw.closeCalls).toBe(1);
  });

  it('closes immediately on fatal remote storage outcomes', () => {
    for (const code of [
      'RECOVERY_REQUIRED',
      'STORAGE_COMMIT_OUTCOME_UNKNOWN',
      'STORAGE_ENGINE_POISONED',
    ]) {
      const raw = new FakeRawEngine();
      raw.response = failure(code, 'reopen', 1);
      const engine = new BinaryWasmEngine(raw);
      expect(() => engine.query(query)).toThrow(
        expect.objectContaining({code}),
      );
      expect(raw.closeCalls).toBe(1);
      expect(raw.freeCalls).toBe(1);
      expect(() => engine.revision()).toThrow(
        expect.objectContaining({
          code: 'STORAGE_ENGINE_POISONED',
          retryable: false,
        }),
      );
      engine.close();
      expect(() => engine.revision()).toThrow(
        expect.objectContaining({code: 'ENGINE_CLOSED'}),
      );
      expect(raw.closeCalls).toBe(1);
      expect(raw.freeCalls).toBe(1);
    }
  });

  it('closes and frees exactly once through the explicit lifecycle', () => {
    const raw = new FakeRawEngine();
    const engine = new BinaryWasmEngine(raw);
    engine.close();
    engine.close();
    expect(raw.closeCalls).toBe(1);
    expect(raw.freeCalls).toBe(1);
  });
});
