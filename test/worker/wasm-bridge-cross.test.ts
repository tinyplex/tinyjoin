import {existsSync, readFileSync} from 'node:fs';
import {resolve} from 'node:path';
import {pathToFileURL} from 'node:url';
import {compileFunction} from 'node:vm';

import {describe, expect, it} from 'vitest';
import {transformSync} from 'esbuild';

import {
  PROTOCOL_VERSION,
  STATEMENT_COMMANDS,
  STATEMENT_SELECT,
  isRpcResult,
  isSqlDataText,
  isStatementResponse,
  type ChangedKeys,
  type JsonPrimitive,
  type JsonValue,
  type Row,
  type RowMode,
  type SqlData,
  type SqlResult,
  type StatementResult,
} from '../../src/protocol.js';
import type {WorkerEngine} from '../../src/worker/engine.js';
import type {PageDevice} from '../../src/worker/page-device.js';
import {warmUp} from '../../src/worker/warm-up.js';
import {
  WASM_OPERATION,
  createStructuredWasmEngine,
  type RawStructuredWasmEngine,
  type RawStructuredWasmEngineConstructor,
} from '../../src/worker/wasm-bridge.js';
import {encodeExecSql, encodeStatement} from '../../src/worker/wasm-preflight.js';
import {decodeRequest} from '../helpers/wasm-request.js';
import {
  decodeStatementHeader,
  statementResult,
} from '../helpers/wasm-response.js';

const BRIDGE_VERSION = 6;
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
  /** The header the call left in the engine's memory, if it left one. */
  header: Uint8Array | undefined;
};

class RecordingStructuredEngine implements RawStructuredWasmEngine {
  readonly calls: StructuredCall[] = [];
  freeCalls = 0;
  readonly wasmMemory: WebAssembly.Memory;
  readonly headerAt: number;

  constructor(readonly raw: RawStructuredWasmEngine) {
    this.wasmMemory = raw.memory!() as WebAssembly.Memory;
    this.headerAt = raw.resultHeader!();
  }

  memory(): WebAssembly.Memory {
    return this.wasmMemory;
  }

  resultHeader(): number {
    return this.headerAt;
  }

