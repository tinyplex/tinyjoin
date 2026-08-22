import {existsSync, readFileSync} from 'node:fs';
import {resolve} from 'node:path';
import {pathToFileURL} from 'node:url';

import {describe, expect, it} from 'vitest';

import type {JsonValue} from '../../src/protocol.js';
import type {PageDevice} from '../../src/worker/page-device.js';
import {
  WASM_OPERATION,
  createStructuredWasmEngine,
  type RawStructuredWasmEngine,
  type RawStructuredWasmEngineConstructor,
} from '../../src/worker/wasm-bridge.js';

const BRIDGE_VERSION = 2;
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
  it('round-trips every SQL-first operation against the real WASM artifact', async () => {
    const wasm = await loadStructuredModule();
    const device = new MemoryPageDevice();
    const {engine, recording} = createRecordingEngine(wasm, device);

    const schemaSql = `CREATE TABLE items (
      id INTEGER PRIMARY KEY,
      title TEXT NOT NULL DEFAULT 'typed-default',
      payload JSONB
    )`;
    const created = engine.execSql(schemaSql);
    expectRequest(recording, WASM_OPERATION.execSql).toBe(schemaSql);
    expect(created).toBe(
      responsePayload(lastCall(recording, WASM_OPERATION.execSql)),
    );
    expect(created[0]?.command).toBe('CREATE TABLE');
    expectDisposition(recording, WASM_OPERATION.execSql, 'durable');

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
    const insertParams = [1, protoValue] satisfies JsonValue[];
    const inserted = engine.executeSql(
      'INSERT INTO items (id, payload) VALUES ($1, $2)',
      insertParams,
    );
    const insertCall = lastCall(recording, WASM_OPERATION.executeSql);
    expect((insertCall.payload as {params: JsonValue[]}).params).toBe(
      insertParams,
    );
    expect(inserted).toBe(responsePayload(insertCall));
    expectDisposition(recording, WASM_OPERATION.executeSql, 'durable');

    const selected = engine.executeSql(
      'SELECT id, title, payload FROM items WHERE id = $1',
      [1],
    );
    expectDisposition(recording, WASM_OPERATION.executeSql, 'safe');
    expect(selected.fields).toEqual([
      {name: 'id', dataTypeID: 20},
      {name: 'title', dataTypeID: 25},
      {name: 'payload', dataTypeID: 114},
    ]);
    const selectedRow = selected.rows[0]!;
    const selectedPayload = selectedRow.payload as Record<string, JsonValue>;
    expect(Object.getPrototypeOf(selectedRow)).toBeNull();
    expect(Object.getPrototypeOf(selectedPayload)).toBeNull();
    expect(Object.hasOwn(selectedPayload, '__proto__')).toBe(true);
    expect(selectedPayload.__proto__).toBe('data');
    expect(Object.is(selectedPayload.signedZero, -0)).toBe(false);
    const nested = selectedPayload.nested as JsonValue[];
    const nestedRecord = nested[3] as Record<string, JsonValue>;
    expect(Object.getPrototypeOf(nestedRecord)).toBeNull();
    expect(Object.hasOwn(nestedRecord, '__proto__')).toBe(true);
    expect(nestedRecord.__proto__).toBe('nested-data');

    engine.beginTransaction();
    expectDisposition(recording, WASM_OPERATION.begin, 'safe');
    expect(engine.inTransaction()).toBe(true);
    engine.executeSql('INSERT INTO items (id, title) VALUES ($1, $2)', [
      2,
      'rolled back',
    ]);
    expectDisposition(recording, WASM_OPERATION.executeSql, 'safe');
    engine.rollbackTransaction();
    expectDisposition(recording, WASM_OPERATION.rollback, 'safe');
    expect(
      engine.executeSql('SELECT id FROM items WHERE id = $1', [2]).rows,
    ).toHaveLength(0);

    engine.beginTransaction();
    engine.executeSql('INSERT INTO items (id, title) VALUES ($1, $2)', [
      3,
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
    const preparedParams = [3] satisfies JsonValue[];
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
    expect(
      recording.calls.every(
        ({bridgeVersion}) => bridgeVersion === BRIDGE_VERSION,
      ),
    ).toBe(true);

    const callCount = recording.calls.length;
    const closedError = captureError(() =>
      engine.executeSql('SELECT id FROM items', []),
    );
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
  expect(response.slice(0, 3)).toEqual([BRIDGE_VERSION, 0, expectedTag]);
}

function expectFailureDisposition(
  engine: RecordingStructuredEngine,
  operation: number,
): void {
  const response = lastCall(engine, operation).response as unknown[];
  expect(response.slice(0, 3)).toEqual([BRIDGE_VERSION, 1, 0]);
}

function captureError(operation: () => unknown): unknown {
  try {
    operation();
  } catch (error) {
    return error;
  }
  throw new Error('Expected the structured bridge operation to fail');
}
