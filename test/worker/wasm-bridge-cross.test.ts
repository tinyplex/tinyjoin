import {existsSync, readFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {resolve} from 'node:path';

import {describe, expect, it} from 'vitest';

import type {
  JsonValue,
  QueryPlan,
  TableSchema,
} from '../../src/protocol.js';
import type {PageDevice} from '../../src/worker/page-device.js';
import {
  WASM_OPERATION,
  createStructuredWasmEngine,
  type RawStructuredWasmEngine,
  type RawStructuredWasmEngineConstructor,
} from '../../src/worker/wasm-bridge.js';

const artifactDirectory =
  process.env.TINYGRES_PAGED_WASM_DIR ?? resolve('dist/wasm');
const artifactModule = `${artifactDirectory}/tinygres_wasm.js`;
const artifactWasm = `${artifactDirectory}/tinygres_wasm_bg.wasm`;

interface StructuredModule {
  default(options: {module_or_path: Uint8Array}): Promise<unknown>;
  WasmEngine: RawStructuredWasmEngineConstructor;
}

type StructuredCall = {
  bridgeVersion: number;
  operation: number;
  payload: unknown;
  response: unknown;
};

type TypedTableSchema = TableSchema & {
  columns: Array<{
    name: string;
    dataType: 'boolean' | 'integer' | 'float' | 'text' | 'json';
    nullable?: boolean;
    default?: JsonValue;
  }>;
};

class RecordingStructuredEngine implements RawStructuredWasmEngine {
  readonly calls: StructuredCall[] = [];
  freeCalls = 0;

  constructor(readonly raw: RawStructuredWasmEngine) {}

  callStructured(
    bridgeVersion: number,
    operation: number,
    payload: unknown,
  ): unknown {
    const response = this.raw.callStructured(
      bridgeVersion,
      operation,
      payload,
    );
    this.calls.push({bridgeVersion, operation, payload, response});
    return response;
  }

  free(): void {
    this.freeCalls += 1;
    this.raw.free?.();
  }
}

class MemoryPageDevice implements PageDevice {
  readonly pages: Uint8Array[] = [];
  closed = 0;

  pageCount(): number {
    return this.pages.length;
  }

  readPage(low: number, high: number, destination: Uint8Array): number {
    const page = high === 0 ? this.pages[low] : undefined;
    if (page === undefined) {
      throw new Error('missing test page');
    }
    destination.set(page);
    return destination.byteLength;
  }

  writePage(low: number, high: number, source: Uint8Array): number {
    if (high !== 0 || low > this.pages.length) {
      throw new Error('invalid test page write');
    }
    this.pages[low] = source.slice();
    return source.byteLength;
  }

  flush(): void {}

  close(): void {
    this.closed += 1;
  }
}

const runIfArtifactExists =
  existsSync(artifactModule) && existsSync(artifactWasm)
    ? describe
    : describe.skip;

runIfArtifactExists('structured TypeScript/Rust bridge contract', () => {
  it('round-trips every direct structured operation against the real WASM artifact', async () => {
    const wasm = await loadStructuredModule();
    const device = new MemoryPageDevice();
    const {engine, recording} = createRecordingEngine(wasm, device);
    const schema: TypedTableSchema = {
      name: 'items',
      primaryKey: ['id'],
      columns: [
        {name: 'id', dataType: 'integer', nullable: false},
        {
          name: 'title',
          dataType: 'text',
          nullable: false,
          default: 'typed-default',
        },
        {
          name: 'payload',
          dataType: 'json',
          nullable: undefined,
          default: undefined,
        } as unknown as TypedTableSchema['columns'][number],
      ],
    };
    const schemas = [
      schema as TableSchema,
      {
        name: 'untyped_items',
        primaryKey: ['id'],
        columns: undefined,
      } as unknown as TableSchema,
    ];

    engine.defineTables(schemas);
    expectRequest(recording, WASM_OPERATION.defineTables).toBe(schemas);
    expectDisposition(recording, WASM_OPERATION.defineTables, 'durable');

    // Repeating an identical definition is a no-op and therefore SAFE.
    engine.defineTables(schemas);
    expectDisposition(recording, WASM_OPERATION.defineTables, 'safe');

    const protoValue = {
      ['__proto__']: 'data',
      signedZero: -0,
      nested: [
        true,
        null,
        1.25,
        {['__proto__']: 'nested-data', values: ['one', false]},
      ],
    } satisfies JsonValue;
    expect(Object.hasOwn(protoValue, '__proto__')).toBe(true);
    const batch = {
      changes: [
        {
          type: 'upsert' as const,
          table: 'items',
          row: {id: 1, payload: protoValue},
        },
      ],
    };
    const applied = engine.applyBatch(batch);
    const applyCall = lastCall(recording, WASM_OPERATION.applyBatch);
    expect(applyCall.payload).toBe(batch);
    expect(applied).toBe(responsePayload(applyCall));
    expect(applied.tables).toEqual(['items']);
    expectDisposition(recording, WASM_OPERATION.applyBatch, 'durable');

    const plan: QueryPlan = {
      table: 'items',
      columns: ['id', 'title', 'payload'],
      filters: [{column: 'id', operator: 'eq', value: 1}],
      orderBy: [{column: 'id', direction: 'desc', nulls: 'last'}],
      limit: 1,
      offset: 0,
    };
    const planned = engine.query(plan);
    const queryCall = lastCall(recording, WASM_OPERATION.query);
    expect(queryCall.payload).toBe(plan);
    expect(planned).toBe(responsePayload(queryCall));
    expectDisposition(recording, WASM_OPERATION.query, 'safe');
    expect(planned.fields).toEqual([
      {name: 'id', dataTypeID: 20},
      {name: 'title', dataTypeID: 25},
      {name: 'payload', dataTypeID: 114},
    ]);
    const plannedRow = planned.rows[0]!;
    const plannedPayload = plannedRow.payload as Record<string, JsonValue>;
    expect(Object.getPrototypeOf(plannedRow)).toBeNull();
    expect(Object.getPrototypeOf(plannedPayload)).toBeNull();
    expect(Object.hasOwn(plannedPayload, '__proto__')).toBe(true);
    expect(plannedPayload.__proto__).toBe('data');
    expect(Object.is(plannedPayload.signedZero, -0)).toBe(false);
    const nested = plannedPayload.nested as JsonValue[];
    const nestedRecord = nested[3] as Record<string, JsonValue>;
    expect(Object.getPrototypeOf(nestedRecord)).toBeNull();
    expect(Object.hasOwn(nestedRecord, '__proto__')).toBe(true);
    expect(nestedRecord.__proto__).toBe('nested-data');

    expect(
      engine.query(
        {
          table: 'items',
          filters: [],
          columns: undefined,
          orderBy: undefined,
          limit: undefined,
          offset: undefined,
        } as unknown as QueryPlan,
      ).rows,
    ).toHaveLength(1);

    engine.beginTransaction();
    expectDisposition(recording, WASM_OPERATION.begin, 'safe');
    expect(engine.inTransaction()).toBe(true);
    const stagedParams = [
      2,
      'staged',
      {nested: [true, null, 1.25]},
    ] satisfies JsonValue[];
    const staged = engine.executeSql(
      'INSERT INTO items (id, title, payload) VALUES ($1, $2, $3)',
      stagedParams,
    );
    const stagedCall = lastCall(recording, WASM_OPERATION.executeSql);
    expect((stagedCall.payload as {params: JsonValue[]}).params).toBe(
      stagedParams,
    );
    expect(staged).toBe(responsePayload(stagedCall));
    expectDisposition(recording, WASM_OPERATION.executeSql, 'safe');
    expect(
      engine.executeSql('SELECT * FROM items ORDER BY id', []).rows,
    ).toHaveLength(2);
    engine.rollbackTransaction();
    expectDisposition(recording, WASM_OPERATION.rollback, 'safe');
    expect(engine.executeSql('SELECT * FROM items', []).rows).toHaveLength(1);

    engine.beginTransaction();
    engine.executeSql('INSERT INTO items (id, title) VALUES ($1, $2)', [
      4,
      'committed',
    ]);
    const committed = engine.commitTransaction();
    expect(committed.tables).toEqual(['items']);
    expectDisposition(recording, WASM_OPERATION.commit, 'durable');
    expect(engine.inTransaction()).toBe(false);

    const preparedSql = 'SELECT title, payload FROM items WHERE id = $1';
    const statementId = engine.prepareSql(preparedSql);
    expect(statementId).toBeGreaterThan(0);
    expectRequest(recording, WASM_OPERATION.prepareSql).toBe(preparedSql);
    const preparedParams = [4] satisfies JsonValue[];
    const prepared = engine.executePrepared(statementId, preparedParams);
    const preparedCall = lastCall(
      recording,
      WASM_OPERATION.executePrepared,
    );
    expect((preparedCall.payload as {params: JsonValue[]}).params).toBe(
      preparedParams,
    );
    expect(prepared).toBe(responsePayload(preparedCall));
    expect(prepared.rows).toEqual([{title: 'committed', payload: null}]);
    expectDisposition(recording, WASM_OPERATION.executePrepared, 'safe');
    engine.closePrepared(statementId);
    engine.closePrepared(statementId);
    const preparedError = captureError(() =>
      engine.executePrepared(statementId, preparedParams),
    );
    expect(preparedError).toMatchObject({
      code: 'PREPARED_STATEMENT_NOT_FOUND',
    });
    expectFailureDisposition(recording, WASM_OPERATION.executePrepared);

    const script =
      'SELECT id FROM items LIMIT 0; SELECT COUNT(*) AS total FROM items LIMIT 0';
    const scripted = engine.execSql(script);
    const execCall = lastCall(recording, WASM_OPERATION.execSql);
    expect(execCall.payload).toBe(script);
    expect(scripted).toBe(responsePayload(execCall));
    expect(scripted).toHaveLength(2);
    expectDisposition(recording, WASM_OPERATION.execSql, 'safe');

    const replacementRows = [
      {
        id: 3,
        title: 'snapshot',
        payload: {['__proto__']: 'replacement', wide: ['ok', 3]},
      },
    ];
    const replacement = engine.replaceTableSnapshot(
      schema as TableSchema,
      replacementRows,
    );
    const replacementCall = lastCall(
      recording,
      WASM_OPERATION.replaceSnapshot,
    );
    expect((replacementCall.payload as {rows: unknown}).rows).toBe(
      replacementRows,
    );
    expect(replacement).toBe(responsePayload(replacementCall));
    expectDisposition(recording, WASM_OPERATION.replaceSnapshot, 'durable');

    const sqlError = captureError(() =>
      engine.executeSql('SELECT * FROM missing_table', []),
    );
    expect(sqlError).toMatchObject({code: 'TABLE_NOT_FOUND'});
    expectFailureDisposition(recording, WASM_OPERATION.executeSql);
    expect(engine.revision()).toBeGreaterThan(0);

    engine.close();
    engine.close();
    expect(device.closed).toBe(1);
    expect(recording.freeCalls).toBe(1);
    expect(
      recording.calls.filter(
        ({operation}) => operation === WASM_OPERATION.close,
      ),
    ).toHaveLength(1);
    expectDisposition(recording, WASM_OPERATION.close, 'safe');

    const callCount = recording.calls.length;
    const closedError = captureError(() => engine.query(plan));
    expect(closedError).toMatchObject({code: 'ENGINE_CLOSED'});
    expect(recording.calls).toHaveLength(callCount);
  });
});

async function loadStructuredModule(): Promise<StructuredModule> {
  const wasm = (await import(
    /* @vite-ignore */ pathToFileURL(artifactModule).href
  )) as StructuredModule;
  await wasm.default({module_or_path: readFileSync(artifactWasm)});
  return wasm;
}

function createRecordingEngine(
  wasm: StructuredModule,
  device: PageDevice,
): {
  engine: ReturnType<typeof createStructuredWasmEngine>;
  recording: RecordingStructuredEngine;
} {
  let recording: RecordingStructuredEngine | undefined;
  class RecordingConstructor extends RecordingStructuredEngine {
    constructor(guardedDevice: PageDevice) {
      super(new wasm.WasmEngine(guardedDevice));
      recording = this;
    }
  }
  const engine = createStructuredWasmEngine(RecordingConstructor, device);
  if (!recording) {
    throw new Error('The structured WASM recording engine was not created');
  }
  return {engine, recording};
}

function lastCall(
  engine: RecordingStructuredEngine,
  operation: number,
): StructuredCall {
  for (let index = engine.calls.length - 1; index >= 0; index -= 1) {
    const call = engine.calls[index];
    if (call?.operation === operation) {
      return call;
    }
  }
  throw new Error(
    `No structured WASM call recorded for operation ${operation}`,
  );
}

function expectRequest(
  engine: RecordingStructuredEngine,
  operation: number,
) {
  return expect(lastCall(engine, operation).payload);
}

function responsePayload(call: StructuredCall): unknown {
  expect(Array.isArray(call.response)).toBe(true);
  return (call.response as unknown[])[3];
}

function expectDisposition(
  engine: RecordingStructuredEngine,
  operation: number,
  expected: 'safe' | 'durable',
): void {
  const expectedTag = expected === 'durable' ? 1 : 0;
  const response = lastCall(engine, operation).response as unknown[];
  expect(response.slice(0, 3)).toEqual([1, 0, expectedTag]);
}

function expectFailureDisposition(
  engine: RecordingStructuredEngine,
  operation: number,
): void {
  const response = lastCall(engine, operation).response as unknown[];
  expect(response.slice(0, 3)).toEqual([1, 1, 0]);
}

function captureError(operation: () => unknown): unknown {
  try {
    operation();
  } catch (error) {
    return error;
  }
  throw new Error('Expected the structured bridge operation to fail');
}