  callStructured(
    bridgeVersion: number,
    operation: number,
    request: Uint8Array,
  ): unknown {
    // The request's bytes are reused for the next, so they are read at once.
    const payload = decodeRequest(operation, request);
    const response = this.raw.callStructured(
      bridgeVersion,
      operation,
      request,
    );
    // So is the header, which the next call clears: its first byte says
    // whether there is one, and its fifth and sixth how long it is.
    const bytes = new Uint8Array(this.wasmMemory.buffer);
    const at = this.headerAt;
    const header = bytes[at]
      ? bytes.slice(at, at + (bytes[at + 4]! | (bytes[at + 5]! << 8)))
      : undefined;
    this.calls.push({bridgeVersion, operation, payload, response, header});
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

  writePages(low: number, high: number, source: Uint8Array): number {
    if (high !== 0 || low > this.pages.length) {
      throw new Error('invalid test page write');
    }
    for (let offset = 0; offset < source.byteLength; offset += 4096) {
      this.pages[low + offset / 4096] = source.slice(offset, offset + 4096);
    }
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
  it('runs every warm-up statement against the real engine', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    // What each prepared statement was answered with, by its SQL.
    const statements = new Map<number, string>();
    const results = new Map<string, DecodedResult[]>();
    const recorded = {
      ...engine,
      prepareSql: (sql: string) => {
        const id = engine.prepareSql(sql);
        statements.set(id, sql);
        results.set(sql, []);
        return id;
      },
      executePrepared: (id: number, params: readonly JsonValue[], rowMode?: RowMode) => {
        const result = engine.executePrepared(id, params, rowMode);
        results.get(statements.get(id)!)!.push(result);
        return result;
      },
    };
    try {
      // Each result gets the full check, so a statement the engine rejects fails here.
      warmUp(recorded as unknown as WorkerEngine);
      expect(engine.inTransaction()).toBe(false);
      expect(engine.executeSql('SELECT count(*) AS n FROM w', []).rows).toEqual([{n: 404}]);
    } finally {
      engine.close();
    }

    // The statements on `v` are there to take the path of a write by key, on
    // a table that lets it be taken, so they must find rows to write: an
    // update that matched nothing, or an upsert that only ever inserted,
    // would leave the rewriting of a row for a database's first statements
    // to compile. Nearly every update finds its row, and one in each pass
    // finds none.
    const updates = results.get('UPDATE v SET n = $1 WHERE id = $2')!;
    expect(updates.map((result) => result.command)).toEqual(Array(25).fill('UPDATE'));
    expect(updates.filter((result) => result.rowCount === 1)).toHaveLength(23);
    expect(updates.filter((result) => result.rowCount === 0)).toHaveLength(2);
    for (const update of updates) {
      expect(update.tables).toEqual(update.rowCount ? ['v'] : []);
      expect(update.keys.v ?? []).toHaveLength(update.rowCount);
    }
    // Every row of `v` was reported by the statement that inserted it, so an
    // upsert that reports a key already reported rewrote that row, and one
    // that reports a new key inserted it. The warm-up does both.
    const known = new Set(
      results
        .get('INSERT INTO v (id, wid, n) VALUES ($1, $2, $3)')!
        .flatMap((result) => result.keys.v!.map((key) => key.id)),
    );
    expect(known.size).toBe(305);
    let rewritten = 0;
    let inserted = 0;
    for (const upsert of results.get(
      'INSERT INTO v (id, wid, n) VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n',
    )!) {
      expect(upsert).toMatchObject({command: 'INSERT', rowCount: 1, tables: ['v']});
      const [key] = upsert.keys.v!;
      if (known.has(key!.id)) {
        rewritten += 1;
      } else {
        inserted += 1;
        known.add(key!.id);
      }
    }
    expect([rewritten, inserted]).toEqual([11, 14]);
  });

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
      expect(engine.executeSql(examples[2]!, []).rows).toEqual([{id: 1}]);
      expect(engine.executeSql(examples[3]!, []).rows).toEqual([{id: 1, 'extra.value': 'kept'}]);
      expect(captureError(() => engine.executeSql(examples[4]!, []))).toMatchObject({
        code: 'INVALID_QUERY',
      });
      expect(engine.executeSql(examples[4]!, [], 'array').rows).toEqual([[1, 1]]);
      expect(captureError(() => engine.executeSql(examples[5]!, []))).toMatchObject({
        code: 'UNSUPPORTED_SQL',
      });
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

  it('returns duplicate projections to array rows in field order', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql(
        `CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL);
         INSERT INTO users VALUES (1, 'ann');
         INSERT INTO posts VALUES (10, 1), (11, 1);`,
      );
      const join =
        'SELECT users.id, posts.id, users.name FROM users ' +
        'JOIN posts ON users.id = posts.user_id ORDER BY posts.id DESC';
      const names = ['id', 'id', 'name'];
      const rows = [
        [1, 11, 'ann'],
        [1, 10, 'ann'],
      ];

      const joined = engine.executeSql(join, [], 'array');
      expect(joined.fields.map(field => field.name)).toEqual(names);
      expect(joined.rows).toEqual(rows);
      const prepared = engine.prepareSql(join);
      expect(engine.executePrepared(prepared, [], 'array').rows).toEqual(rows);
      expect(
        engine.execSql(`SELECT name, name FROM users; ${join}`, 'array'),
      ).toMatchObject([{rows: [['ann', 'ann']]}, {rows}]);
      expect(
        engine.executeSql('SELECT count(*), count(name) FROM users', [], 'array'),
      ).toMatchObject({
        fields: [{name: 'count'}, {name: 'count'}],
        rows: [[1, 1]],
      });

      // Object rows cannot hold two values under one name.
      expect(captureError(() => engine.executeSql(join, []))).toMatchObject({
        code: 'INVALID_QUERY',
      });
      expect(
        captureError(() => engine.executePrepared(prepared, [], 'object')),
      ).toMatchObject({code: 'INVALID_QUERY'});
    } finally {
      engine.close();
    }
  });

  it('reads the schema the real engine holds', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      expect(engine.schema()).toEqual({version: 0, tables: []});
      engine.execSql(
        `CREATE TABLE notes (owner TEXT, id INTEGER, body TEXT DEFAULT 'x',
           meta JSONB DEFAULT NULL, rating REAL NOT NULL, PRIMARY KEY (owner, id));
         CREATE TABLE "Tags" (name VARCHAR(20) PRIMARY KEY, hot BOOLEAN DEFAULT false);
         CREATE UNIQUE INDEX notes_body ON notes (body, owner);
         CREATE INDEX a_notes_id ON notes (id);
         ALTER TABLE notes ADD COLUMN extra BIGINT;`,
      );
      const schema = engine.schema();
      expect(isRpcResult('schema', schema)).toBe(true);
      expect(schema).toEqual({
        version: 0,
        tables: [
          {
            name: 'Tags',
            columns: [
              {name: 'name', type: 'text', nullable: false, maxLength: 20},
              {name: 'hot', type: 'boolean', nullable: true, default: false},
            ],
            primaryKey: ['name'],
            indexes: [],
            foreignKeys: [],
          },
          {
            name: 'notes',
            columns: [
              {name: 'owner', type: 'text', nullable: false},
              {name: 'id', type: 'integer', nullable: false},
              {name: 'body', type: 'text', nullable: true, default: 'x'},
              {name: 'meta', type: 'json', nullable: true, default: null},
              {name: 'rating', type: 'float', nullable: false},
              {name: 'extra', type: 'integer', nullable: true},
            ],
            primaryKey: ['owner', 'id'],
            indexes: [
              {name: 'a_notes_id', columns: ['id'], unique: false},
              {name: 'notes_body', columns: ['body', 'owner'], unique: true},
            ],
            foreignKeys: [],
          },
        ],
      });

      engine.beginTransaction();
      expect(engine.schema()).toEqual(schema);
      engine.rollbackTransaction();
    } finally {
      engine.close();
    }
  });

  it('sets the schema the real engine holds, and refuses an older one', async () => {
    const wasm = await loadStructuredModule();
    const {engine} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      const schema = {
        version: 2,
        tables: [
          {
            name: 'tasks',
            columns: [
              {name: 'id', type: 'text' as const, nullable: false},
              {name: 'title', type: 'text' as const, nullable: false, maxLength: 80},
              {name: 'done', type: 'boolean' as const, nullable: false, default: false},
            ],
            primaryKey: ['id'],
            indexes: [{name: 'tasks_done', columns: ['done'], unique: false}],
            foreignKeys: [],
          },
        ],
      };
      expect(engine.setSchema(schema, false)).toEqual({
        revision: 1,
        tables: ['tasks'],
        keys: {},
      });
      expect(engine.schema()).toEqual(schema);
      expect(engine.setSchema(schema, false)).toEqual({
        revision: 1,
        tables: [],
        keys: {},
      });
      expect(
        captureError(() => engine.setSchema({...schema, version: 1}, false)),
      ).toMatchObject({code: 'SCHEMA_OUTDATED'});
      expect(
        captureError(() =>
          engine.setSchema({version: 3, tables: [{name: 'x'}]} as never, false),
        ),
      ).toMatchObject({code: 'INVALID_SCHEMA'});
      expect(engine.revision()).toBe(1);
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

  it('answers a statement that published nothing as its response, which says what its JSON would', async () => {
    const wasm = await loadStructuredModule();
    const schema = `
      CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, c TEXT NOT NULL);
      CREATE TABLE pairs (owner TEXT, n FLOAT, flag BOOLEAN, body TEXT, PRIMARY KEY (owner, n, flag));
      CREATE TABLE "__proto__" ("__proto__" TEXT PRIMARY KEY, v INTEGER);
      CREATE TABLE parents (id INTEGER PRIMARY KEY);
      CREATE TABLE children (id INTEGER PRIMARY KEY, parent INTEGER REFERENCES parents (id) ON DELETE CASCADE);
      CREATE INDEX children_parent ON children (parent);
      CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT);
      CREATE TABLE many (id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
      CREATE TABLE named (id TEXT PRIMARY KEY, v INTEGER NOT NULL);
      INSERT INTO t VALUES (1, 10, 'one'), (2, 20, 'two'), (3, 30, 'three');
      INSERT INTO parents VALUES (1), (2);
      INSERT INTO children VALUES (10, 1), (11, 1), (12, 2);
    `;
    const name = (id: number) => `a rather long name of a key ${String(id).padStart(6, '0')}`;
    // The same database three times over: for statements as text, for the same
    // statements prepared, and for the same statements as scripts, whose
    // results are always JSON.
    const open = () => {
      const opened = createRecordingEngine(wasm, new MemoryPageDevice());
      opened.engine.execSql(schema);
      opened.engine.beginTransaction();
      const many = opened.engine.prepareSql('INSERT INTO many VALUES ($1, 0)');
      const named = opened.engine.prepareSql('INSERT INTO named VALUES ($1, 0)');
      for (let id = 1; id <= 1200; id++) {
        opened.bridged.executePrepared(many, [id]);
        if (id <= 1000) opened.bridged.executePrepared(named, [name(id)]);
      }
      opened.engine.commitTransaction();
      return opened;
    };
    const direct = open();
    const prepared = open();
    const scripted = open();
    const long = 'k'.repeat(100);
    const revision = direct.engine.revision();
    // Each statement, and the response it is answered with, or `undefined`
    // for one whose result has not the shape of a response.
    const flat = (command: number, rowCount: number, ...rest: JsonPrimitive[]) => [
      0,
      0,
      command,
      revision,
      rowCount,
      ...rest,
    ];
    const statements = (
      rowMode: RowMode,
    ): [sql: string, response: JsonPrimitive[] | undefined][] => [
      [`INSERT INTO t VALUES (4, 40, 'four')`, flat(0, 1, 't', 1, 'id', 4)],
      [`INSERT INTO t VALUES (5, 50, 'five'), (6, 60, 'six')`, flat(0, 2, 't', 1, 'id', 5, 6)],
      [`UPDATE t SET c = 'x' WHERE id = 1`, flat(1, 1, 't', 1, 'id', 1)],
      [`UPDATE t SET c = 'x' WHERE id = 99`, flat(1, 0)],
      [`DELETE FROM t WHERE id = 2`, flat(2, 1, 't', 1, 'id', 2)],
      [`DELETE FROM t WHERE id = 98`, flat(2, 0)],
      [
        `INSERT INTO t VALUES (3, 31, 'again') ON CONFLICT (id) DO UPDATE SET c = EXCLUDED.c`,
        flat(0, 1, 't', 1, 'id', 3),
      ],
      [
        `INSERT INTO t VALUES (7, 70, 'seven') ON CONFLICT (id) DO UPDATE SET c = EXCLUDED.c`,
        flat(0, 1, 't', 1, 'id', 7),
      ],
      [`INSERT INTO t VALUES (3, 1, 'dup') ON CONFLICT (id) DO NOTHING`, flat(0, 0)],
      [`UPDATE t SET a = a + 1`, flat(1, 6, 't', 1, 'id', 1, 3, 4, 5, 6, 7)],
      // A key of several columns and kinds, in key order.
      [
        `INSERT INTO pairs VALUES ('é', 1.5, true, 'b'), ('plain', -2.25, false, 'c'), ('z', 2.0, true, 'd')`,
        flat(0, 3, 'pairs', 3, 'owner', 'n', 'flag', 'plain', -2.25, false, 'z', 2, true, 'é', 1.5, true),
      ],
      [`UPDATE pairs SET body = 'z' WHERE owner = 'é'`, flat(1, 1, 'pairs', 3, 'owner', 'n', 'flag', 'é', 1.5, true)],
      [`DELETE FROM pairs WHERE n < 2`, flat(2, 2, 'pairs', 3, 'owner', 'n', 'flag', 'plain', -2.25, false, 'é', 1.5, true)],
      // Names that are properties of every object.
      [
        `INSERT INTO "__proto__" VALUES ('__proto__', 1), ('constructor', 2)`,
        flat(0, 2, '__proto__', 1, '__proto__', '__proto__', 'constructor'),
      ],
      // Text keys that are empty, long, not ASCII, and marked with a byte order.
      [
        `INSERT INTO notes VALUES ('\ufeffbom', 'x'), ('${long}', 'long'), ('é😀', 'emoji'), ('', 'empty'), ('abc', 'short')`,
        flat(0, 5, 'notes', 1, 'id', '', 'abc', long, 'é😀', '\ufeffbom'),
      ],
      [`UPDATE notes SET body = 'again' WHERE id >= 'é'`, flat(1, 2, 'notes', 1, 'id', 'é😀', '\ufeffbom')],
      // As many keys as a table reports, and one more, which reports none.
      [
        `UPDATE many SET v = v + 1 WHERE id <= 1000`,
        flat(1, 1000, 'many', 1, 'id', ...Array.from({length: 1000}, (_, id) => id + 1)),
      ],
      [`UPDATE many SET v = v + 1 WHERE id <= 1001`, flat(1, 1001, 'many')],
      [`DELETE FROM many`, flat(2, 1200, 'many')],
      // Keys that fit a header, and as many as a table reports that do not.
      [
        `UPDATE named SET v = v + 1 WHERE id < '${name(430)}'`,
        flat(1, 429, 'named', 1, 'id', ...Array.from({length: 429}, (_, id) => name(id + 1))),
      ],
      [`UPDATE named SET v = v + 1`, undefined],
      // Rows returned, with or without any to return.
      [`UPDATE t SET a = a + 1 WHERE id = 1 RETURNING id, a`, undefined],
      [`DELETE FROM t WHERE id = 97 RETURNING id`, undefined],
      // Two tables changed, as a cascade changes them.
      [`DELETE FROM parents WHERE id = 1`, undefined],
      [`DELETE FROM parents WHERE id = 2`, undefined],
      [`DELETE FROM parents WHERE id = 3`, flat(2, 0)],
      // Reads.
      [`SELECT * FROM t ORDER BY id`, flat(3, 6, expect.any(String) as unknown as string)],
      [`SELECT id FROM t WHERE id = 99`, flat(3, 0, '{"fields":[{"name":"id","dataTypeID":20}],"rows":[]}')],
      [`SELECT count(*) AS n, max(c) AS c FROM t`, flat(3, 1, expect.any(String) as unknown as string)],
      [
        `SELECT "__proto__" FROM "__proto__" ORDER BY "__proto__"`,
        flat(
          3,
          2,
          `{"fields":[{"name":"__proto__","dataTypeID":25}],"rows":${
            rowMode === 'array'
              ? '[["__proto__"],["constructor"]]'
              : '[{"__proto__":"__proto__"},{"__proto__":"constructor"}]'
          }}`,
        ),
      ],
    ];
    // Statements are prepared before a transaction begins, as a client prepares them.
    const ids = statements('object').map(([sql]) => prepared.engine.prepareSql(sql));
    // Each kind of row in a transaction of its own, rolled back for the next.
    for (const rowMode of ['object', 'array'] as const) {
      for (const {engine} of [direct, prepared, scripted]) {
        engine.beginTransaction();
      }
      for (const [at, [sql, response]] of statements(rowMode).entries()) {
        const sent = direct.bridged.executeSql(sql, [], rowMode);
        const result = direct.checked(sent);
        expect(Array.isArray(sent), sql).toBe(response !== undefined);
        if (response) {
          expect(sent, sql).toEqual(response);
        }
        // Prepared, the statement is answered alike.
        expect(prepared.bridged.executePrepared(ids[at]!, [], rowMode), sql).toEqual(sent);
        // And what either says is what the JSON of the same statement says,
        // key for key and in the same order.
        const scripts = scripted.engine.execSql(sql, rowMode);
        expect(scripts, sql).toHaveLength(1);
        expect(sameResult(result, scripts[0]!), sql).toBe(true);
      }
      // Nothing was published by any of it.
      for (const {engine} of [direct, prepared, scripted]) {
        expect(engine.revision()).toBe(revision);
        engine.rollbackTransaction();
      }
    }
    for (const {engine} of [direct, prepared, scripted]) {
      engine.close();
    }
  });

  it('answers in JSON what published, and outside a transaction what changed nothing as its response', async () => {
    const wasm = await loadStructuredModule();
    const {engine, bridged, recording} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql(`CREATE TABLE t (id INTEGER PRIMARY KEY, c TEXT NOT NULL); INSERT INTO t VALUES (1, 'one')`);
      const revision = engine.revision();
      const update = engine.prepareSql('UPDATE t SET c = $1 WHERE id = $2');
      const insert = engine.prepareSql('INSERT INTO t VALUES ($1, $2) ON CONFLICT (id) DO NOTHING');
      const remove = engine.prepareSql('DELETE FROM t WHERE id = $1');
      const select = engine.prepareSql('SELECT c FROM t WHERE id = $1');
      const duplicate = engine.prepareSql('INSERT INTO t VALUES ($1, $2)');

      // A read, and a write that changed nothing, published nothing.
      expect(bridged.executePrepared(select, [1])).toEqual([
        0, 0, 3, revision, 1, '{"fields":[{"name":"c","dataTypeID":25}],"rows":[{"c":"one"}]}',
      ]);
      expect(bridged.executePrepared(select, [1], 'array')).toEqual([
        0, 0, 3, revision, 1, '{"fields":[{"name":"c","dataTypeID":25}],"rows":[["one"]]}',
      ]);
      expect(bridged.executePrepared(update, ['x', 99])).toEqual([0, 0, 1, revision, 0]);
      expect(bridged.executePrepared(insert, [1, 'dup'])).toEqual([0, 0, 0, revision, 0]);
      expect(bridged.executePrepared(remove, [99])).toEqual([0, 0, 2, revision, 0]);
      expect(bridged.executeSql('DELETE FROM t WHERE id = $1', [99])).toEqual([0, 0, 2, revision, 0]);
      expect(engine.revision()).toBe(revision);

      // A write that changed a row published it, and is read to be announced.
      expect(bridged.executePrepared(update, ['x', 1])).toEqual({
        command: 'UPDATE',
        revision: revision + 1,
        rowCount: 1,
        tables: ['t'],
        keys: {t: [{id: 1}]},
        data: '{"fields":[],"rows":[]}',
      });
      expectDisposition(recording, WASM_OPERATION.executePrepared, 'durable');
      expect(lastCall(recording, WASM_OPERATION.executePrepared).header).toBeUndefined();
      expect(bridged.executeSql('INSERT INTO t VALUES ($1, $2)', [2, 'two'])).toMatchObject({
        command: 'INSERT',
        revision: revision + 2,
        keys: {t: [{id: 2}]},
      });
      // Any other command is answered in JSON, though it changed nothing.
      expect(bridged.executeSql('CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)', [])).toEqual({
        command: 'CREATE TABLE',
        revision: revision + 2,
        rowCount: 0,
        tables: [],
        keys: {},
        data: '{"fields":[],"rows":[]}',
      });
      expectDisposition(recording, WASM_OPERATION.executeSql, 'safe');

      // Inside a transaction every statement is a response, the same writes among them.
      engine.beginTransaction();
      expect(bridged.executePrepared(update, ['y', 1])).toEqual([0, 0, 1, revision + 2, 1, 't', 1, 'id', 1]);
      expect(bridged.executePrepared(insert, [3, 'three'])).toEqual([0, 0, 0, revision + 2, 1, 't', 1, 'id', 3]);
      expect(bridged.executePrepared(remove, [2])).toEqual([0, 0, 2, revision + 2, 1, 't', 1, 'id', 2]);
      expect(bridged.executePrepared(select, [1])).toEqual([
        0, 0, 3, revision + 2, 1, '{"fields":[{"name":"c","dataTypeID":25}],"rows":[{"c":"y"}]}',
      ]);
      // A statement request's parameters are read where they stand in it.
      const request: JsonValue[] = [PROTOCOL_VERSION, 9, 4, update, 'tx-1', 0, 'z', 3];
      expect(bridged.executePrepared(update, request, undefined, 6)).toEqual([0, 0, 1, revision + 2, 1, 't', 1, 'id', 3]);
      expectRequest(recording, WASM_OPERATION.executePrepared).toEqual({statementId: update, params: ['z', 3]});
      expect(bridged.executeSql('SELECT c FROM t WHERE id = $1', request.slice(0, 7).concat(3), 'array', 7)).toEqual([
        0, 0, 3, revision + 2, 1, '{"fields":[{"name":"c","dataTypeID":25}],"rows":[["z"]]}',
      ]);

      // A header never outlives its call: a failure after one is the failure,
      // a result in JSON after one is that result, and no other operation
      // leaves or reads one.
      expect(
        captureError(() => bridged.executePrepared(duplicate, [1, 'dup'])),
      ).toMatchObject({code: 'CONSTRAINT_VIOLATION'});
      expectFailureDisposition(recording, WASM_OPERATION.executePrepared);
      expect(bridged.executePrepared(select, [1])).toHaveLength(6);
      expect(bridged.executeSql('UPDATE t SET c = c WHERE id = 1 RETURNING c', [])).toMatchObject({
        command: 'UPDATE',
        data: '{"fields":[{"name":"c","dataTypeID":25}],"rows":[{"c":"y"}]}',
      });
      expect(engine.inTransaction()).toBe(true);
      expect(engine.revision()).toBe(revision + 2);
      expect(engine.commitTransaction()).toEqual({
        revision: revision + 3,
        tables: ['t'],
        keys: {t: [{id: 1}, {id: 2}, {id: 3}]},
      });
      for (const call of recording.calls) {
        const statement =
          call.operation === WASM_OPERATION.executeSql ||
          call.operation === WASM_OPERATION.executePrepared;
        expect(call.header === undefined || statement).toBe(true);
        // `true`, and text that is not an envelope, only ever stand beside a header.
        if (call.header === undefined) {
          expect(call.response).toEqual(expect.stringMatching(/^\[6,[01],[01],/));
        }
      }
    } finally {
      engine.close();
    }
  });

  it('refuses a request by its version, then by whether the engine is open, and only then by what it holds', async () => {
    const wasm = await loadStructuredModule();
    const raw = new wasm.WasmEngine(new MemoryPageDevice());
    const refusal = (bridgeVersion: number, operation: number, request: number[]) => {
      const response = raw.callStructured(bridgeVersion, operation, Uint8Array.from(request));
      const [version, status, disposition, payload] = JSON.parse(response as string) as [
        number,
        number,
        number,
        {code: string},
      ];
      expect([version, status, disposition]).toEqual([BRIDGE_VERSION, 1, 0]);
      return payload.code;
    };
    // A statement that is not one, a request with bytes after it, and an
    // operation there is none of.
    const malformed: [operation: number, request: number[]][] = [
      [WASM_OPERATION.executeSql, [2]],
      [WASM_OPERATION.executePrepared, [0, 1, 0, 0, 0]],
      [WASM_OPERATION.begin, [0]],
      [WASM_OPERATION.revision, [1, 2, 3]],
      [99, []],
    ];
    try {
      for (const [operation, request] of malformed) {
        expect(refusal(BRIDGE_VERSION, operation, request)).toBe('INVALID_BRIDGE_VALUE');
      }
      // An engine of another version refuses whatever it is asked, closing included.
      expect(refusal(BRIDGE_VERSION - 1, WASM_OPERATION.revision, [])).toBe('INVALID_BRIDGE_VALUE');
      expect(refusal(BRIDGE_VERSION + 1, WASM_OPERATION.close, [])).toBe('INVALID_BRIDGE_VALUE');
      expect(JSON.parse(raw.callStructured(BRIDGE_VERSION, WASM_OPERATION.revision, new Uint8Array(0)) as string)).toEqual([
        BRIDGE_VERSION, 0, 0, 0,
      ]);

      // Closed, it says so to every request, whatever the request holds,
      expect(raw.callStructured(BRIDGE_VERSION, WASM_OPERATION.close, new Uint8Array(0))).toBe(
        `[${BRIDGE_VERSION},0,0,null]`,
      );
      for (const [operation, request] of [
        ...malformed,
        [WASM_OPERATION.revision, []],
        [WASM_OPERATION.executePrepared, [0, 1, 0, 0, 0, 0, 0, 0, 0]],
      ] as [number, number[]][]) {
        expect(refusal(BRIDGE_VERSION, operation, request)).toBe('ENGINE_CLOSED');
      }
      // but still tells a request of another version that it is one,
      expect(refusal(BRIDGE_VERSION + 1, WASM_OPERATION.revision, [])).toBe('INVALID_BRIDGE_VALUE');
      // and may be closed again.
      expect(raw.callStructured(BRIDGE_VERSION, WASM_OPERATION.close, new Uint8Array(0))).toBe(
        `[${BRIDGE_VERSION},0,0,null]`,
      );
    } finally {
      raw.free?.();
    }
  });

  it('runs nothing after a fatal storage failure, and says why until it is closed', async () => {
    const wasm = await loadStructuredModule();
    class FailingDevice extends MemoryPageDevice {
      failing = false;

      override flush(): void {
        if (this.failing) {
          throw new Error('device flush failed');
        }
      }
    }
    const device = new FailingDevice();
    const raw = new wasm.WasmEngine(device);
    const respond = (operation: number, request: Uint8Array = new Uint8Array(0)) =>
      JSON.parse((raw.callStructured(BRIDGE_VERSION, operation, request) as string).split('\n')[0]!) as [
        number,
        number,
        number,
        {code?: string; retryable?: boolean} | null,
      ];
    try {
      expect(
        respond(WASM_OPERATION.execSql, encodeExecSql('CREATE TABLE a (id INTEGER PRIMARY KEY)', false)).slice(0, 3),
      ).toEqual([BRIDGE_VERSION, 0, 1]);
      // A write whose pages reached the device, but not its flush, may or
      // may not have been published: the engine says so, and closes itself.
      device.failing = true;
      const insert = encodeStatement(false, 'INSERT INTO a VALUES ($1)', [1], false).slice();
      expect(respond(WASM_OPERATION.executeSql, insert)).toEqual([
        BRIDGE_VERSION,
        1,
        0,
        expect.objectContaining({code: 'RECOVERY_REQUIRED'}),
      ]);
      device.failing = false;
      expect(device.closed).toBe(1);
      // Every request after it is refused for that, whatever it holds.
      for (const [operation, request] of [
        [WASM_OPERATION.revision, []],
        [WASM_OPERATION.executeSql, [...insert]],
        [WASM_OPERATION.executeSql, [2]],
        [99, []],
      ] as [number, number[]][]) {
        expect(respond(operation, Uint8Array.from(request))).toEqual([
          BRIDGE_VERSION,
          1,
          0,
          {
            code: 'STORAGE_ENGINE_POISONED',
            message: 'The TinyJoin engine cannot be used after an uncertain result',
            retryable: false,
          },
        ]);
      }
      // Closed, it is an engine that is closed, as any other is.
      expect(respond(WASM_OPERATION.close)).toEqual([BRIDGE_VERSION, 0, 0, null]);
      expect(respond(WASM_OPERATION.revision)).toEqual([
        BRIDGE_VERSION,
        1,
        0,
        {code: 'ENGINE_CLOSED', message: 'The TinyJoin engine is closed'},
      ]);
      expect(device.closed).toBe(1);
    } finally {
      raw.free?.();
    }
  });

  it('answers every statement in JSON to whatever does not ask where its header is', async () => {
    const wasm = await loadStructuredModule();
    // The engine wrapped by something that passes on its calls and nothing
    // else, as a tool that times or traces them may be written. The bridge
    // finds no memory to read a header in, so it never asks where one would
    // be, and the engine, not having been asked, writes none.
    const responses: unknown[] = [];
    class CallsOnly implements RawStructuredWasmEngine {
      readonly raw: RawStructuredWasmEngine;

      constructor(device: PageDevice) {
        this.raw = new wasm.WasmEngine(device);
      }

      callStructured(bridgeVersion: number, operation: number, request: Uint8Array): unknown {
        const response = this.raw.callStructured(bridgeVersion, operation, request);
        responses.push(response);
        return response;
      }

      free(): void {
        this.raw.free?.();
      }
    }
    const wrapped = createStructuredWasmEngine(CallsOnly, new MemoryPageDevice());
    // The same statements on an engine that is read as the Worker reads it.
    const {engine: read, bridged} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      const schema = `CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT NOT NULL);
        INSERT INTO notes VALUES ('one', 'first')`;
      wrapped.execSql(schema);
      read.execSql(schema);
      // Each is run four times over, so each can be: as text and prepared,
      // for rows of either kind.
      const statements: [sql: string, params: JsonValue[]][] = [
        ['SELECT * FROM notes ORDER BY id', []],
        ['UPDATE notes SET body = $1 WHERE id = $2', ['x', 'none']],
        ['DELETE FROM notes WHERE id = $1', ['none']],
        ['INSERT INTO notes VALUES ($1, $2) ON CONFLICT (id) DO NOTHING', ['two', 'second']],
        ['UPDATE notes SET body = $1 WHERE id = $2', ['changed', 'one']],
        ['INSERT INTO notes VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body', ['one', 'again']],
        ['SELECT body FROM notes WHERE id = $1', ['one']],
        ['DELETE FROM notes WHERE id = $1', ['two']],
        ['UPDATE notes SET body = body', []],
        // Rows returned, which no header holds for either engine.
        ['UPDATE notes SET body = body WHERE id = $1 RETURNING id, body', ['one']],
      ];
      const ids = statements.map(([sql]) => [wrapped.prepareSql(sql), read.prepareSql(sql)] as const);
      // Once outside a transaction, where a write that changes a row
      // publishes it, and once inside one, where nothing is published and
      // every one of these results has the shape of a statement's response.
      for (const inTransaction of [false, true]) {
        if (inTransaction) {
          wrapped.beginTransaction();
          read.beginTransaction();
        }
        let headed = 0;
        for (const [at, [sql, params]] of statements.entries()) {
          for (const rowMode of ['object', 'array'] as const) {
            const [wrappedId, readId] = ids[at]!;
            for (const [inJson, inHeader] of [
              [wrapped.executeSql(sql, params, rowMode), bridged.executeSql(sql, params, rowMode)],
              [wrapped.executePrepared(wrappedId, params, rowMode), bridged.executePrepared(readId, params, rowMode)],
            ] as const) {
              // Never the array that is read from a header,
              expect(Array.isArray(inJson), sql).toBe(false);
              headed += Array.isArray(inHeader) ? 1 : 0;
              // and always what the header, where there is one, says.
              expect(sameResult(decode(inJson), decode(inHeader)), sql).toBe(true);
            }
          }
        }
        // The engine that was asked answers with a header all that
        // published nothing but the one that returned rows: in a
        // transaction every other statement, and outside one the reads and
        // the writes that changed nothing.
        if (inTransaction) {
          expect(headed).toBe((statements.length - 1) * 4);
        } else {
          expect(headed).toBeGreaterThanOrEqual(16);
          expect(headed).toBeLessThan((statements.length - 1) * 4);
        }
        if (inTransaction) {
          expect(wrapped.commitTransaction()).toEqual(read.commitTransaction());
        }
      }
      // Every response was the text of an envelope: never `true`, and never a
      // read's rows alone.
      expect(responses.length).toBeGreaterThan(80);
      for (const response of responses) {
        expect(response).toEqual(expect.stringMatching(/^\[6,[01],[01],/));
      }
    } finally {
      wrapped.close();
      read.close();
    }
  });

  it('reads headers from the memory as it grows', async () => {
    const wasm = await loadStructuredModule(true);
    const {engine, bridged, recording} = createRecordingEngine(wasm, new MemoryPageDevice());
    try {
      engine.execSql('CREATE TABLE t (id INTEGER PRIMARY KEY, c TEXT NOT NULL)');
      const insert = engine.prepareSql('INSERT INTO t VALUES ($1, $2)');
      const select = engine.prepareSql('SELECT id FROM t WHERE id = $1');
      const revision = engine.revision();
      const inserted = (id: number) => [0, 0, 0, revision, 1, 't', 1, 'id', id];
      const selected = (id: number) => [
        0, 0, 3, revision, 1, `{"fields":[{"name":"id","dataTypeID":20}],"rows":[{"id":${id}}]}`,
      ];
      engine.beginTransaction();
      // Growing the memory leaves the buffer the bridge was reading detached,
      // whoever grows it: here the test, between two calls.
      for (let id = 1; id <= 3; id++) {
        expect(bridged.executePrepared(insert, [id, 'grown'])).toEqual(inserted(id));
        const buffer = recording.wasmMemory.buffer;
        recording.wasmMemory.grow(1);
        expect(buffer.byteLength).toBe(0);
        expect(bridged.executePrepared(select, [id])).toEqual(selected(id));
      }
      // And here the engine, inside the call that writes the header, once the
      // rows a transaction stages have taken what memory there was.
      const text = 'x'.repeat(1000);
      let buffer = recording.wasmMemory.buffer;
      let grown = 0;
      for (let id = 4; grown < 2 && id < 15_000; id++) {
        expect(bridged.executePrepared(insert, [id, text])).toEqual(inserted(id));
        if (recording.wasmMemory.buffer !== buffer) {
          expect(buffer.byteLength).toBe(0);
          buffer = recording.wasmMemory.buffer;
          grown += 1;
          expect(bridged.executePrepared(select, [id])).toEqual(selected(id));
        }
      }
      expect(grown).toBe(2);
      engine.rollbackTransaction();
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
    expectRequest(recording, WASM_OPERATION.execSql).toEqual({sql: schemaSql});
    expect(created.map(withoutRows)).toEqual(
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
    expect((insertCall.payload as {params: JsonValue[]}).params).toEqual(
      insertParams,
    );
    expect(withoutRows(inserted)).toEqual(responsePayload(insertCall));
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
    // Keys come in field order, and a `__proto__` key stays an own property
    // without changing any prototype.
    expect(Object.keys(selectedRow)).toEqual(['id', 'title', 'payload']);
    const selectedPayload = selectedRow.payload as Record<string, JsonValue>;
    expect(Object.getPrototypeOf(selectedPayload)).toBe(Object.prototype);
    expect(Object.hasOwn(selectedPayload, '__proto__')).toBe(true);
    expect(selectedPayload.__proto__).toBe('data');
    expect(Object.is(selectedPayload.signedZero, -0)).toBe(false);
    const nested = selectedPayload.nested as JsonValue[];
    const nestedRecord = nested[3] as Record<string, JsonValue>;
    expect(Object.getPrototypeOf(nestedRecord)).toBe(Object.prototype);
    expect(Object.hasOwn(nestedRecord, '__proto__')).toBe(true);
    expect(nestedRecord.__proto__).toBe('nested-data');
    expect(
      engine.executeSql(
        'SELECT payload, title, id FROM items WHERE id = $1',
        [1],
        'array',
      ).rows,
    ).toEqual([[selectedPayload, 'typed-default', 1]]);
    expectRequest(recording, WASM_OPERATION.executeSql).toEqual({
      sql: 'SELECT payload, title, id FROM items WHERE id = $1',
      params: [1],
      arrayRows: true,
    });

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
    expect((preparedCall.payload as {params: JsonValue[]}).params).toEqual(
      preparedParams,
    );
    expect(withoutRows(prepared)).toEqual(responsePayload(preparedCall));
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
    expect(execCall.payload).toEqual({sql: script});
    expect(scripted.map(withoutRows)).toEqual(responsePayload(execCall));
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

// Every test shares one instance of the module, and so one memory, unless it
// asks for an instance of its own, whose memory is as small as a new one is.
async function loadStructuredModule(own = false): Promise<StructuredModule> {
  const wasm = (await import(
    /* @vite-ignore */ `${pathToFileURL(artifactModule).href}${own ? `?own=${ownModules++}` : ''}`
  )) as StructuredModule;
  await wasm.default({module_or_path: readFileSync(artifactWasm)});
  return wasm;
}

let ownModules = 0;

type DecodedResult = Omit<SqlResult, 'data'> & SqlData & {rows: Row[]};

// Every result the real engine writes gets the full check that a custom
// Worker's results get on the page, and then its rows are parsed as the page
// parses them. A result that published nothing arrives as the array its
// response is, where it has the shape of one, and is read as the page reads
// that: into the result its JSON would have been.
function decode(sent: SqlResult | StatementResult): DecodedResult {
  if (!Array.isArray(sent)) {
    expect(isRpcResult('executeSql', sent)).toBe(true);
    const {data, ...header} = sent;
    return {...header, ...(JSON.parse(data) as SqlData)} as DecodedResult;
  }
  // Its first two slots are left for whoever posts it.
  expect(sent.slice(0, 2)).toEqual([0, 0]);
  expect(isStatementResponse([PROTOCOL_VERSION, 1, ...sent.slice(2)], true)).toBe(true);
  const command = sent[2];
  const header = {
    command: STATEMENT_COMMANDS[command]!,
    revision: sent[3],
    rowCount: sent[4],
  };
  if (command === STATEMENT_SELECT) {
    const data = sent[5] as string;
    expect(isSqlDataText(data)).toBe(true);
    return {...header, tables: [], keys: {}, ...(JSON.parse(data) as SqlData)} as DecodedResult;
  }
  const tables: string[] = [];
  const keys: ChangedKeys = {};
  if (sent.length > 5) {
    tables.push(sent[5] as string);
  }
  if (sent.length > 6) {
    const width = sent[6] as number;
    const rows: Row[] = [];
    for (let at = 7 + width; at < sent.length; at += width) {
      const row: Row = {};
      for (let column = 0; column < width; column++) {
        // Defined rather than assigned, so that a `__proto__` column stays data.
        define(row, sent[7 + column] as string, sent[at + column]!);
      }
      rows.push(row);
    }
    define(keys, tables[0]!, rows);
  }
  return {...header, tables, keys, fields: [], rows: []};
}

function define(target: object, key: string, value: unknown): void {
  Object.defineProperty(target, key, {
    value,
    enumerable: true,
    writable: true,
    configurable: true,
  });
}

function withoutRows({fields: _fields, rows: _rows, ...header}: DecodedResult) {
  return header;
}

// Whether two results say the same, to the order of every key's columns and
// the sign of every zero, which a deep comparison alone leaves out.
function sameResult(left: DecodedResult, right: DecodedResult): boolean {
  expect(left).toEqual(right);
  const text = (result: DecodedResult) =>
    JSON.stringify(result, (_key, value: unknown) =>
      Object.is(value, -0) ? '-0' : value,
    );
  expect(text(left)).toBe(text(right));
  expect(Object.keys(left.keys)).toEqual(Object.keys(right.keys));
  for (const table of Object.keys(left.keys)) {
    expect(Object.hasOwn(right.keys, table)).toBe(true);
    for (const [at, key] of left.keys[table]!.entries()) {
      expect(Object.getPrototypeOf(key)).toBe(Object.prototype);
      expect(Object.keys(key)).toEqual(Object.keys(right.keys[table]![at]!));
    }
  }
  return true;
}

function createRecordingEngine(
  wasm: StructuredModule,
  device: PageDevice,
) {
  let recording: RecordingStructuredEngine | undefined;
  class RecordingConstructor extends RecordingStructuredEngine {
    constructor(guardedDevice: PageDevice) {
      super(new wasm.WasmEngine(guardedDevice));
      recording = this;
    }
  }
  const bridged = createStructuredWasmEngine(RecordingConstructor, device);
  if (!recording) {
    throw new Error('The structured WASM recording engine was not created');
  }
  const recorded = recording;
  // A statement's result, with what the engine itself wrote held against what
  // the bridge made of it.
  const checked = (result: SqlResult | StatementResult) => {
    const call = recorded.calls.at(-1)!;
    if (Array.isArray(result)) {
      // The header's bytes, read by a reader of the test's own, are the array.
      const header = decodeStatementHeader(call.header!);
      expect(result).toEqual(
        statementResult(
          header,
          header.command === 'SELECT' ? (call.response as string) : undefined,
        ),
      );
      expect(call.response).toBe(header.command === 'SELECT' ? result[5] : true);
    } else {
      expect(call.header).toBeUndefined();
    }
    return decode(result);
  };
  const engine = {
    ...bridged,
    executeSql: (
      sql: string,
      params: readonly JsonValue[],
      rowMode?: RowMode,
      from?: number,
    ) => checked(bridged.executeSql(sql, params, rowMode, from)),
    executePrepared: (
      statementId: number,
      params: readonly JsonValue[],
      rowMode?: RowMode,
      from?: number,
    ) => checked(bridged.executePrepared(statementId, params, rowMode, from)),
    execSql: (sql: string, rowMode?: RowMode) =>
      bridged.execSql(sql, rowMode).map(decode),
  };
  return {engine, recording, bridged, checked};
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

// A response is JSON text, whose first line holds its envelope, unless the
// call left a header, which stands for the envelope of a success that
// published nothing and for the header of its result.
function responseEnvelope(call: StructuredCall): unknown[] {
  if (call.header) {
    const header = decodeStatementHeader(call.header);
    const keys: ChangedKeys = {};
    if (header.columns) {
      define(
        keys,
        header.table!,
        header.keys!.map((values) =>
          Object.fromEntries(header.columns!.map((column, at) => [column, values[at]!])),
        ),
      );
    }
    return [
      BRIDGE_VERSION,
      0,
      0,
      {
        command: header.command,
        revision: header.revision,
        rowCount: header.rowCount,
        tables: header.table === undefined ? [] : [header.table],
        keys,
      },
    ];
  }
  expect(typeof call.response).toBe('string');
  return JSON.parse((call.response as string).split('\n')[0]!) as unknown[];
}

function responsePayload(call: StructuredCall): unknown {
  return responseEnvelope(call)[3];
}

function expectDisposition(
  engine: RecordingStructuredEngine,
  operation: number,
  expected: 'safe' | 'durable',
): void {
  const expectedTag = expected === 'durable' ? 1 : 0;
  expect(responseEnvelope(lastCall(engine, operation)).slice(0, 3)).toEqual([
    BRIDGE_VERSION,
    0,
    expectedTag,
  ]);
}

function expectFailureDisposition(
  engine: RecordingStructuredEngine,
  operation: number,
): void {
  expect(responseEnvelope(lastCall(engine, operation)).slice(0, 3)).toEqual([
    BRIDGE_VERSION,
    1,
    0,
  ]);
}

function captureError(operation: () => unknown): unknown {
  try {
    operation();
  } catch (error) {
    return error;
  }
  throw new Error('Expected the structured bridge operation to fail');
}
