import type {TableSchema} from '../../../src/protocol.js';
import type {PageDevice} from '../../../src/worker/page-device.js';
import {
  createBinaryWasmEngine,
  type RawBinaryWasmEngineConstructor,
  type WireTableSchema,
} from '../../../src/worker/wasm-wire.js';

const PAGE_SIZE = 4096;
const pagedWasmModule = new URL(
  '../../../dist/wasm/tinygres_wasm.js',
  import.meta.url,
).href;

interface PagedWasmModule {
  default(): Promise<unknown>;
  WasmEngine: RawBinaryWasmEngineConstructor;
}

interface SharedPages {
  readonly pages: Uint8Array[];
  closes: number;
}

class MemoryPageDevice implements PageDevice {
  readonly #shared: SharedPages;
  #closed = false;

  constructor(shared: SharedPages) {
    this.#shared = shared;
  }

  pageCount(): number {
    this.#assertOpen();
    return this.#shared.pages.length;
  }

  readPage(low: number, high: number, target: Uint8Array): number {
    this.#assertOpen();
    this.#assertPageBuffer(target);
    const page = high === 0 ? this.#shared.pages[low] : undefined;
    if (page === undefined) {
      throw new RangeError('The requested memory page is not allocated');
    }
    target.set(page);
    return PAGE_SIZE;
  }

  writePage(low: number, high: number, source: Uint8Array): number {
    this.#assertOpen();
    this.#assertPageBuffer(source);
    if (high !== 0 || low > this.#shared.pages.length) {
      throw new RangeError('The requested memory page cannot be written');
    }
    // Consume the borrowed WASM view synchronously. Never retain it.
    this.#shared.pages[low] = source.slice();
    return PAGE_SIZE;
  }

  flush(): void {
    this.#assertOpen();
  }

  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#shared.closes += 1;
  }

  #assertOpen(): void {
    if (this.#closed) {
      throw new Error('The memory page device is closed');
    }
  }

  #assertPageBuffer(value: Uint8Array): void {
    if (value.byteLength !== PAGE_SIZE) {
      throw new RangeError('A memory page transfer must contain exactly one page');
    }
  }
}

self.addEventListener('message', (event: MessageEvent<unknown>) => {
  if (
    typeof event.data !== 'object' ||
    event.data === null ||
    !('type' in event.data) ||
    event.data.type !== 'run'
  ) {
    return;
  }
  void run().then(
    (report) => self.postMessage({ok: true, report}),
    (error) =>
      self.postMessage({
        ok: false,
        message: error instanceof Error ? error.message : String(error),
      }),
  );
});

async function run(): Promise<{
  closes: number;
  committedRevision: number;
  defineRevision: number;
  pageCount: number;
  reopenedRevision: number;
  rows: unknown[];
}> {
  const wasm = (await import(
    /* @vite-ignore */ pagedWasmModule
  )) as PagedWasmModule;
  await wasm.default();

  const shared: SharedPages = {pages: [], closes: 0};
  const schema: WireTableSchema = {
    name: 'items',
    primaryKey: ['id'],
    columns: [
      {name: 'id', dataType: 'integer', nullable: false},
      {
        name: 'title',
        dataType: 'text',
        nullable: false,
        default: 'default title',
      },
    ],
  };
  const engine = createBinaryWasmEngine(
    wasm.WasmEngine,
    new MemoryPageDevice(shared),
  );
  engine.defineTables([schema as TableSchema]);
  const defineRevision = engine.revision();
  engine.applyBatch({
    sourceId: 'browser-memory-proof',
    cursor: {kind: 'sequence', value: '1'},
    changes: [{type: 'upsert', table: 'items', row: {id: 1}}],
  });

  engine.beginTransaction();
  engine.executeSql('INSERT INTO items (id, title) VALUES ($1, $2)', [
    2,
    'rolled back',
  ]);
  engine.rollbackTransaction();

  engine.beginTransaction();
  engine.executeSql('INSERT INTO items (id, title) VALUES ($1, $2)', [
    3,
    'committed',
  ]);
  const committed = engine.commitTransaction();
  const committedRevision = committed.revision;
  engine.close();

  const reopened = createBinaryWasmEngine(
    wasm.WasmEngine,
    new MemoryPageDevice(shared),
  );
  const result = reopened.query({
    table: 'items',
    columns: ['id', 'title'],
    filters: [],
    orderBy: [{column: 'id', direction: 'asc', nulls: 'default'}],
  });
  const reopenedRevision = reopened.revision();
  const pageCount = shared.pages.length;
  reopened.close();

  return {
    closes: shared.closes,
    committedRevision,
    defineRevision,
    pageCount,
    reopenedRevision,
    rows: result.rows,
  };
}
