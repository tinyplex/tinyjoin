import {existsSync, readFileSync} from 'node:fs';
import {resolve} from 'node:path';
import {pathToFileURL} from 'node:url';
import {compileFunction} from 'node:vm';

import {describe, expect, it} from 'vitest';
import {transformSync} from 'esbuild';

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
  process.env.TINYJOIN_PAGED_WASM_DIR ?? resolve('dist/wasm');
const artifactModule = `${artifactDirectory}/tinyjoin_wasm.js`;
const artifactWasm = `${artifactDirectory}/tinyjoin_wasm_bg.wasm`;

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
  it('executes the documented join boundaries against the real engine', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    const section = readFileSync('site/guides/3_sql_compatibility.md', 'utf8')
      .split('### Join projection and identifier boundaries')[1]!
      .split('### Join work budgets')[0]!;
    const examples = [...section.matchAll(/```sql\n([\s\S]*?)```/g)].map((match) => match[1]!);
    expect(examples).toHaveLength(6);
    try {
      engine.execSql(examples[0]!);
      expect(engine.executeSql(examples[1]!, []).rows).toEqual([{id: 1}]);
      expect(engine.executeSql(examples[2]!, []).rows).toEqual([{id: 1, 'extra.value': 'kept'}]);
      for (const [index, sql] of examples.slice(3).entries()) {
        expect(captureError(() => engine.executeSql(sql, [])), sql)
          .toMatchObject({code: index === 1 ? 'SQL_PARSE_ERROR' : 'UNSUPPORTED_SQL'});
      }
    } finally {
      engine.close();
    }
  });

  it('round-trips the documented application backup and rejects unsafe restores', async () => {
    const {engine, db, backupNotes, restoreNotes} = await createDocumentedBackupFixture();
    const notes = [
      {id: 'stable-a', body: "'); DROP TABLE notes; --", pinned: true},
      {id: 'stable-b', body: 'retained', pinned: false},
    ];
    const backup = JSON.stringify({schemaVersion: 1, notes});
    try {
      await restoreNotes(db, backup);
      expect(JSON.parse(await backupNotes(db))).toEqual({schemaVersion: 1, notes});
      await expect(restoreNotes(db, backup)).rejects.toThrow('empty notes table');
      engine.execSql('DELETE FROM notes');
      await expect(restoreNotes(db, JSON.stringify({schemaVersion: 2, notes}))).rejects.toThrow('Invalid notes backup');
      await expect(restoreNotes(db, JSON.stringify({schemaVersion: 1, notes: [{...notes[0], pinned: 'true'}]}))).rejects.toThrow('Invalid notes backup');
      await expect(restoreNotes(db, JSON.stringify({schemaVersion: 1, notes: [notes[0], notes[0]]}))).rejects.toMatchObject({code: 'CONSTRAINT_VIOLATION'});
      expect(engine.executeSql('SELECT id FROM notes', []).rows).toEqual([]);
      await restoreNotes(db, backup);
      expect(JSON.parse(await backupNotes(db))).toEqual({schemaVersion: 1, notes});
    } finally {
      engine.close();
    }
  });

  it('enforces the documented backup and restore row caps at the real WASM boundary', async () => {
    const {engine, db, backupNotes, restoreNotes} = await createDocumentedBackupFixture();
    try {
      await restoreNotes(db, JSON.stringify({schemaVersion: 1, notes: []}));
      const oversized = {schemaVersion: 1, notes: Array.from({length: 1001}, (_, id) => ({id: `note-${id}`, body: 'small', pinned: false}))};
      await expect(restoreNotes(db, JSON.stringify(oversized))).rejects.toThrow('Invalid notes backup');

      // This test measures the example's row cap, not 1,001 separate commits.
      // Seed full-size data in bounded batches below the engine's 1,024-parameter
      // and 4,096-token statement limits.
      for (let offset = 0; offset < 1000; offset += 250) {
        const batch = oversized.notes.slice(offset, offset + 250);
        const values = batch.map((_, index) =>
          `($${index * 3 + 1}, $${index * 3 + 2}, $${index * 3 + 3})`,
        ).join(', ');
        engine.executeSql(
          `INSERT INTO notes VALUES ${values}`,
          batch.flatMap((note) => [note.id, note.body, note.pinned]),
        );
      }
      expect(JSON.parse(await backupNotes(db)).notes).toHaveLength(1000);
      const last = oversized.notes[1000]!;
      engine.executeSql('INSERT INTO notes VALUES ($1, $2, $3)', [last.id, last.body, last.pinned]);
      await expect(backupNotes(db)).rejects.toThrow('larger-data policy');
    } finally {
      engine.close();
    }
  });

  it('rejects duplicate projections independently of matching rows', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql('CREATE TABLE items (id INTEGER PRIMARY KEY)');
      const queries = [
        'SELECT id, id FROM items',
        'SELECT id, id FROM items WHERE id = 99',
        'SELECT id, id FROM items ORDER BY id',
        'SELECT id, id FROM items LIMIT 0',
      ];
      const prepared = queries.map(sql => engine.prepareSql(sql));
      for (const populated of [false, true]) {
        if (populated) {
          engine.executeSql('INSERT INTO items VALUES (1)', []);
        }
        const revision = engine.revision();
        for (const [index, sql] of queries.entries()) {
          expect(
            captureError(() => engine.executeSql(sql, [])),
            sql,
          ).toMatchObject({code: 'INVALID_QUERY'});
          expect(
            captureError(() => engine.executePrepared(prepared[index]!, [])),
            sql,
          ).toMatchObject({code: 'INVALID_QUERY'});
        }
        expect(engine.revision()).toBe(revision);
        expect(engine.executeSql('SELECT id FROM items', []).rows).toEqual(
          populated ? [{id: 1}] : [],
        );
      }
    } finally {
      engine.close();
    }
  });

  it('rejects duplicate RETURNING columns before changing durable or staged rows', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT); INSERT INTO items VALUES (1, 'original')",
      );
      const statements = [
        "INSERT INTO items VALUES (2, 'new') RETURNING id, id",
        "UPDATE items SET title = 'changed' WHERE id = 1 RETURNING id, id",
        "UPDATE items SET title = 'changed' WHERE id = 99 RETURNING id, id",
        'DELETE FROM items WHERE id = 1 RETURNING id, id',
        'DELETE FROM items WHERE id = 99 RETURNING id, id',
      ];
      const prepared = statements.map(sql => engine.prepareSql(sql));
      const revision = engine.revision();
      for (const inTransaction of [false, true]) {
        if (inTransaction) engine.beginTransaction();
        for (const [index, sql] of statements.entries()) {
          for (const run of [
            () => engine.executeSql(sql, []),
            () => engine.executePrepared(prepared[index]!, []),
          ]) {
            expect(captureError(run), sql).toMatchObject({code: 'INVALID_QUERY'});
            expect(engine.executeSql('SELECT * FROM items', []).rows).toEqual([
              {id: 1, title: 'original'},
            ]);
          }
        }
        if (inTransaction) engine.commitTransaction();
        expect(engine.revision()).toBe(revision);
      }
    } finally {
      engine.close();
    }
  });

  it('rejects JSON pagination values without confusing prepared markers with user data', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql('CREATE TABLE items (id INTEGER PRIMARY KEY, payload JSON)');
      const marker = {['\0tinyjoin:parameter']: 1} satisfies JsonValue;
      const insert = engine.prepareSql('INSERT INTO items VALUES ($1, $2)');
      engine.executeSql('INSERT INTO items VALUES ($1, $2)', [1, marker]);
      engine.executePrepared(insert, [2, marker]);
      const selects = [
        {sql: 'SELECT id FROM items ORDER BY id', rows: [{id: 1}, {id: 2}]},
        {
          sql: 'SELECT id, COUNT(*) AS count FROM items GROUP BY id ORDER BY id',
          rows: [{id: 1, count: 1}, {id: 2, count: 1}],
        },
        {
          sql: 'SELECT a.id AS id FROM items a JOIN items b ON a.id = b.id ORDER BY id',
          rows: [{id: 1}, {id: 2}],
        },
      ];
      const revision = engine.revision();
      for (const {sql: select, rows} of selects) {
        for (const clause of ['LIMIT', 'OFFSET']) {
          const sql = `${select} ${clause} $1`;
          const statement = engine.prepareSql(sql);
          const expected = clause === 'LIMIT' ? rows.slice(0, 1) : rows.slice(1);
          for (const value of [{ordinary: true}, marker]) {
            expect(
              captureError(() => engine.executeSql(sql, [value])),
              sql,
            ).toMatchObject({code: 'INVALID_QUERY'});
            expect(
              captureError(() => engine.executePrepared(statement, [value])),
              sql,
            ).toMatchObject({code: 'INVALID_QUERY'});
            expect(engine.executeSql(sql, [1]).rows, sql).toEqual(expected);
            expect(engine.executePrepared(statement, [1]).rows, sql).toEqual(expected);
          }
          expect(engine.executePrepared(statement, [0]).rows, sql).toEqual(
            clause === 'LIMIT' ? [] : rows,
          );
        }
      }
      const jsonQuery = 'SELECT id, payload FROM items WHERE payload = $1 ORDER BY id';
      const jsonStatement = engine.prepareSql(jsonQuery);
      const expected = [{id: 1, payload: marker}, {id: 2, payload: marker}];
      expect(engine.executeSql(jsonQuery, [marker]).rows).toEqual(expected);
      expect(engine.executePrepared(jsonStatement, [marker]).rows).toEqual(expected);
      expect(engine.executePrepared(jsonStatement, [{ordinary: true}]).rows).toEqual([]);
      expect(engine.executePrepared(jsonStatement, [marker]).rows).toEqual(expected);
      expect(engine.revision()).toBe(revision);
    } finally {
      engine.close();
    }
  });

  it('executes selective three-table joins whose actual work fits the budget', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      const values = Array.from({length: 100}, (_, id) => `(${id})`).join(',');
      for (const table of ['a', 'b', 'c']) {
        engine.execSql(
          `CREATE TABLE ${table} (id INTEGER PRIMARY KEY); INSERT INTO ${table} VALUES ${values}`,
        );
      }
      const sql = 'SELECT a.id AS id FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id ORDER BY id';
      const expected = Array.from({length: 100}, (_, id) => ({id}));
      const statement = engine.prepareSql(sql);
      const revision = engine.revision();
      expect(engine.executeSql(sql, []).rows).toEqual(expected);
      expect(engine.executePrepared(statement, []).rows).toEqual(expected);
      engine.beginTransaction();
      expect(engine.executePrepared(statement, []).rows).toEqual(expected);
      engine.commitTransaction();
      expect(engine.revision()).toBe(revision);
    } finally {
      engine.close();
    }
  });

  it('rejects amplified parameters without poisoning the real WASM engine', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql('CREATE TABLE items (id INTEGER PRIMARY KEY, payload JSON)');
      const sql = `SELECT id FROM items WHERE payload IN (${Array(384).fill('$1').join(',')}) LIMIT 0`;
      const statement = engine.prepareSql(sql);
      const revision = engine.revision();
      const params = ['x'.repeat(64 * 1024)];
      expect(captureError(() => engine.executeSql(sql, params))).toMatchObject({
        code: 'RESOURCE_LIMIT',
      });
      expect(
        captureError(() => engine.executePrepared(statement, params)),
      ).toMatchObject({code: 'RESOURCE_LIMIT'});

      let shared: JsonValue = 1;
      for (let depth = 0; depth < 40; depth++) {
        shared = [shared, shared];
      }
      expect(
        captureError(() =>
          engine.executeSql('INSERT INTO items VALUES (1, $1)', [shared]),
        ),
      ).toMatchObject({code: 'RESOURCE_LIMIT'});
      expect(engine.revision()).toBe(revision);
      expect(engine.executeSql('SELECT id FROM items', []).rows).toEqual([]);
      engine.executeSql('INSERT INTO items VALUES (1, $1)', [{safe: true}]);
      expect(engine.executeSql('SELECT id FROM items', []).rows).toEqual([{id: 1}]);
      expect(engine.executePrepared(statement, ['small']).rows).toEqual([]);
    } finally {
      engine.close();
    }
  });

  it('keeps JSON comparisons and rejected mutations consistent through WASM', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql(
        'CREATE TABLE items (id INTEGER PRIMARY KEY, payload JSON, changed BOOLEAN NOT NULL DEFAULT false)',
      );
      const values: JsonValue[] = [{a: 1}, [1], 'one', 3, true, null];
      for (const [id, value] of values.entries()) {
        engine.executeSql('INSERT INTO items (id, payload) VALUES ($1, $2)', [
          id,
          value,
        ]);
      }
      const equality = engine.prepareSql(
        'SELECT id FROM items WHERE payload = $1 ORDER BY id',
      );
      for (const [id, value] of values.entries()) {
        const expected = value === null ? [] : [{id}];
        expect(engine.executePrepared(equality, [value]).rows).toEqual(expected);
      }
      expect(
        engine.executeSql('SELECT id FROM items WHERE payload <> $1 ORDER BY id', [
          {a: 1},
        ]).rows,
      ).toEqual([{id: 1}, {id: 2}, {id: 3}, {id: 4}]);

      const invalidDelete = engine.prepareSql(
        'DELETE FROM items WHERE payload > $1 RETURNING id',
      );
      const before = engine.revision();
      for (const inTransaction of [false, true]) {
        if (inTransaction) {
          engine.beginTransaction();
        }
        expect(
          captureError(() => engine.executePrepared(invalidDelete, [2])),
        ).toMatchObject({code: 'TYPE_MISMATCH'});
        expect(
          captureError(() =>
            engine.executeSql('UPDATE items SET changed = $1 WHERE id = 99', [
              'not a boolean',
            ]),
          ),
        ).toMatchObject({code: 'TYPE_MISMATCH'});
        if (inTransaction) {
          engine.commitTransaction();
        }
        expect(engine.revision()).toBe(before);
        expect(engine.executeSql('SELECT id FROM items', []).rows).toHaveLength(6);
      }
      expect(
        engine.executeSql(
          'UPDATE items SET changed = true WHERE payload = $1 RETURNING id',
          [{a: 1}],
        ).rows,
      ).toEqual([{id: 0}]);
    } finally {
      engine.close();
    }
  });

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

async function createDocumentedBackupFixture() {
  const wasm = await loadStructuredModule();
  const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
  const section = readFileSync('site/guides/2_storage_and_lifecycle.md', 'utf8')
    .split('## Application backups and restoration')[1]!
    .split('## Resetting and removing obsolete databases')[0]!;
  const source = /```ts\n([\s\S]*?)```/.exec(section)![1]!;
  const {code} = transformSync(source, {loader: 'ts', target: 'es2022'});
  const {backupNotes, restoreNotes} = compileFunction(
    code + '\nreturn {backupNotes, restoreNotes};',
  )();
  // Use the documented Client operations over the real WASM engine, keeping
  // the example itself verbatim so an edited SQL statement is exercised too.
  const db = {
    query: async (sql: string, params: JsonValue[] = []) => engine.executeSql(sql, params),
    exec: async (sql: string) => engine.execSql(sql),
    transaction: async (callback: (tx: unknown) => Promise<void>) => {
      engine.beginTransaction();
      try {
        await callback(db);
        engine.commitTransaction();
      } catch (error) {
        engine.rollbackTransaction();
        throw error;
      }
    },
  };
  return {engine, db, backupNotes, restoreNotes};
}

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
