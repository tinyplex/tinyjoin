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
import {
  startWorker,
  type WorkerScope,
} from '../../src/worker/host.ts';
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
  await vi.waitFor(() => expect(scope.posted.length).toBeGreaterThanOrEqual(count));
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
        message: 'The TinyGres worker is already initialized with different storage',
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
        await context.replaceTable(
          {name: 'posts', primaryKey: ['id']},
          [{id: 1, title: 'from source'}],
        );
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
