import {describe, expect, it, vi} from 'vitest';

import type {ReplicaSource} from '../../src/adapters/types.ts';
import {
  PROTOCOL_VERSION,
  type ApplyOutcome,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
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
  const outcome = (table: string): ApplyOutcome => ({
    revision: ++revision,
    tables: [table],
  });
  const engine: WorkerEngine = {
    defineTable: vi.fn(),
    defineTables: vi.fn(),
    replaceTableSnapshot: vi.fn((schema) => outcome(schema.name)),
    applyBatch: vi.fn((batch) => outcome(batch.changes[0]?.table ?? 'none')),
    query: vi.fn(() => ({revision, rows: [{id: 1}]})),
    querySql: vi.fn(() => ({revision, rows: [{id: 1}]})),
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
    startWorker({scope, engineFactory: async () => engine});

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
    startWorker({scope, engineFactory: async () => engine});

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

  it('rejects malformed messages without invoking the engine', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, engineFactory: async () => engine});

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
    startWorker({scope, engineFactory: async () => engine});

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
    const engine = mockEngine();
    startWorker({scope, engineFactory: async () => engine});

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
    expect(engine.defineTables).not.toHaveBeenCalled();
    expect(engine.close).toHaveBeenCalledOnce();
  });

  it('makes a failed initialization terminal and releases its engine', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.mocked(engine.defineTables).mockImplementation(() => {
      throw Object.assign(new Error('schema conflict'), {
        code: 'INVALID_SCHEMA',
      });
    });
    startWorker({scope, engineFactory: async () => engine});

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
    startWorker({scope, engineFactory: async () => engine});
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
    startWorker({scope, engineFactory: async () => engine});
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
    startWorker({scope, source, engineFactory: async () => engine});

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
      engineFactory: async () => engine,
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
      engineFactory: async () => engine,
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
      engineFactory: async () => engine,
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
    startWorker({scope, source, engineFactory});
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
    startWorker({scope, engineFactory});
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
      engineFactory: async () => engine,
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
    startWorker({scope, source, engineFactory: async () => mockEngine()});
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
      engineFactory: async () => mockEngine(),
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
    startWorker({scope, engineFactory: async () => engine});

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
