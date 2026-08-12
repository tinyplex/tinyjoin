import {describe, expect, it, vi} from 'vitest';

import type {
  ApplyOutcome,
  ChangeBatch,
  JsonValue,
  QueryPlan,
  QueryResult,
  Row,
  SqlResult,
  TableSchema,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {createPersistentEngine} from '../../src/worker/persistent-engine.ts';
import {
  decodeJournalTransaction,
  encodeJournalTransaction,
  JOURNAL_TRANSACTION_VERSION,
} from '../../src/worker/journal-payload.ts';
import {
  StorageError,
  type SnapshotCandidate,
  type SnapshotStore,
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
  transactionState: EngineState | undefined;
  readonly transactionTables = new Set<string>();

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

  executeSql(sql: string, params: JsonValue[]): SqlResult {
    const table = this.state.tables.posts;
    if (!table) {
      throw Object.assign(new Error('missing table'), {
        code: 'TABLE_NOT_FOUND',
      });
    }
    const command = sql.trim().split(/\s+/, 1)[0]!.toUpperCase();
    if (command === 'INSERT') {
      table.rows.push({id: params[0] ?? table.rows.length + 1});
    } else if (command === 'DELETE') {
      table.rows = [];
    } else {
      throw Object.assign(new Error('unsupported statement'), {
        code: 'UNSUPPORTED_SQL',
      });
    }
    if (this.inTransaction()) {
      this.transactionTables.add('posts');
    } else {
      this.state.revision += 1;
    }
    return {
      command,
      revision: this.state.revision,
      rowCount: 1,
      rows: [],
      tables: ['posts'],
    };
  }

  beginTransaction(): void {
    if (this.transactionState) {
      throw Object.assign(new Error('transaction active'), {
        code: 'TRANSACTION_ACTIVE',
      });
    }
    this.transactionState = structuredClone(this.state);
    this.transactionTables.clear();
  }

  commitTransaction(): ApplyOutcome {
    if (!this.transactionState) {
      throw Object.assign(new Error('no active transaction'), {
        code: 'NO_ACTIVE_TRANSACTION',
      });
    }
    const tables = [...this.transactionTables].sort();
    if (tables.length > 0) {
      this.state.revision += 1;
    }
    this.transactionState = undefined;
    this.transactionTables.clear();
    return {revision: this.state.revision, tables};
  }

  rollbackTransaction(): void {
    if (!this.transactionState) {
      throw Object.assign(new Error('no active transaction'), {
        code: 'NO_ACTIVE_TRANSACTION',
      });
    }
    this.state = this.transactionState;
    this.transactionState = undefined;
    this.transactionTables.clear();
  }

  inTransaction(): boolean {
    return this.transactionState !== undefined;
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
      throw Object.assign(new Error('future'), {
        code: 'UNSUPPORTED_SNAPSHOT',
      });
    }
    this.state = JSON.parse(decoder.decode(snapshot)) as EngineState;
    this.transactionState = undefined;
    this.transactionTables.clear();
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

class MemoryJournalSnapshotStore extends MemorySnapshotStore {
  readonly checkpoints: Uint8Array[] = [];
  readonly payloads: {sequence: bigint; payload: Uint8Array}[] = [];
  sequence = 0n;
  appendFailure: unknown;

  append(payload: Uint8Array): bigint {
    if (this.appendFailure !== undefined) {
      throw this.appendFailure;
    }
    this.sequence += 1n;
    this.payloads.push({sequence: this.sequence, payload: payload.slice()});
    return this.sequence;
  }

  checkpoint(snapshot: Uint8Array): void {
    if (this.failure !== undefined) {
      throw this.failure;
    }
    this.checkpoints.push(snapshot.slice());
    this.payloads.length = 0;
  }

  override commit(snapshot: Uint8Array): void {
    this.checkpoint(snapshot);
  }

  journalRecordCount(): number {
    return this.payloads.length;
  }

  journalByteLength(): number {
    return this.payloads.reduce(
      (bytes, record) => bytes + record.payload.byteLength,
      28,
    );
  }
}

const postsSchema = {name: 'posts', primaryKey: ['id']} satisfies TableSchema;

describe('persistent worker engine', () => {
  it('journals deterministic mutations and replays them from one checkpoint', () => {
    const store = new MemoryJournalSnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);
    expect(store.checkpoints).toHaveLength(1);

    engine.defineTables([postsSchema]);
    engine.replaceTableSnapshot(postsSchema, [{id: 1, title: 'one'}]);
    engine.applyBatch({
      changes: [
        {type: 'upsert', table: 'posts', row: {id: 2, title: 'two'}},
      ],
    });
    expect(store.checkpoints).toHaveLength(1);
    expect(store.payloads).toHaveLength(3);
    expect(
      store.payloads.map(({payload}) =>
        decodeJournalTransaction(payload).mutations[0]?.type,
      ),
    ).toEqual(['defineTables', 'replaceTableSnapshot', 'applyBatch']);

    const restoredStore = new MemorySnapshotStore([
      {
        slot: 0,
        generation: 1n,
        snapshot: store.checkpoints[0]!,
        journal: {
          baseSequence: 0n,
          records: store.payloads,
          validBytes: store.journalByteLength(),
          tail: 'clean',
        },
      },
    ]);
    const restored = createPersistentEngine(new StateEngine(), restoredStore);
    expect(restored.revision()).toBe(2);
    expect(restored.query({table: 'posts', filters: []}).rows).toEqual([
      {id: 1, title: 'one'},
      {id: 2, title: 'two'},
    ]);
  });

  it('journals an explicit SQL transaction as one replayable record', () => {
    const store = new MemoryJournalSnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);
    engine.defineTables([postsSchema]);
    store.checkpoint(engine.exportSnapshot());
    store.payloads.length = 0;

    engine.beginTransaction();
    engine.executeSql('INSERT INTO posts (id) VALUES ($1)', [1]);
    engine.executeSql('INSERT INTO posts (id) VALUES ($1)', [2]);
    engine.commitTransaction();

    expect(store.payloads).toHaveLength(1);
    expect(decodeJournalTransaction(store.payloads[0]!.payload)).toMatchObject({
      revisionBefore: 0,
      revisionAfter: 1,
      mutations: [
        {
          type: 'executeSql',
          statements: [
            {sql: 'INSERT INTO posts (id) VALUES ($1)', params: [1]},
            {sql: 'INSERT INTO posts (id) VALUES ($1)', params: [2]},
          ],
        },
      ],
    });
  });

  it('restores memory when a journal append fails', () => {
    const store = new MemoryJournalSnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);
    engine.defineTables([postsSchema]);
    engine.replaceTableSnapshot(postsSchema, [{id: 1}]);
    store.appendFailure = Object.assign(new Error('quota'), {
      code: 'STORAGE_QUOTA_EXCEEDED',
    });

    expect(() =>
      engine.applyBatch({
        changes: [{type: 'upsert', table: 'posts', row: {id: 2}}],
      }),
    ).toThrowError(expect.objectContaining({code: 'STORAGE_QUOTA_EXCEEDED'}));
    expect(engine.revision()).toBe(1);
    expect(engine.query({table: 'posts', filters: []}).rows).toEqual([{id: 1}]);
  });

  it('fails closed when a journal revision or payload is corrupt', () => {
    const base = new StateEngine().exportSnapshot();
    const payload = encodeJournalTransaction({
      version: JOURNAL_TRANSACTION_VERSION,
      revisionBefore: 8,
      revisionAfter: 9,
      mutations: [{type: 'defineTables', schemas: [postsSchema]}],
    });
    const store = new MemorySnapshotStore([
      {
        slot: 0,
        generation: 1n,
        snapshot: base,
        journal: {
          baseSequence: 0n,
          records: [{sequence: 1n, payload}],
          validBytes: payload.byteLength,
          tail: 'clean',
        },
      },
    ]);
    expect(() => createPersistentEngine(new StateEngine(), store)).toThrowError(
      expect.objectContaining({code: 'STORAGE_JOURNAL_CORRUPT'}),
    );
  });

  it('does not silently lose newer records by falling back across journal corruption', () => {
    const valid = new StateEngine().exportSnapshot();
    const store = new MemorySnapshotStore([
      {
        slot: 1,
        generation: 2n,
        snapshot: valid,
        journalError: new StorageError(
          'STORAGE_JOURNAL_CORRUPT',
          'framed journal corruption',
        ),
      },
      {slot: 0, generation: 1n, snapshot: valid},
    ]);
    expect(() => createPersistentEngine(new StateEngine(), store)).toThrowError(
      expect.objectContaining({code: 'STORAGE_JOURNAL_CORRUPT'}),
    );
    expect(store.selected).toBeUndefined();
  });

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

  it('persists all transaction writes in one snapshot when committing', () => {
    const store = new MemorySnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);
    engine.defineTables([postsSchema]);
    store.commits.length = 0;

    engine.beginTransaction();
    expect(
      engine.executeSql('INSERT INTO posts (id) VALUES ($1)', [1]),
    ).toMatchObject({
      command: 'INSERT',
      revision: 0,
      tables: ['posts'],
    });
    engine.executeSql('INSERT INTO posts (id) VALUES ($1)', [2]);
    expect(engine.query({table: 'posts', filters: []}).rows).toEqual([
      {id: 1},
      {id: 2},
    ]);
    expect(store.commits).toHaveLength(0);

    expect(engine.commitTransaction()).toEqual({
      revision: 1,
      tables: ['posts'],
    });
    expect(store.commits).toHaveLength(1);

    const restored = createPersistentEngine(
      new StateEngine(),
      new MemorySnapshotStore([
        {slot: 0, generation: 1n, snapshot: store.commits[0]!},
      ]),
    );
    expect(restored.revision()).toBe(1);
    expect(restored.query({table: 'posts', filters: []}).rows).toEqual([
      {id: 1},
      {id: 2},
    ]);
  });

  it('rolls staged transaction writes back without persisting a snapshot', () => {
    const store = new MemorySnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);
    engine.replaceTableSnapshot(postsSchema, [{id: 1}]);
    store.commits.length = 0;

    engine.beginTransaction();
    engine.executeSql('DELETE FROM posts', []);
    expect(engine.query({table: 'posts', filters: []}).rows).toEqual([]);
    engine.rollbackTransaction();

    expect(store.commits).toHaveLength(0);
    expect(engine.revision()).toBe(1);
    expect(engine.query({table: 'posts', filters: []}).rows).toEqual([{id: 1}]);
  });

  it('restores committed memory when a transaction snapshot cannot flush', () => {
    const store = new MemorySnapshotStore();
    const engine = createPersistentEngine(new StateEngine(), store);
    engine.replaceTableSnapshot(postsSchema, [{id: 1}]);
    store.failure = Object.assign(new Error('quota'), {
      code: 'STORAGE_QUOTA_EXCEEDED',
    });

    engine.beginTransaction();
    engine.executeSql('INSERT INTO posts (id) VALUES ($1)', [2]);
    expect(() => engine.commitTransaction()).toThrowError(
      expect.objectContaining({code: 'STORAGE_QUOTA_EXCEEDED'}),
    );

    expect(engine.inTransaction()).toBe(false);
    expect(engine.revision()).toBe(1);
    expect(engine.query({table: 'posts', filters: []}).rows).toEqual([{id: 1}]);
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
