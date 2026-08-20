import {describe, expect, it, vi} from 'vitest';

import {
  PROTOCOL_VERSION,
  isWorkerRequest,
  type ApplyOutcome,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {startWorker, type WorkerScope} from '../../src/worker/host.ts';

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
  const engine: WorkerEngine = {
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
    close: vi.fn(),
  };
  return engine;
}

async function waitForPosted(scope: FakeScope, count: number): Promise<void> {
  await vi.waitFor(() =>
    expect(scope.posted.length).toBeGreaterThanOrEqual(count),
  );
}

describe('startWorker', () => {
  it('rejects unknown configuration and batch metadata at the protocol boundary', () => {
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'init',
        params: {
          schemas: [],
          storage: {kind: 'memory'},
          integration: {kind: 'unknown'},
        },
      }),
    ).toBe(false);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 2,
        method: 'applyBatch',
        params: {batch: {changes: [], metadata: 'unknown'}},
      }),
    ).toBe(false);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 3,
        method: 'applyBatch',
        params: {batch: {changes: []}},
      }),
    ).toBe(true);
  });

  it('initializes schemas before acknowledging readiness', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.mocked(engine.defineTables).mockImplementation(() => {
      expect(scope.posted).toHaveLength(0);
    });
    startWorker({
      scope,
      durableEngineFactory: async () => engine,
    });

    scope.send({
      v: PROTOCOL_VERSION,
      id: 99,
      method: 'init',
      params: {
        schemas: [{name: 'posts', primaryKey: ['id']}],
        storage: {kind: 'opfs', name: 'schema-init'},
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);

    expect(engine.defineTables).toHaveBeenCalledWith([
      {name: 'posts', primaryKey: ['id']},
    ]);
    expect(scope.posted[0]).toMatchObject({id: 99, ok: true});
  });

  it('closes the engine when schema initialization fails', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.mocked(engine.defineTables).mockImplementation(() => {
      throw Object.assign(new Error('schema conflict'), {
        code: 'INVALID_SCHEMA',
      });
    });
    startWorker({
      scope,
      durableEngineFactory: async () => engine,
    });

    scope.send({
      v: PROTOCOL_VERSION,
      id: 100,
      method: 'init',
      params: {
        schemas: [{name: 'posts', primaryKey: ['slug']}],
        storage: {kind: 'opfs', name: 'pending-failure'},
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(engine.close).toHaveBeenCalledOnce();
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 100,
      ok: false,
      error: {code: 'INVALID_SCHEMA', message: 'schema conflict'},
    });
  });

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

  it('uses a storage-owning engine factory directly', async () => {
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
      result: {revision: 0},
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
