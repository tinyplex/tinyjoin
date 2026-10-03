import {describe, expect, it, vi} from 'vitest';

import {
  PROTOCOL_VERSION,
  isWorkerRequest,
  type JsonValue,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {startWorker, type WorkerScope} from '../../src/worker/host.ts';
import {
  WASM_OPERATION,
  adaptStructuredWasmEngine,
} from '../../src/worker/wasm-bridge.ts';

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
        revision,
        rowCount: 1,
        tables: [table],
        keys: {},
        data: sqlData([], []),
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
        revision,
        rowCount: params.length,
        tables: [table],
        keys: {},
        data: sqlData([], []),
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
          revision,
          rowCount: 0,
          tables: [table],
          keys: {},
          data: sqlData([], []),
        },
        {
          command: 'SELECT',
          revision,
          rowCount: 1,
          tables: [],
          keys: {},
          data: sqlData([{name: 'id', dataTypeID: 20}], [{id: 1}]),
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
      return {revision, tables, keys: {}};
    }),
    rollbackTransaction: vi.fn(() => {
      transactionActive = false;
      transactionTables.clear();
    }),
    inTransaction: vi.fn(() => transactionActive),
    revision: vi.fn(() => revision),
    check: vi.fn(),
    schema: vi.fn(() => ({version: 0, tables: []})),
    setSchema: vi.fn(() => ({revision: 0, tables: [], keys: {}})),
    close: vi.fn(),
  };
  return engine;
}

function sqlData(fields: unknown[], rows: unknown[]): string {
  return JSON.stringify({fields, rows});
}

async function waitForPosted(scope: FakeScope, count: number): Promise<void> {
  await vi.waitFor(() =>
    expect(scope.posted.length).toBeGreaterThanOrEqual(count),
  );
}

