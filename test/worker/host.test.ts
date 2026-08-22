import {describe, expect, it, vi} from 'vitest';

import {
  PROTOCOL_VERSION,
  isWorkerRequest,
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
  const engine: WorkerEngine = {
    executeSql: vi.fn((sql) => {
      const command = sql.trim().split(/\s+/, 1)[0]!.toUpperCase();
      const table = /\busers\b/i.test(sql) ? 'users' : 'posts';
      if (transactionActive) {
        transactionTables.add(table);
      } else {
        revision += 1;
      }
      return {
        command,
        fields: [],
        revision,
        rowCount: 1,
        rows: [],
        tables: [table],
      };
    }),
    prepareSql: vi.fn(() => 1),
    executePrepared: vi.fn((_statementId, params) => {
      const table = 'posts';
      if (transactionActive) {
        transactionTables.add(table);
      } else {
        revision += 1;
      }
      return {
        command: 'UPDATE',
        fields: [],
        revision,
        rowCount: params.length,
        rows: [],
        tables: [table],
      };
    }),
    closePrepared: vi.fn(),
    execSql: vi.fn(() => {
      const table = 'posts';
      if (transactionActive) {
        transactionTables.add(table);
      } else {
        revision += 1;
      }
      return [
        {
          command: 'CREATE',
          fields: [],
          revision,
          rowCount: 0,
          rows: [],
          tables: [table],
        },
        {
          command: 'SELECT',
          fields: [{name: 'id', dataTypeID: 20}],
          revision,
          rowCount: 1,
          rows: [{id: 1}],
          tables: [],
        },
      ];
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
    revision: vi.fn(() => revision),
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
  it('rejects unknown initialization and SQL metadata at the protocol boundary', () => {
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'init',
        params: {
          storage: {kind: 'memory'},
          integration: {kind: 'unknown'},
        },
      }),
    ).toBe(false);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 2,
        method: 'executeSql',
        params: {sql: 'SELECT 1', params: [], metadata: 'unknown'},
      }),
    ).toBe(false);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 3,
        method: 'executeSql',
        params: {sql: 'SELECT 1', params: []},
      }),
    ).toBe(true);
  });

  it('checks engine readiness before acknowledging initialization', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.spyOn(engine, 'revision').mockImplementation(() => {
      expect(scope.posted).toHaveLength(0);
      return 7;
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
        storage: {kind: 'opfs', name: 'schema-init'},
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);

    expect(engine.revision).toHaveBeenCalledOnce();
    expect(scope.posted[0]).toEqual({
      v: PROTOCOL_VERSION,
      id: 99,
      ok: true,
      result: {revision: 7},
    });
  });

  it('closes the engine when its readiness check fails', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.spyOn(engine, 'revision').mockImplementation(() => {
      throw Object.assign(new Error('recovery required'), {
        code: 'RECOVERY_REQUIRED',
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
        storage: {kind: 'opfs', name: 'pending-failure'},
      },
    } satisfies WorkerRequest);
    await vi.waitFor(() => expect(scope.closed).toBe(true));

    expect(engine.close).toHaveBeenCalledOnce();
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 100,
      ok: false,
      error: {code: 'RECOVERY_REQUIRED', message: 'recovery required'},
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
      params: {storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'executeSql',
      params: {sql: 'INSERT INTO posts (id) VALUES ($1)', params: [1]},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);

    expect(engine.executeSql).toHaveBeenCalledWith(
      'INSERT INTO posts (id) VALUES ($1)',
      [1],
    );
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: {
        command: 'INSERT',
        fields: [],
        revision: 1,
        rowCount: 1,
        rows: [],
        tables: ['posts'],
      },
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
      params: {storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'executeSql',
      params: {sql: 'UPDATE posts SET id = id', params: []},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'executeSql',
      params: {sql: 'UPDATE users SET id = id', params: []},
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
      params: {storage: {kind: 'memory'}},
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
        fields: [],
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
          fields: [],
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

  it('prepares, executes, and closes session statements with transaction-aware invalidation', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'prepareSql',
      params: {sql: 'UPDATE posts SET title = $1'},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'executePrepared',
      params: {statementId: 1, params: ['outside']},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);
    expect(engine.prepareSql).toHaveBeenCalledWith(
      'UPDATE posts SET title = $1',
    );
    expect(engine.executePrepared).toHaveBeenCalledWith(1, ['outside']);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 1, tables: ['posts']},
    });

    scope.posted.length = 0;
    scope.send({
      v: PROTOCOL_VERSION,
      id: 4,
      method: 'beginTransaction',
      params: undefined,
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 5,
      method: 'executePrepared',
      params: {
        statementId: 1,
        params: ['inside'],
        transactionId: 'tx-1',
      },
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);
    await new Promise<void>((resolve) => queueMicrotask(resolve));
    expect(scope.posted.some((message) => 'event' in message)).toBe(false);

    scope.send({
      v: PROTOCOL_VERSION,
      id: 6,
      method: 'prepareSql',
      params: {sql: 'SELECT 1'},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 7,
      method: 'closePrepared',
      params: {statementId: 1},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 8,
      method: 'commitTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 6);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 6,
      ok: false,
      error: {
        code: 'TRANSACTION_ACTIVE',
        message: 'A TinyGres transaction is already active',
      },
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 7,
      ok: false,
      error: {
        code: 'TRANSACTION_ACTIVE',
        message: 'A TinyGres transaction is already active',
      },
    });
    expect(engine.closePrepared).not.toHaveBeenCalled();
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 2, tables: ['posts']},
    });
    scope.send({
      v: PROTOCOL_VERSION,
      id: 9,
      method: 'closePrepared',
      params: {statementId: 1},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 7);
    expect(engine.closePrepared).toHaveBeenCalledWith(1);
  });

  it('returns every exec result and emits one combined invalidation', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.posted.length = 0;

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'execSql',
      params: {sql: 'CREATE TABLE posts; SELECT id FROM posts'},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);

    expect(engine.execSql).toHaveBeenCalledWith(
      'CREATE TABLE posts; SELECT id FROM posts',
    );
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: [
        {
          command: 'CREATE',
          fields: [],
          revision: 1,
          rowCount: 0,
          rows: [],
          tables: ['posts'],
        },
        {
          command: 'SELECT',
          fields: [{name: 'id', dataTypeID: 20}],
          revision: 1,
          rowCount: 1,
          rows: [{id: 1}],
          tables: [],
        },
      ],
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 1, tables: ['posts']},
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
      params: {storage: {kind: 'memory'}},
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

  it('keeps the transaction token active when rollback fails so cleanup can retry', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.mocked(engine.rollbackTransaction).mockImplementationOnce(() => {
      throw Object.assign(new Error('storage rollback failed'), {
        code: 'ROLLBACK_FAILED',
      });
    });
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
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
      method: 'rollbackTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 4,
      method: 'rollbackTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 4);

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 3,
      ok: false,
      error: {
        code: 'ROLLBACK_FAILED',
        message: 'storage rollback failed',
      },
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 4,
      ok: true,
      result: undefined,
    });
    expect(engine.rollbackTransaction).toHaveBeenCalledTimes(2);
  });

  it('keeps the transaction token active when commit cleanup fails', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    vi.mocked(engine.commitTransaction).mockImplementationOnce(() => {
      throw Object.assign(new Error('commit failed'), {code: 'COMMIT_FAILED'});
    });
    vi.mocked(engine.rollbackTransaction).mockImplementationOnce(() => {
      throw Object.assign(new Error('cleanup failed'), {code: 'CLEANUP_FAILED'});
    });
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
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
      method: 'commitTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 4,
      method: 'rollbackTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 4);

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 3,
      ok: false,
      error: {code: 'COMMIT_FAILED', message: 'commit failed'},
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 4,
      ok: true,
      result: undefined,
    });
    expect(engine.rollbackTransaction).toHaveBeenCalledTimes(2);
  });

  it('rejects malformed messages without invoking the engine', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({v: 99, id: 8, method: 'removedMethod', params: {}});
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
    expect(engine.executeSql).not.toHaveBeenCalled();
  });

  it('rejects well-shaped methods with unsafe parameters', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 9,
      method: 'executeSql',
      params: {sql: 'SELECT $1', params: [1n]},
    });
    await waitForPosted(scope, 1);

    expect(scope.posted[0]).toMatchObject({
      id: 9,
      ok: false,
      error: {code: 'PROTOCOL_MISMATCH'},
    });
    expect(engine.executeSql).not.toHaveBeenCalled();
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
    expect(engine.revision).toHaveBeenCalledOnce();
  });

  it('releases the existing engine when a second init changes storage', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});
    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'init',
      params: {
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
      params: {storage: {kind: 'memory'}},
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
