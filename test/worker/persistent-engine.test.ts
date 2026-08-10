import {describe, expect, it, vi} from 'vitest';

import type {
  ApplyOutcome,
  ChangeBatch,
  JsonValue,
  QueryPlan,
  QueryResult,
  Row,
  TableSchema,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {createPersistentEngine} from '../../src/worker/persistent-engine.ts';
import type {
  SnapshotCandidate,
  SnapshotStore,
} from '../../src/worker/snapshot-store.ts';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

type EngineState = {
  revision: number;
  tables: Record<string, {schema: TableSchema; rows: Row[]}>;
};

class StateEngine implements WorkerEngine {
  state: EngineState = {revision: 0, tables: {}};
  closed = false;
  failImport = false;

  defineTable(schema: TableSchema): void {
    const existing = this.state.tables[schema.name];
    if (existing) {
      if (JSON.stringify(existing.schema) !== JSON.stringify(schema)) {
        throw Object.assign(new Error('schema conflict'), {
          code: 'INVALID_SCHEMA',
        });
      }
      return;
    }
    this.state.tables[schema.name] = {schema, rows: []};
  }

  defineTables(schemas: TableSchema[]): void {
    for (const schema of schemas) {
      this.defineTable(schema);
    }
  }

  replaceTableSnapshot(schema: TableSchema, rows: Row[]): ApplyOutcome {
    this.defineTable(schema);
    this.state.tables[schema.name]!.rows = structuredClone(rows);
    this.state.revision += 1;
    return {revision: this.state.revision, tables: [schema.name]};
  }

  applyBatch(batch: ChangeBatch): ApplyOutcome {
    const changed = new Set<string>();
    for (const change of batch.changes) {
      const table = this.state.tables[change.table];
      if (!table) {
        throw Object.assign(new Error('missing table'), {
          code: 'TABLE_NOT_FOUND',
        });
      }
      if (change.type === 'upsert') {
        const id = change.row.id;
        table.rows = [
          ...table.rows.filter((row) => row.id !== id),
          structuredClone(change.row),
        ];
      } else {
        table.rows = table.rows.filter((row) => row.id !== change.key.id);
      }
      changed.add(change.table);
    }
    if (changed.size > 0) {
      this.state.revision += 1;
    }
    return {revision: this.state.revision, tables: [...changed].sort()};
  }

  query(plan: QueryPlan): QueryResult {
    const table = this.state.tables[plan.table];
    if (!table) {
      throw Object.assign(new Error('missing table'), {
        code: 'TABLE_NOT_FOUND',
      });
    }
    return {revision: this.state.revision, rows: structuredClone(table.rows)};
  }

  querySql(_sql: string, _params: JsonValue[]): QueryResult {
    return this.query({table: 'posts', filters: []});
  }

  revision(): number {
    return this.state.revision;
  }

  exportSnapshot(): Uint8Array {
    return encoder.encode(JSON.stringify(this.state));
  }

  importSnapshot(snapshot: Uint8Array): void {
    if (this.failImport) {
      throw new Error('rollback import failed');
    }
    if (snapshot[0] === 0xff) {
      throw Object.assign(new Error('invalid'), {code: 'INVALID_SNAPSHOT'});
    }
    if (snapshot[0] === 0xfe) {
      throw Object.assign(new Error('future'), {code: 'UNSUPPORTED_SNAPSHOT'});
    }
    this.state = JSON.parse(decoder.decode(snapshot)) as EngineState;
  }

  close(): void {
    this.closed = true;
  }
}

class MemorySnapshotStore implements SnapshotStore {
  hadData: boolean;
  readonly commits: Uint8Array[] = [];
  selected: SnapshotCandidate | undefined;
  failure: unknown;
  closed = false;
  readonly #candidates: SnapshotCandidate[];

  constructor(candidates: SnapshotCandidate[] = [], hadData = false) {
    this.#candidates = candidates;
    this.hadData = hadData || candidates.length > 0;
  }

  candidates(): readonly SnapshotCandidate[] {
    return this.#candidates;
  }

  select(candidate: SnapshotCandidate): void {
    this.selected = candidate;
  }

  commit(snapshot: Uint8Array): void {
    if (this.failure !== undefined) {
      throw this.failure;
    }
    this.commits.push(snapshot.slice());
  }

  close(): void {
    this.closed = true;
  }
}

const postsSchema = {name: 'posts', primaryKey: ['id']} satisfies TableSchema;

describe('persistent worker engine', () => {
  it('persists schema-only changes without changing the engine revision', () => {
    const store = new MemorySnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);

    engine.defineTables([postsSchema]);
    expect(engine.revision()).toBe(0);
    expect(store.commits).toHaveLength(1);

    engine.defineTables([postsSchema]);
    expect(store.commits).toHaveLength(1);

    const restoredBase = new StateEngine();
    const restoredStore = new MemorySnapshotStore([
      {slot: 0, generation: 1n, snapshot: store.commits[0]!},
    ]);
    const restored = createPersistentEngine(restoredBase, restoredStore);
    expect(restored.query({table: 'posts', filters: []}).rows).toEqual([]);
    expect(restored.revision()).toBe(0);
  });

  it('rolls memory back when exporting or flushing a mutation fails', () => {
    const store = new MemorySnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);
    engine.replaceTableSnapshot(postsSchema, [{id: 1, title: 'before'}]);
    store.failure = Object.assign(new Error('quota'), {
      code: 'STORAGE_QUOTA_EXCEEDED',
    });

    expect(() =>
      engine.applyBatch({
        changes: [
          {
            type: 'upsert',
            table: 'posts',
            row: {id: 1, title: 'after'},
          },
        ],
      }),
    ).toThrow('quota');
    expect(engine.revision()).toBe(1);
    expect(engine.query({table: 'posts', filters: []}).rows).toEqual([
      {id: 1, title: 'before'},
    ]);
  });

  it('poisons the engine when rollback cannot restore memory', () => {
    const store = new MemorySnapshotStore();
    const base = new StateEngine();
    const engine = createPersistentEngine(base, store);
    engine.replaceTableSnapshot(postsSchema, [{id: 1, title: 'before'}]);
    store.failure = new Error('flush failed');
    base.failImport = true;

    expect(() =>
      engine.applyBatch({
        changes: [
          {
            type: 'upsert',
            table: 'posts',
            row: {id: 1, title: 'after'},
          },
        ],
      }),
    ).toThrowError(expect.objectContaining({code: 'STORAGE_ROLLBACK_FAILED'}));
    expect(store.closed).toBe(true);
    expect(base.closed).toBe(true);
    expect(() => engine.query({table: 'posts', filters: []})).toThrowError(
      expect.objectContaining({code: 'STORAGE_ENGINE_POISONED'}),
    );
  });

  it('poisons the engine when a commit outcome is unknown', () => {
    const store = new MemorySnapshotStore();
    const base = new StateEngine();
    const engine = createPersistentEngine(base, store);
    engine.replaceTableSnapshot(postsSchema, [{id: 1}]);
    store.failure = Object.assign(new Error('unknown commit'), {
      code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN',
    });

    expect(() =>
      engine.applyBatch({
        changes: [
          {
            type: 'upsert',
            table: 'posts',
            row: {id: 2},
          },
        ],
      }),
    ).toThrowError(
      expect.objectContaining({code: 'STORAGE_COMMIT_OUTCOME_UNKNOWN'}),
    );
    expect(store.closed).toBe(true);
    expect(base.closed).toBe(true);
    expect(() => engine.revision()).toThrowError(
      expect.objectContaining({code: 'STORAGE_ENGINE_POISONED'}),
    );
  });

  it('falls back from an invalid newest engine snapshot', () => {
    const validEngine = new StateEngine();
    validEngine.replaceTableSnapshot(postsSchema, [{id: 1}]);
    const older = validEngine.exportSnapshot();
    const store = new MemorySnapshotStore([
      {slot: 1, generation: 2n, snapshot: new Uint8Array([0xff])},
      {slot: 0, generation: 1n, snapshot: older},
    ]);

    const engine = createPersistentEngine(new StateEngine(), store);
    expect(store.selected?.generation).toBe(1n);
    expect(engine.query({table: 'posts', filters: []}).rows).toEqual([{id: 1}]);
  });

  it('does not overwrite an unsupported engine snapshot', () => {
    const store = new MemorySnapshotStore([
      {slot: 0, generation: 1n, snapshot: new Uint8Array([0xfe])},
    ]);

    expect(() => createPersistentEngine(new StateEngine(), store)).toThrowError(
      expect.objectContaining({code: 'STORAGE_VERSION_UNSUPPORTED'}),
    );
    expect(store.commits).toHaveLength(0);
  });

  it('rejects nonempty storage when no snapshot is valid', () => {
    const store = new MemorySnapshotStore([], true);
    expect(() => createPersistentEngine(new StateEngine(), store)).toThrowError(
      expect.objectContaining({code: 'STORAGE_CORRUPT'}),
    );
  });

  it('closes the store before the underlying engine', () => {
    const order: string[] = [];
    const base = new StateEngine();
    const store = new MemorySnapshotStore();
    vi.spyOn(store, 'close').mockImplementation(() => order.push('store'));
    vi.spyOn(base, 'close').mockImplementation(() => order.push('engine'));

    createPersistentEngine(base, store).close?.();
    expect(order).toEqual(['store', 'engine']);
  });
});
