import {describe, expect, it, vi} from 'vitest';

import type {ReplicaSource} from '../../src/adapters/types.ts';
import {
  PROTOCOL_VERSION,
  type ApplyOutcome,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.ts';
import type {LegacyRecoveryEngine} from '../../src/worker/migration-engine.ts';
import {startWorker, type WorkerScope} from '../../src/worker/host.ts';
import {createPersistentEngine} from '../../src/worker/persistent-engine.ts';
import type {SnapshotStore} from '../../src/worker/snapshot-store.ts';

class FakeScope implements WorkerScope {
  readonly posted: Array<WorkerResponse | WorkerEvent> = [];
  closed = false;
  readonly #listeners = new Set<(event: MessageEvent<unknown>) => void>();

  postMessage(message: WorkerResponse | WorkerEvent): void {
    this.posted.push(message);
  }

  addEventListener(
    _type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void {
    this.#listeners.add(listener);
  }

  removeEventListener(
    _type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void {
    this.#listeners.delete(listener);
  }

  close(): void {
    this.closed = true;
  }

  send(message: unknown): void {
    const event = {data: message} as MessageEvent<unknown>;
    for (const listener of this.#listeners) {
      listener(event);
    }
  }
}

function mockEngine() {
  let revision = 0;
  let transactionActive = false;
  const transactionTables = new Set<string>();
  const outcome = (table: string): ApplyOutcome => ({
    revision: ++revision,
    tables: [table],
  });
  const engine: LegacyRecoveryEngine = {
    defineTable: vi.fn(),
    defineTables: vi.fn(),
    replaceTableSnapshot: vi.fn((schema) => outcome(schema.name)),
    applyBatch: vi.fn((batch) => outcome(batch.changes[0]?.table ?? 'none')),
    query: vi.fn(() => ({revision, rows: [{id: 1}]})),
    querySql: vi.fn(() => ({revision, rows: [{id: 1}]})),
    executeSql: vi.fn((sql) => {
      const command = sql.trim().split(/\s+/, 1)[0]!.toUpperCase();
      const table = 'posts';
      if (transactionActive) {
        transactionTables.add(table);
      } else {
        revision += 1;
      }
      return {
        command,
        revision,
        rowCount: 1,
        rows: [],
        tables: [table],
      };
    }),
    prepareDefineTables: vi.fn(() => ({result: null, commit: null})),
    prepareReplaceTableSnapshot: vi.fn((schema) => ({
      result: outcome(schema.name),
      commit: null,
    })),
    prepareApplyBatch: vi.fn((batch) => ({
      result: outcome(batch.changes[0]?.table ?? 'none'),
      commit: null,
    })),
    prepareExecuteSql: vi.fn(() => ({
      result: {
        command: 'INSERT',
        revision,
        rowCount: 0,
        rows: [],
        tables: [],
      },
      commit: null,
    })),
    prepareCommitTransaction: vi.fn(() => ({
      result: {revision, tables: []},
      commit: null,
    })),
    installPreparedCommit: vi.fn(() => ({revision, tables: []})),
    abortPreparedCommit: vi.fn(),
    replayCommit: vi.fn(() => ({revision, tables: []})),
    beginTransaction: vi.fn(() => {
      transactionActive = true;
      transactionTables.clear();
    }),
    commitTransaction: vi.fn(() => {
      const tables = [...transactionTables].sort();
      if (tables.length > 0) {
        revision += 1;
      }
      transactionActive = false;
      transactionTables.clear();
      return {revision, tables};
    }),
    rollbackTransaction: vi.fn(() => {
      transactionActive = false;
      transactionTables.clear();
    }),
    inTransaction: vi.fn(() => transactionActive),
    revision: () => revision,
    exportSnapshot: vi.fn(() => new Uint8Array([revision])),
    importSnapshot: vi.fn((snapshot) => {
      revision = snapshot[0] ?? 0;
    }),
    close: vi.fn(),
  };
  return engine;
}

async function waitForPosted(scope: FakeScope, count: number): Promise<void> {
  await vi.waitFor(() =>
    expect(scope.posted.length).toBeGreaterThanOrEqual(count),
  );
}

function builtInSource() {
  return {
    kind: 'supabase' as const,
    url: 'https://example.supabase.co',
    publishableKey: 'sb_publishable_example',
    tables: [{table: 'posts', primaryKey: ['id']}],
  };
}

function deferred<Value>() {
  let resolve!: (value: Value) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<Value>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return {promise, resolve, reject};
}

describe('startWorker', () => {
  it('routes requests through the engine and returns versioned responses', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [{name: 'posts', primaryKey: ['id']}],
        storage: {kind: 'memory'},
      },
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'querySql',
      params: {sql: 'select * from posts', params: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);

    expect(engine.defineTables).toHaveBeenCalledWith([
      {name: 'posts', primaryKey: ['id']},
    ]);
    expect(engine.querySql).toHaveBeenCalledWith('select * from posts', []);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: {revision: 0, rows: [{id: 1}]},
    });
  });

  it('microbatches table invalidations from adjacent mutations', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'replaceTable',
      params: {
        schema: {name: 'posts', primaryKey: ['id']},
        rows: [],
      },
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'replaceTable',
      params: {
        schema: {name: 'users', primaryKey: ['id']},
        rows: [],
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);

    expect(
      scope.posted.filter(
        (message): message is WorkerEvent => 'event' in message,
      ),
    ).toEqual([
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 2, tables: ['posts', 'users']},
      },
    ]);
  });

  it('invalidates writable SQL immediately but a transaction only when it commits', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'executeSql',
      params: {sql: 'INSERT INTO posts (id) VALUES (1)', params: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: {
        command: 'INSERT',
        revision: 1,
        rowCount: 1,
        rows: [],
        tables: ['posts'],
      },
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 1, tables: ['posts']},
    });
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'beginTransaction',
      params: undefined,
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    expect(scope.posted[0]).toMatchObject({
      id: 3,
      ok: true,
      result: {transactionId: 'tx-1'},
    });
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 4,
      method: 'executeSql',
      params: {
        sql: 'UPDATE posts SET title = $1 WHERE id = $2',
        params: ['changed', 1],
        transactionId: 'tx-1',
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    await new Promise<void>((resolve) => queueMicrotask(resolve));
    expect(scope.posted).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 4,
        ok: true,
        result: {
          command: 'UPDATE',
          revision: 1,
          rowCount: 1,
          rows: [],
          tables: ['posts'],
        },
      },
    ]);

    scope.send({
      v: PROTOCOL_VERSION,
      id: 5,
      method: 'commitTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 5,
      ok: true,
      result: {revision: 2, tables: ['posts']},
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 2, tables: ['posts']},
    });
  });

  it('requires the active transaction token and rollback emits no invalidation', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'beginTransaction',
      params: undefined,
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'executeSql',
      params: {sql: 'DELETE FROM posts', params: []},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 4,
      method: 'rollbackTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 4);
    await new Promise<void>((resolve) => queueMicrotask(resolve));

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 3,
      ok: false,
      error: {
        code: 'TRANSACTION_ACTIVE',
        message: 'Use the active TinyGres transaction for this operation',
      },
    });
    expect(engine.executeSql).not.toHaveBeenCalled();
    expect(engine.rollbackTransaction).toHaveBeenCalledOnce();
    expect(scope.posted.some((message) => 'event' in message)).toBe(false);
  });

  it('rejects local writes while a replication source is configured', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const source: ReplicaSource = {
      id: 'read-only-source',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      start: vi.fn(async () => undefined),
    };
    startWorker({scope, source, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'executeSql',
      params: {sql: 'INSERT INTO posts (id) VALUES (1)', params: []},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'beginTransaction',
      params: undefined,
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);

    for (const id of [2, 3]) {
      expect(scope.posted).toContainEqual({
        v: PROTOCOL_VERSION,
        id,
        ok: false,
        error: {
          code: 'SOURCE_DATABASE_READ_ONLY',
          message:
            'Local SQL writes are disabled while a TinyGres replication source is configured',
        },
      });
    }
    expect(engine.executeSql).not.toHaveBeenCalled();
    expect(engine.beginTransaction).not.toHaveBeenCalled();
  });

  it('rejects malformed messages without invoking the engine', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({v: 99, id: 8, method: 'query', params: {}});
    await waitForPosted(scope, 1);

    expect(scope.posted[0]).toEqual({
      v: PROTOCOL_VERSION,
      id: 8,
      ok: false,
      error: {
        code: 'PROTOCOL_MISMATCH',
        message: 'The worker received an invalid TinyGres protocol request',
      },
    });
    expect(engine.query).not.toHaveBeenCalled();
  });

  it('rejects well-shaped methods with unsafe parameters', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 9,
      method: 'replaceTable',
      params: {table: 'posts', rows: [{id: 1n}]},
    });
    await waitForPosted(scope, 1);

    expect(scope.posted[0]).toMatchObject({
      id: 9,
      ok: false,
      error: {code: 'PROTOCOL_MISMATCH'},
    });
    expect(engine.replaceTableSnapshot).not.toHaveBeenCalled();
  });

  it('fails explicit OPFS initialization instead of falling back to memory', async () => {
    const scope = new FakeScope();
    const engineFactory = vi.fn(async () => {
      throw Object.assign(new Error('OPFS is unavailable'), {
        code: 'OPFS_UNAVAILABLE',
      });
    });
    startWorker({scope, durableEngineFactory: engineFactory});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 10,
      method: 'init',
      params: {
        schemas: [{name: 'posts', primaryKey: ['id']}],
        storage: {kind: 'opfs', name: 'unit-test'},
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);

    expect(scope.posted[0]).toMatchObject({
      id: 10,
      ok: false,
      error: {code: 'OPFS_UNAVAILABLE'},
    });
    expect(engineFactory).toHaveBeenCalledWith({
      kind: 'opfs',
      name: 'unit-test',
    });
  });

  it('does not wrap a storage-owning engine factory in legacy persistence', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const engineFactory = vi.fn(async () => engine);
    startWorker({scope, durableEngineFactory: engineFactory});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 10,
      method: 'init',
      params: {
        schemas: [{name: 'posts', primaryKey: ['id']}],
        storage: {kind: 'opfs', name: 'factory-owned'},
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);

    expect(scope.posted[0]).toEqual({
      v: PROTOCOL_VERSION,
      id: 10,
      ok: true,
      result: {revision: 0, sourceConfigured: false},
    });
    expect(engineFactory).toHaveBeenCalledWith({
      kind: 'opfs',
      name: 'factory-owned',
    });
    expect(engine.defineTables).toHaveBeenCalledOnce();
  });

  it('makes a failed initialization terminal and releases its engine', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.mocked(engine.defineTables).mockImplementation(() => {
      throw Object.assign(new Error('schema conflict'), {
        code: 'INVALID_SCHEMA',
      });
    });
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 11,
      method: 'init',
      params: {
        schemas: [{name: 'posts', primaryKey: ['slug']}],
        storage: {kind: 'memory'},
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 11,
      ok: false,
      error: {code: 'INVALID_SCHEMA', message: 'schema conflict'},
    });
    expect(engine.close).toHaveBeenCalledOnce();
  });

  it('releases the existing engine when a second init changes storage', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'init',
      params: {
        schemas: [],
        storage: {kind: 'opfs', name: 'different-storage'},
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: false,
      error: {
        code: 'STORAGE_ALREADY_INITIALIZED',
        message:
          'The TinyGres worker is already initialized with different storage',
      },
    });
    expect(engine.close).toHaveBeenCalledOnce();
  });

  it('does not acknowledge or invalidate a mutation that fails to flush', async () => {
    const scope = new FakeScope();
    const baseEngine = mockEngine();
    const store: SnapshotStore = {
      hadData: false,
      candidates: () => [],
      select: vi.fn(),
      commit: () => {
        throw Object.assign(new Error('quota exhausted'), {
          code: 'STORAGE_QUOTA_EXCEEDED',
          retryable: true,
        });
      },
      close: vi.fn(),
    };
    const engine = createPersistentEngine(baseEngine, store);
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'replaceTable',
      params: {
        schema: {name: 'posts', primaryKey: ['id']},
        rows: [{id: 1}],
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    await new Promise<void>((resolve) => queueMicrotask(() => resolve()));

    expect(scope.posted).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 2,
        ok: false,
        error: {
          code: 'STORAGE_QUOTA_EXCEEDED',
          message: 'quota exhausted',
          retryable: true,
        },
      },
    ]);
    expect(baseEngine.revision()).toBe(0);
  });

  it('starts a future source adapter behind the worker boundary', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const source: ReplicaSource = {
      id: 'test-source',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      async start(context) {
        await context.replaceTable({name: 'posts', primaryKey: ['id']}, [
          {id: 1, title: 'from source'},
        ]);
        context.setSyncState({
          phase: 'live-best-effort',
          sourceId: 'test-source',
        });
      },
    };
    startWorker({scope, source, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 4);

    expect(engine.replaceTableSnapshot).toHaveBeenCalledWith(
      {name: 'posts', primaryKey: ['id']},
      [{id: 1, title: 'from source'}],
    );
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {phase: 'connecting', sourceId: 'test-source'},
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {phase: 'live-best-effort', sourceId: 'test-source'},
    });
  });

  it('normalizes and starts a built-in source after local init succeeds', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const source: ReplicaSource = {
      id: 'supabase:https://example.supabase.co/',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      async start(context) {
        context.setSyncState({
          phase: 'live-best-effort',
          sourceId: this.id,
        });
      },
      close: vi.fn(async () => undefined),
    };
    const builtinSourceFactory = vi.fn(async () => source);
    startWorker({
      scope,
      builtinSourceFactory,
      durableEngineFactory: async () => engine,
    });

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [{name: 'users', primaryKey: ['id']}],
        storage: {kind: 'memory'},
        source: builtInSource(),
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);

    expect(engine.defineTables).toHaveBeenCalledWith([
      {name: 'users', primaryKey: ['id']},
      {name: 'posts', primaryKey: ['id']},
    ]);
    expect(scope.posted[0]).toEqual({
      v: PROTOCOL_VERSION,
      id: 1,
      ok: true,
      result: {revision: 0, sourceConfigured: true},
    });
    expect(scope.posted[1]).toEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {
        phase: 'connecting',
        sourceId: 'supabase:https://example.supabase.co/',
      },
    });
    expect(builtinSourceFactory).toHaveBeenCalledWith({
      kind: 'supabase',
      url: 'https://example.supabase.co/',
      publishableKey: 'sb_publishable_example',
      tables: [
        {
          schema: 'public',
          table: 'posts',
          primaryKey: ['id'],
          localName: 'posts',
        },
      ],
      id: 'supabase:https://example.supabase.co/',
      pageSize: 500,
      maxSnapshotPasses: 3,
    });
  });

  it('keeps ready local and closes a lazily constructing source deterministically', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const construction = deferred<ReplicaSource>();
    const source: ReplicaSource = {
      id: 'late-source',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      start: vi.fn(async () => undefined),
      close: vi.fn(async () => undefined),
    };
    startWorker({
      scope,
      builtinSourceFactory: () => construction.promise,
      durableEngineFactory: async () => engine,
    });
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [],
        storage: {kind: 'memory'},
        source: builtInSource(),
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);
    expect(scope.posted[0]).toMatchObject({id: 1, ok: true});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'querySql',
      params: {sql: 'select * from posts', params: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: {revision: 0, rows: [{id: 1}]},
    });

    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'close',
      params: undefined,
    } satisfies WorkerRequest);
    await new Promise<void>((resolve) => queueMicrotask(resolve));
    expect(scope.posted).not.toContainEqual(expect.objectContaining({id: 3}));

    construction.resolve(source);
    await vi.waitFor(() => expect(scope.closed).toBe(true));
    expect(source.start).not.toHaveBeenCalled();
    expect(source.close).toHaveBeenCalledOnce();
    expect(engine.close).toHaveBeenCalledOnce();
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 3,
      ok: true,
      result: undefined,
    });
  });

  it('reports lazy source construction failure without poisoning close', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({
      scope,
      builtinSourceFactory: async () => {
        throw Object.assign(new Error('source module failed to load'), {
          code: 'SOURCE_IMPORT_FAILED',
        });
      },
      durableEngineFactory: async () => engine,
    });
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [],
        storage: {kind: 'memory'},
        source: builtInSource(),
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {
        phase: 'error',
        sourceId: 'supabase:https://example.supabase.co/',
        error: {
          code: 'SOURCE_IMPORT_FAILED',
          message: 'source module failed to load',
        },
      },
    });

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'close',
      params: undefined,
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: undefined,
    });
    expect(engine.close).toHaveBeenCalledOnce();
  });

  it('rejects conflicting custom and built-in source configuration terminally', async () => {
    const scope = new FakeScope();
    const engineFactory = vi.fn(async () => mockEngine());
    const source: ReplicaSource = {
      id: 'custom',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      start: vi.fn(async () => undefined),
      close: vi.fn(async () => undefined),
    };
    startWorker({scope, source, durableEngineFactory: engineFactory});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [],
        storage: {kind: 'memory'},
        source: builtInSource(),
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 1,
      ok: false,
      error: {
        code: 'SOURCE_CONFLICT',
        message:
          'The TinyGres worker cannot combine a fixed custom source with a built-in source configuration',
      },
    });
    expect(engineFactory).not.toHaveBeenCalled();
    expect(source.start).not.toHaveBeenCalled();
    expect(source.close).toHaveBeenCalledOnce();
  });

  it('rejects invalid built-in source config before opening storage', async () => {
    const scope = new FakeScope();
    const engineFactory = vi.fn(async () => mockEngine());
    startWorker({scope, durableEngineFactory: engineFactory});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [],
        storage: {kind: 'memory'},
        source: {...builtInSource(), url: 'not a URL'},
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 1,
      ok: false,
      error: {
        code: 'SUPABASE_INVALID_CONFIG',
        message: 'Supabase URL must be an absolute HTTP(S) URL',
      },
    });
    expect(engineFactory).not.toHaveBeenCalled();
  });

  it('rejects source schema conflicts before starting the network source', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const builtinSourceFactory = vi.fn();
    startWorker({
      scope,
      builtinSourceFactory,
      durableEngineFactory: async () => engine,
    });
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [{name: 'posts', primaryKey: ['slug']}],
        storage: {kind: 'memory'},
        source: builtInSource(),
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 1,
      ok: false,
      error: {
        code: 'SOURCE_SCHEMA_CONFLICT',
        message:
          'Built-in source table `posts` conflicts with an explicitly configured schema',
      },
    });
    expect(engine.defineTables).not.toHaveBeenCalled();
    expect(builtinSourceFactory).not.toHaveBeenCalled();
    expect(engine.close).toHaveBeenCalledOnce();
  });

  it('passes the resolved source-bound storage name to the engine factory', async () => {
    const scope = new FakeScope();
    const engineFactory = vi.fn(async () => {
      throw Object.assign(new Error('stop after resolving storage'), {
        code: 'STORAGE_OPEN_FAILED',
      });
    });
    startWorker({
      scope,
      durableEngineFactory: engineFactory,
      sourceIdentityHasher: async () => 'a'.repeat(64),
    });
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [],
        storage: {kind: 'opfs', name: 'logical-name'},
        source: builtInSource(),
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(engineFactory).toHaveBeenCalledOnce();
    expect(engineFactory).toHaveBeenCalledWith({
      kind: 'opfs',
      name: 'a'.repeat(64),
    });
  });

  it('starts an app-owned custom source even when its id is empty', async () => {
    const scope = new FakeScope();
    const source: ReplicaSource = {
      id: '',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      start: vi.fn(async () => undefined),
    };
    startWorker({
      scope,
      source,
      durableEngineFactory: async () => mockEngine(),
    });
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);

    expect(source.start).toHaveBeenCalledOnce();
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {phase: 'connecting', sourceId: 'custom-source'},
    });
    expect(scope.posted[0]).toMatchObject({
      result: {sourceConfigured: true},
    });
  });

  it('reports structured source failures without crashing the worker', async () => {
    const scope = new FakeScope();
    const source: ReplicaSource = {
      id: 'failing-source',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      async start() {
        throw Object.assign(new Error('temporarily offline'), {
          code: 'SOURCE_CONNECT_FAILED',
          retryable: true,
        });
      },
    };
    startWorker({
      scope,
      source,
      durableEngineFactory: async () => mockEngine(),
    });

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {
        phase: 'error',
        sourceId: 'failing-source',
        error: {
          code: 'SOURCE_CONNECT_FAILED',
          message: 'temporarily offline',
          retryable: true,
        },
      },
    });
  });

  it('releases engine resources before acknowledging close', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const order: string[] = [];
    vi.mocked(engine.close!).mockImplementation(() => order.push('engine'));
    vi.spyOn(scope, 'postMessage').mockImplementation((message) => {
      if ('id' in message && message.id === 2) {
        order.push('response');
      }
      scope.posted.push(message);
    });
    vi.spyOn(scope, 'close').mockImplementation(() => {
      order.push('scope');
      scope.closed = true;
    });
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [], storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'close',
      params: undefined,
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(order).toEqual(['engine', 'response', 'scope']);
  });
});