describe('startWorker', () => {
  it('rejects graph expansion in preflight and keeps serving requests', async () => {
    const scope = new FakeScope();
    const result = {
      command: 'SELECT',
      revision: 0,
      rowCount: 1,
      tables: [],
      keys: {},
      data: sqlData([{name: 'id', dataTypeID: 20}], [{id: 1}]),
    };
    const callStructured = vi.fn((_version: number, operation: number) => {
      if (operation === WASM_OPERATION.executeSql) {
        const {data, ...header} = result;
        return `${JSON.stringify([5, 0, 0, header])}\n${data}`;
      }
      return JSON.stringify([
        5,
        0,
        0,
        operation === WASM_OPERATION.revision ? 0 : null,
      ]);
    });
    const engine = adaptStructuredWasmEngine({callStructured});
    const controller = startWorker({
      scope,
      durableEngineFactory: async () => engine,
    });
    let shared: JsonValue = null;
    for (let depth = 0; depth < 40; depth += 1) {
      shared = [shared, shared];
    }

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    scope.send(structuredClone({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'executeSql',
      params: {sql: 'SELECT id FROM posts WHERE id = $1', params: [shared]},
    } satisfies WorkerRequest));
    scope.send(structuredClone({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'executePrepared',
      params: {statementId: 1, params: [shared]},
    } satisfies WorkerRequest));
    scope.send({
      v: PROTOCOL_VERSION,
      id: 4,
      method: 'executeSql',
      params: {sql: 'SELECT id FROM posts', params: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 4);

    for (const index of [1, 2]) {
      expect(scope.posted[index]).toMatchObject({
        id: index + 1,
        ok: false,
        error: {code: 'RESOURCE_LIMIT'},
      });
    }
    // A result that published nothing passes on as the text WASM wrote.
    const {data, ...header} = result;
    expect(scope.posted[3]).toEqual({
      v: PROTOCOL_VERSION,
      id: 4,
      ok: true,
      result: [JSON.stringify(header), data],
    });
    expect(callStructured.mock.calls.map(([, operation]) => operation)).toEqual([
      WASM_OPERATION.revision,
      WASM_OPERATION.executeSql,
    ]);
    expect(scope.closed).toBe(false);
    await controller.close();
  });

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
      undefined,
    );
    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'executeSql',
      params: {sql: 'SELECT id FROM posts', params: [], rowMode: 'array'},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);
    expect(engine.executeSql).toHaveBeenLastCalledWith(
      'SELECT id FROM posts',
      [],
      'array',
    );
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: {
        command: 'INSERT',
        revision: 1,
        rowCount: 1,
        tables: ['posts'],
        keys: {},
        data: sqlData([], []),
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
        payload: {revision: 2, tables: ['posts', 'users'], keys: {}},
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
        revision: 1,
        rowCount: 1,
        tables: ['posts'],
        keys: {},
        data: sqlData([], []),
      },
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 1, tables: ['posts'], keys: {}},
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
          tables: ['posts'],
          keys: {},
          data: sqlData([], []),
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
      result: {revision: 2, tables: ['posts'], keys: {}},
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 2, tables: ['posts'], keys: {}},
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
    expect(engine.executePrepared).toHaveBeenCalledWith(
      1,
      ['outside'],
      undefined,
    );
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 1, tables: ['posts'], keys: {}},
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
        message: 'A TinyJoin transaction is already active',
      },
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 7,
      ok: false,
      error: {
        code: 'TRANSACTION_ACTIVE',
        message: 'A TinyJoin transaction is already active',
      },
    });
    expect(engine.closePrepared).not.toHaveBeenCalled();
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 2, tables: ['posts'], keys: {}},
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
      undefined,
    );
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: [
        {
          command: 'CREATE',
          revision: 1,
          rowCount: 0,
          tables: ['posts'],
          keys: {},
          data: sqlData([], []),
        },
        {
          command: 'SELECT',
          revision: 1,
          rowCount: 1,
          tables: [],
          keys: {},
          data: sqlData([{name: 'id', dataTypeID: 20}], [{id: 1}]),
        },
      ],
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 1, tables: ['posts'], keys: {}},
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
        message: 'Use the active TinyJoin transaction for this operation',
      },
    });
    expect(engine.executeSql).not.toHaveBeenCalled();
    expect(engine.rollbackTransaction).toHaveBeenCalledOnce();
    expect(scope.posted.some((message) => 'event' in message)).toBe(false);
  });

  it('checks the database only outside a transaction', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, durableEngineFactory: async () => engine});
    for (const [id, method] of [
      [1, 'init'],
      [2, 'check'],
      [3, 'beginTransaction'],
      [4, 'check'],
    ] as const) {
      scope.send({
        v: PROTOCOL_VERSION,
        id,
        method,
        params: method === 'init' ? {storage: {kind: 'memory'}} : undefined,
      } as WorkerRequest);
    }
    await waitForPosted(scope, 4);

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: undefined,
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 4,
      ok: false,
      error: {
        code: 'TRANSACTION_ACTIVE',
        message: 'A TinyJoin transaction is already active',
      },
    });
    expect(engine.check).toHaveBeenCalledOnce();
  });

  it('reads the schema, even with a transaction active', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const schema = {
      version: 0,
      tables: [
        {
          name: 'posts',
          columns: [{name: 'id', type: 'integer' as const, nullable: false}],
          primaryKey: ['id'],
          indexes: [],
        },
      ],
    };
    vi.mocked(engine.schema).mockReturnValue(schema);
    startWorker({scope, durableEngineFactory: async () => engine});
    for (const [id, method] of [
      [1, 'init'],
      [2, 'schema'],
      [3, 'beginTransaction'],
      [4, 'schema'],
    ] as const) {
      scope.send({
        v: PROTOCOL_VERSION,
        id,
        method,
        params: method === 'init' ? {storage: {kind: 'memory'}} : undefined,
      } as WorkerRequest);
    }
    await waitForPosted(scope, 4);

    for (const id of [2, 4]) {
      expect(scope.posted).toContainEqual({
        v: PROTOCOL_VERSION,
        id,
        ok: true,
        result: schema,
      });
    }
    expect(engine.schema).toHaveBeenCalledTimes(2);
  });

  it('sets the schema outside a transaction and announces the tables it changed', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const schema = {version: 1, tables: []};
    vi.mocked(engine.setSchema)
      .mockReturnValueOnce({revision: 1, tables: ['tasks'], keys: {}})
      .mockReturnValueOnce({revision: 0, tables: [], keys: {}});
    startWorker({scope, durableEngineFactory: async () => engine});
    const requests = [
      {id: 1, method: 'init', params: {storage: {kind: 'memory'}}},
      {id: 2, method: 'setSchema', params: {schema, drop: true}},
      {id: 3, method: 'setSchema', params: {schema, drop: false}},
      {id: 4, method: 'beginTransaction', params: undefined},
      {id: 5, method: 'setSchema', params: {schema, drop: false}},
    ];
    for (const request of requests) {
      scope.send({v: PROTOCOL_VERSION, ...request} as WorkerRequest);
    }
    await waitForPosted(scope, 6);

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: true,
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 3,
      ok: true,
      result: false,
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 5,
      ok: false,
      error: {
        code: 'TRANSACTION_ACTIVE',
        message: 'A TinyJoin transaction is already active',
      },
    });
    expect(vi.mocked(engine.setSchema).mock.calls).toEqual([
      [schema, true],
      [schema, false],
    ]);
    expect(
      scope.posted.filter(
        (message): message is WorkerEvent => 'event' in message,
      ),
    ).toEqual([
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables: ['tasks'], keys: {}},
      },
    ]);
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
        message: 'The worker received an invalid TinyJoin protocol request',
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
          'The TinyJoin worker is already initialized with different storage',
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

  it('serves direct requests in order, without messages', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    let open!: (engine: WorkerEngine) => void;
    const host = startWorker({
      scope,
      durableEngineFactory: () =>
        new Promise((resolve) => {
          open = resolve;
        }),
    });
    const update = (sql: string) =>
      host.request({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'executeSql',
        params: {sql, params: []},
      });

    const initialized = host.request({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
    });
    const queued = update('UPDATE posts SET title = 1');
    await Promise.resolve();
    expect(engine.executeSql).not.toHaveBeenCalled();
    // Nothing can be served at once while init waits for the engine.
    const now = (sql: string) =>
      host.requestNow({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'executeSql',
        params: {sql, params: []},
      });
    expect(now('SELECT 1')).toBeUndefined();
    open(engine);
    await expect(initialized).resolves.toEqual({revision: 0});
    await expect(queued).resolves.toMatchObject({revision: 1});

    // With the engine open and nothing ahead of it, a request is served at once.
    const immediate = update('UPDATE posts SET title = 2');
    expect(engine.executeSql).toHaveBeenCalledTimes(2);
    await expect(immediate).resolves.toMatchObject({revision: 2});
    expect(now('UPDATE posts SET title = 3')).toMatchObject({
      ok: true,
      value: {revision: 3},
    });
    expect(
      host.requestNow({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'rollbackTransaction',
        params: {transactionId: 'tx-9'},
      }),
    ).toMatchObject({ok: false, error: {code: 'TRANSACTION_NOT_ACTIVE'}});
    await expect(
      host.request({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'commitTransaction',
        params: {transactionId: 'tx-9'},
      }),
    ).rejects.toMatchObject({
      code: 'TRANSACTION_NOT_ACTIVE',
      retryable: false,
    });
    expect(scope.posted.filter((message) => 'id' in message)).toEqual([]);
  });
});
