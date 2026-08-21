import {existsSync, readFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import {resolve} from 'node:path';

import {describe, expect, it} from 'vitest';

import type {TableSchema} from '../../src/protocol.js';
import type {PageDevice} from '../../src/worker/page-device.js';
import {
  createBinaryWasmEngine,
  type RawBinaryWasmEngine,
  type WireTableSchema,
} from '../../src/worker/wasm-wire.js';

const binaryDirectory =
  process.env.TINYGRES_PAGED_WASM_DIR ?? resolve('dist/wasm');
const binaryModule = `${binaryDirectory}/tinygres_wasm.js`;
const binaryWasm = `${binaryDirectory}/tinygres_wasm_bg.wasm`;

interface BinaryModule {
  default(options: {module_or_path: Uint8Array}): Promise<unknown>;
  WasmEngine: new (device: PageDevice) => RawBinaryWasmEngine;
}

class MemoryPageDevice {
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
    this.closed++;
  }
}

const runIfBinaryExists =
  existsSync(binaryModule) && existsSync(binaryWasm) ? describe : describe.skip;

runIfBinaryExists('binary TypeScript/Rust wire contract', () => {
  it('round-trips every public model across the current Rust WASM binary', async () => {
    const wasm = (await import(
      /* @vite-ignore */ pathToFileURL(binaryModule).href
    )) as BinaryModule;
    await wasm.default({module_or_path: readFileSync(binaryWasm)});

    const device = new MemoryPageDevice();
    const engine = createBinaryWasmEngine(wasm.WasmEngine, device);
    const schema: WireTableSchema = {
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
        {name: 'payload', dataType: 'json'},
      ],
    };

    engine.defineTables([schema as TableSchema]);
    // The exact-publish disposition seam must classify a repeated definition
    // as safe while preserving the same public no-op behavior.
    engine.defineTables([schema as TableSchema]);
    expect(
      engine.applyBatch({
        changes: [
          {
            type: 'upsert',
            table: 'items',
            row: {id: 1, payload: {__proto__: 'data', signedZero: -0}},
          },
        ],
      }).tables,
    ).toEqual(['items']);

    const planned = engine.query({
      table: 'items',
      columns: ['id', 'title', 'payload'],
      filters: [{column: 'id', operator: 'eq', value: 1}],
      orderBy: [{column: 'id', direction: 'desc', nulls: 'last'}],
      limit: 1,
      offset: 0,
    });
    expect(Object.getPrototypeOf(planned.rows[0])).toBeNull();
    expect(planned.fields).toEqual([
      {name: 'id', dataTypeID: 20},
      {name: 'title', dataTypeID: 25},
      {name: 'payload', dataTypeID: 114},
    ]);
    expect(planned.rows).toEqual([
      {
        id: 1,
        title: 'typed-default',
        payload: {__proto__: 'data', signedZero: 0},
      },
    ]);

    engine.beginTransaction();
    engine.executeSql(
      'INSERT INTO items (id, title, payload) VALUES ($1, $2, $3)',
      [2, 'sql', {nested: [true, null, 1.25]}],
    );
    expect(engine.inTransaction()).toBe(true);
    expect(
      engine.executeSql('SELECT * FROM items ORDER BY id', []).rows,
    ).toHaveLength(2);
    engine.rollbackTransaction();
    expect(engine.executeSql('SELECT * FROM items', []).rows).toHaveLength(1);

    engine.beginTransaction();
    engine.executeSql('INSERT INTO items (id, title) VALUES ($1, $2)', [
      4,
      'committed',
    ]);
    const committed = engine.commitTransaction();
    expect(committed.tables).toEqual(['items']);
    expect(engine.inTransaction()).toBe(false);
    const selected = engine.executeSql(
      'SELECT title FROM items WHERE id = $1',
      [4],
    );
    expect(selected.fields).toEqual([{name: 'title', dataTypeID: 25}]);
    expect(selected.rows).toEqual([{title: 'committed'}]);

    const replacement = engine.replaceTableSnapshot(schema as TableSchema, [
      {id: 3, title: 'snapshot', payload: {wide: 'ok'}},
    ]);
    expect(replacement.tables).toEqual(['items']);
    expect(engine.executeSql('SELECT id FROM items', []).rows).toEqual([
      {id: 3},
    ]);
    expect(
      engine.execSql(
        'SELECT id FROM items LIMIT 0; SELECT COUNT(*) AS total FROM items LIMIT 0',
      ),
    ).toEqual([
      {
        command: 'SELECT',
        revision: replacement.revision,
        rowCount: 0,
        fields: [{name: 'id', dataTypeID: 20}],
        rows: [],
        tables: [],
      },
      {
        command: 'SELECT',
        revision: replacement.revision,
        rowCount: 0,
        fields: [{name: 'total', dataTypeID: 20}],
        rows: [],
        tables: [],
      },
    ]);
    expect(engine.revision()).toBeGreaterThan(0);

    engine.close();
    engine.close();
    expect(device.closed).toBe(1);
  });
});
