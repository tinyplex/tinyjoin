import {describe, expect, it, vi} from 'vitest';

import {
  PROTOCOL_VERSION,
  STATEMENT_COMMANDS,
  STATEMENT_PARAMS,
  STATEMENT_PREPARED,
  STATEMENT_SQL,
  isStatementResponse,
  isWorkerRequest,
  type JsonValue,
  type RowMode,
  type SqlResult,
  type StatementRequest,
  type StatementResponse,
  type StatementResult,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {
  startWorker,
  statementRequest,
  type WorkerScope,
} from '../../src/worker/host.ts';
import {
  WASM_OPERATION,
  adaptStructuredWasmEngine,
} from '../../src/worker/wasm-bridge.ts';

type Posted = WorkerResponse | StatementResponse | WorkerEvent;

class FakeScope implements WorkerScope {
  readonly posted: Posted[] = [];
  closed = false;
  readonly #listeners = new Set<(event: MessageEvent<unknown>) => void>();

  postMessage(message: Posted): void {
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
    // The raw engine answers in the version the bridge called it with, which
    // is the bridge's own tests' to pin.
    const callStructured = vi.fn((version: number, operation: number) => {
      if (operation === WASM_OPERATION.executeSql) {
        const {data, ...header} = result;
        return `${JSON.stringify([version, 0, 0, header])}\n${data}`;
      }
      return JSON.stringify([
        version,
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
    // A raw engine with no memory to read a flat result from answers in JSON,
    // which the bridge reads into a result, and which a response then carries.
    expect(scope.posted[3]).toEqual({
      v: PROTOCOL_VERSION,
      id: 4,
      ok: true,
      result,
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

  it('answers a mutation before it reads the keys its event will list', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    // What happened, in order: a key read, which merging does to tell it from
    // the keys before it, and a message posted.
    const log: string[] = [];
    const key = (id: number) => ({
      get id() {
        log.push(`read ${id}`);
        return id;
      },
    });
    // Two statements that list the keys they changed, one key in both, and of
    // which the second also changed a table too widely to list its keys.
    const outcomes = [
      {tables: ['posts'], keys: {posts: [key(1), key(2)]}},
      {tables: ['posts', 'users'], keys: {posts: [key(2), key(3)]}},
    ];
    let revision = 0;
    engine.executeSql = vi.fn(() => ({
      command: 'UPDATE',
      rowCount: 2,
      data: sqlData([], []),
      ...outcomes[revision]!,
      revision: ++revision,
    }));
    startWorker({scope, durableEngineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {storage: {kind: 'memory'}},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 1);
    scope.posted.length = 0;
    const post = scope.postMessage.bind(scope);
    scope.postMessage = (message) => {
      log.push(
        'event' in message
          ? 'event'
          : `answer ${Array.isArray(message) ? message[1] : message.id}`,
      );
      post(message);
    };

    for (const id of [2, 3]) {
      scope.send({
        v: PROTOCOL_VERSION,
        id,
        method: 'executeSql',
        params: {sql: 'UPDATE posts SET id = id', params: []},
      } satisfies WorkerRequest);
    }
    await waitForPosted(scope, 3);

    // Both were answered before a key was read, and the keys were merged, each
    // once, as the event was made.
    expect(log.slice(0, 2)).toEqual(['answer 2', 'answer 3']);
    expect(log.at(-1)).toBe('event');
    expect(log.filter((entry) => entry.startsWith('read')).length).toBe(4);
    const event = scope.posted.find(
      (message): message is WorkerEvent => 'event' in message,
    )!;
    log.length = 0;
    expect(JSON.parse(JSON.stringify(event))).toEqual({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {
        revision: 2,
        tables: ['posts', 'users'],
        keys: {posts: [{id: 1}, {id: 2}, {id: 3}]},
      },
    });
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
    // The commit answers with its revision alone, and before the event that
    // carries what it changed to every subscriber.
    expect(scope.posted.slice(1)).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 5,
        ok: true,
        result: {revision: 2},
      },
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 2, tables: ['posts'], keys: {}},
      },
    ]);
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
          foreignKeys: [],
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

// Statement requests: a statement sent as one flat array, and a result that
// published nothing answered with one.

const SELECT = 'SELECT id FROM posts WHERE id = $1';
const UPDATE = 'UPDATE posts SET title = $1 WHERE id = $2';
const RETURNING = 'INSERT INTO posts (id) VALUES ($1) RETURNING id';
const FAIL = 'FAIL $1';

const statement = (
  id: number,
  sql: string,
  params: JsonValue[] = [],
  transaction: string | 0 = 0,
  arrayRows: 0 | 1 = 0,
): StatementRequest => [
  PROTOCOL_VERSION,
  id,
  STATEMENT_SQL,
  sql,
  transaction,
  arrayRows,
  ...params,
];

const execution = (
  id: number,
  statementId: number,
  params: JsonValue[] = [],
  transaction: string | 0 = 0,
  arrayRows: 0 | 1 = 0,
): StatementRequest => [
  PROTOCOL_VERSION,
  id,
  STATEMENT_PREPARED,
  statementId,
  transaction,
  arrayRows,
  ...params,
];

const initRequest = (id = 1): WorkerRequest => ({
  v: PROTOCOL_VERSION,
  id,
  method: 'init',
  params: {storage: {kind: 'memory'}},
});

const idOf = (message: Posted): number | undefined =>
  Array.isArray(message) ? message[1] : 'id' in message ? message.id : undefined;

const answers = ({posted}: {posted: Posted[]}): Posted[] =>
  posted.filter((message) => idOf(message) !== undefined);

const events = ({posted}: {posted: Posted[]}): Posted[] =>
  posted.filter((message) => idOf(message) === undefined);

type EngineCall = {
  target: string | number;
  params: JsonValue[];
  rowMode: RowMode | undefined;
};

/**
 * An engine that answers as the bridge does: a statement whose result
 * published nothing with the flat array of that result, its first two slots
 * left for whoever posts it, and any other with a result object. It reads a
 * statement's parameters from where the call says they begin, and records
 * them as they would be had they been sliced out.
 */
function flatEngine() {
  let revision = 0;
  let transactionActive = false;
  let staged = false;
  const calls: EngineCall[] = [];
  const prepared: string[] = [];
  const run = (
    sql: string,
    target: string | number,
    all: readonly JsonValue[],
    rowMode: RowMode | undefined,
    from = 0,
  ): SqlResult | StatementResult => {
    const params = all.slice(from);
    calls.push({target, params, rowMode});
    const command = sql.split(' ', 1)[0]!;
    if (command === 'FAIL') {
      throw Object.assign(new Error(`The engine refused ${String(params[0])}`), {
        code: String(params[0]),
        ...(params[1] === true ? {retryable: true} : {}),
      });
    }
    if (command === 'BREAK') {
      throw new Error('The engine broke');
    }
    const key = params.find((value) => typeof value === 'number') ?? null;
    if (command === 'SELECT') {
      return [
        0,
        0,
        STATEMENT_COMMANDS.indexOf('SELECT'),
        revision,
        1,
        sqlData(
          [{name: 'id', dataTypeID: 20}],
          [rowMode === 'array' ? [key] : {id: key}],
        ),
      ];
    }
    const returning = sql.includes('RETURNING');
    if (transactionActive && !returning) {
      staged = true;
      return [
        0,
        0,
        STATEMENT_COMMANDS.indexOf(command as 'INSERT'),
        revision,
        1,
        'posts',
        1,
        'id',
        key,
      ];
    }
    if (transactionActive) {
      staged = true;
    } else {
      revision += 1;
    }
    return {
      command,
      revision,
      rowCount: 1,
      tables: ['posts'],
      keys: {posts: [{id: key}]},
      data: returning
        ? sqlData(
            [{name: 'id', dataTypeID: 20}],
            [rowMode === 'array' ? [key] : {id: key}],
          )
        : sqlData([], []),
    };
  };
  const engine: WorkerEngine = {
    executeSql: vi.fn((sql, params, rowMode, from) =>
      run(sql, sql, params, rowMode, from),
    ),
    prepareSql: vi.fn((sql) => prepared.push(sql)),
    executePrepared: vi.fn((statementId, params, rowMode, from) => {
      const sql = prepared[statementId - 1];
      if (sql === undefined) {
        throw Object.assign(new Error('The statement is not prepared'), {
          code: 'PREPARED_STATEMENT_NOT_FOUND',
        });
      }
      return run(sql, statementId, params, rowMode, from);
    }),
    closePrepared: vi.fn(),
    execSql: vi.fn(() => []),
    beginTransaction: vi.fn(() => {
      transactionActive = true;
      staged = false;
    }),
    commitTransaction: vi.fn(() => {
      transactionActive = false;
      if (staged) {
        revision += 1;
      }
      return {revision, tables: staged ? ['posts'] : [], keys: {}};
    }),
    rollbackTransaction: vi.fn(() => {
      transactionActive = false;
    }),
    inTransaction: vi.fn(() => transactionActive),
    revision: vi.fn(() => revision),
    check: vi.fn(),
    schema: vi.fn(() => ({version: 0, tables: []})),
    setSchema: vi.fn(() => ({revision, tables: [], keys: {}})),
    close: vi.fn(),
  };
  return {engine, calls};
}

async function openHost(engine: WorkerEngine) {
  const scope = new FakeScope();
  const host = startWorker({scope, durableEngineFactory: async () => engine});
  scope.send(initRequest());
  await waitForPosted(scope, 1);
  scope.posted.length = 0;
  return {scope, host};
}

const nextTask = (): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, 0));

describe('statement requests', () => {
  it('stand for the requests a client would otherwise send, key for key', () => {
    const sql = 'SELECT $1';
    const cases: [StatementRequest, WorkerRequest['params']][] = [
      [statement(1, sql, [1]), {sql, params: [1]}],
      [statement(1, sql, [1], 0, 1), {sql, params: [1], rowMode: 'array'}],
      [statement(1, sql, [], 'tx-1'), {sql, params: [], transactionId: 'tx-1'}],
      [
        statement(1, sql, [null, 'a'], 'tx-1', 1),
        {sql, params: [null, 'a'], transactionId: 'tx-1', rowMode: 'array'},
      ],
      [execution(1, 3, [1]), {statementId: 3, params: [1]}],
      [execution(1, 3, [], 0, 1), {statementId: 3, params: [], rowMode: 'array'}],
      [
        execution(1, 3, [{a: [1]}], 'tx-1'),
        {statementId: 3, params: [{a: [1]}], transactionId: 'tx-1'},
      ],
      [
        execution(1, 3, [true], 'tx-1', 1),
        {statementId: 3, params: [true], transactionId: 'tx-1', rowMode: 'array'},
      ],
    ];
    for (const [request, params] of cases) {
      const stoodFor = statementRequest(request);
      expect(stoodFor).toEqual({
        v: PROTOCOL_VERSION,
        id: 1,
        method: request[2] === STATEMENT_PREPARED ? 'executePrepared' : 'executeSql',
        params,
      });
      // The keys come in the order a client writes them, and no others.
      expect(Object.keys(stoodFor)).toEqual(['v', 'id', 'method', 'params']);
      expect(Object.keys(stoodFor.params!)).toEqual(Object.keys(params!));
      expect(isWorkerRequest(stoodFor)).toBe(true);
    }
  });

  it('are served at once, with their parameters read where they arrived', async () => {
    const {engine} = flatEngine();
    const {scope} = await openHost(engine);

    // Nothing waits, so the answer is posted before the message returns: the
    // flat result itself, with the version and the request's id filled in.
    const select = statement(2, SELECT, [7]);
    scope.send(select);
    expect(scope.posted).toEqual([
      [
        PROTOCOL_VERSION,
        2,
        3,
        0,
        1,
        sqlData([{name: 'id', dataTypeID: 20}], [{id: 7}]),
      ],
    ]);
    expect(isStatementResponse(scope.posted[0] as unknown[], true)).toBe(true);
    // The engine is given the request itself, and where its parameters begin.
    expect(engine.executeSql).toHaveBeenCalledExactlyOnceWith(
      SELECT,
      select,
      undefined,
      STATEMENT_PARAMS,
    );
    expect(vi.mocked(engine.executeSql).mock.calls[0]![1]).toBe(select);

    scope.send({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'prepareSql',
      params: {sql: SELECT},
    } satisfies WorkerRequest);
    const prepared = execution(4, 1, [9], 0, 1);
    scope.send(prepared);
    expect(scope.posted.at(-1)).toEqual([
      PROTOCOL_VERSION,
      4,
      3,
      0,
      1,
      sqlData([{name: 'id', dataTypeID: 20}], [[9]]),
    ]);
    expect(engine.executePrepared).toHaveBeenCalledExactlyOnceWith(
      1,
      prepared,
      'array',
      STATEMENT_PARAMS,
    );
    expect(vi.mocked(engine.executePrepared).mock.calls[0]![1]).toBe(prepared);

    // A flat result published nothing, and announces nothing.
    await nextTask();
    expect(events(scope)).toEqual([]);
  });

  it('answer a result that is not flat in a response, and announce what it published', async () => {
    const {engine} = flatEngine();
    const {scope} = await openHost(engine);

    scope.send(statement(2, UPDATE, ['changed', 5]));
    const result = {
      command: 'UPDATE',
      revision: 1,
      rowCount: 1,
      tables: ['posts'],
      keys: {posts: [{id: 5}]},
      data: sqlData([], []),
    };
    expect(scope.posted).toEqual([
      {v: PROTOCOL_VERSION, id: 2, ok: true, result},
    ]);
    await nextTask();
    expect(events(scope)).toEqual([
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables: ['posts'], keys: {posts: [{id: 5}]}},
      },
    ]);
  });

  it('announce a transaction only when it commits, and commit with the revision alone', async () => {
    const {engine} = flatEngine();
    const {scope} = await openHost(engine);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'beginTransaction',
      params: undefined,
    } satisfies WorkerRequest);
    scope.send(statement(3, UPDATE, ['staged', 5], 'tx-1'));
    // Rows a transaction's statement returns do not make it a publication.
    scope.send(statement(4, RETURNING, [6], 'tx-1'));
    expect(answers(scope).slice(1)).toEqual([
      [PROTOCOL_VERSION, 3, 1, 0, 1, 'posts', 1, 'id', 5],
      {
        v: PROTOCOL_VERSION,
        id: 4,
        ok: true,
        result: {
          command: 'INSERT',
          revision: 0,
          rowCount: 1,
          tables: ['posts'],
          keys: {posts: [{id: 6}]},
          data: sqlData([{name: 'id', dataTypeID: 20}], [{id: 6}]),
        },
      },
    ]);
    await nextTask();
    expect(events(scope)).toEqual([]);

    scope.posted.length = 0;
    scope.send({
      v: PROTOCOL_VERSION,
      id: 5,
      method: 'commitTransaction',
      params: {transactionId: 'tx-1'},
    } satisfies WorkerRequest);
    await nextTask();
    expect(scope.posted).toEqual([
      {v: PROTOCOL_VERSION, id: 5, ok: true, result: {revision: 1}},
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables: ['posts'], keys: {}},
      },
    ]);
  });

  // The statements of one session, in each of the three ways they can reach
  // the engine: as flat arrays served at once, as flat arrays that must wait
  // their turn, and as the requests they stand for.
  const session = (): (StatementRequest | WorkerRequest)[] => [
    statement(2, SELECT, [7]),
    statement(3, SELECT, [8], 0, 1),
    statement(4, UPDATE, ['outside', 5]),
    statement(5, SELECT, [1], 'tx-1'),
    {v: PROTOCOL_VERSION, id: 6, method: 'prepareSql', params: {sql: UPDATE}},
    execution(7, 1, ['prepared', 9]),
    execution(8, 2, []),
    {v: PROTOCOL_VERSION, id: 9, method: 'beginTransaction', params: undefined},
    statement(10, UPDATE, ['staged', 5], 'tx-1'),
    execution(11, 1, ['staged', 6], 'tx-1', 1),
    statement(12, RETURNING, [7], 'tx-1', 1),
    statement(13, SELECT, [5]),
    statement(14, SELECT, [5], 'tx-2'),
    statement(15, FAIL, ['CONSTRAINT_VIOLATION', true], 'tx-1'),
    statement(16, 'BREAK', [], 'tx-1'),
    statement(17, SELECT, [{nested: [1, 'two']}, 4], 'tx-1'),
    {
      v: PROTOCOL_VERSION,
      id: 18,
      method: 'commitTransaction',
      params: {transactionId: 'tx-1'},
    },
    statement(19, 'DELETE FROM posts WHERE id = $1', [5]),
    statement(20, SELECT, [5], 'tx-1'),
  ];

  it('get the same answers served at once, in turn, and as the requests they stand for', async () => {
    const run = async (how: 'at once' | 'in turn' | 'as requests') => {
      const {engine, calls} = flatEngine();
      const scope = new FakeScope();
      startWorker({scope, durableEngineFactory: async () => engine});
      scope.send(initRequest());
      if (how !== 'in turn') {
        await waitForPosted(scope, 1);
      }
      for (const message of session()) {
        const before = scope.posted.length;
        scope.send(
          how === 'as requests' && Array.isArray(message)
            ? statementRequest(message)
            : message,
        );
        // With nothing ahead of it, every request is answered as it arrives;
        // behind the engine's opening, none is.
        expect(scope.posted.length).toBe(how === 'in turn' ? 0 : before + 1);
      }
      await waitForPosted(scope, session().length + 1);
      await nextTask();
      return {posted: scope.posted, calls};
    };
    const atOnce = await run('at once');
    const inTurn = await run('in turn');
    const asRequests = await run('as requests');

    expect(inTurn.posted).toEqual(atOnce.posted);
    expect(asRequests.posted).toEqual(atOnce.posted);
    expect(inTurn.calls).toEqual(atOnce.calls);
    expect(asRequests.calls).toEqual(atOnce.calls);

    // The answers are in the order of the requests, each of its own kind.
    expect(answers(atOnce).map(idOf)).toEqual(
      Array.from({length: 20}, (_, index) => index + 1),
    );
    const byId = new Map(atOnce.posted.map((message) => [idOf(message), message]));
    expect(byId.get(2)).toEqual([PROTOCOL_VERSION, 2, 3, 0, 1, expect.any(String)]);
    expect(byId.get(4)).toMatchObject({ok: true, result: {revision: 1}});
    expect(byId.get(5)).toMatchObject({
      ok: false,
      error: {code: 'TRANSACTION_NOT_ACTIVE'},
    });
    expect(byId.get(7)).toMatchObject({ok: true, result: {revision: 2}});
    expect(byId.get(8)).toMatchObject({
      ok: false,
      error: {code: 'PREPARED_STATEMENT_NOT_FOUND'},
    });
    expect(byId.get(10)).toEqual([PROTOCOL_VERSION, 10, 1, 2, 1, 'posts', 1, 'id', 5]);
    expect(byId.get(11)).toEqual([PROTOCOL_VERSION, 11, 1, 2, 1, 'posts', 1, 'id', 6]);
    expect(byId.get(12)).toMatchObject({
      ok: true,
      result: {command: 'INSERT', data: sqlData([{name: 'id', dataTypeID: 20}], [[7]])},
    });
    for (const id of [13, 14]) {
      expect(byId.get(id)).toMatchObject({
        ok: false,
        error: {
          code: 'TRANSACTION_ACTIVE',
          message: 'Use the active TinyJoin transaction for this operation',
        },
      });
    }
    expect(byId.get(15)).toEqual({
      v: PROTOCOL_VERSION,
      id: 15,
      ok: false,
      error: {
        code: 'CONSTRAINT_VIOLATION',
        message: 'The engine refused CONSTRAINT_VIOLATION',
        retryable: true,
      },
    });
    expect(byId.get(16)).toEqual({
      v: PROTOCOL_VERSION,
      id: 16,
      ok: false,
      error: {code: 'WORKER_OPERATION_FAILED', message: 'The engine broke'},
    });
    expect(byId.get(17)).toEqual([PROTOCOL_VERSION, 17, 3, 2, 1, expect.any(String)]);
    expect(byId.get(18)).toEqual({
      v: PROTOCOL_VERSION,
      id: 18,
      ok: true,
      result: {revision: 3},
    });
    expect(byId.get(19)).toMatchObject({ok: true, result: {command: 'DELETE', revision: 4}});
    expect(byId.get(20)).toEqual({
      v: PROTOCOL_VERSION,
      id: 20,
      ok: false,
      error: {
        code: 'TRANSACTION_NOT_ACTIVE',
        message: 'The TinyJoin transaction is no longer active',
      },
    });
    // Parameters reach the engine alone, whichever way the request came.
    expect(atOnce.calls).toContainEqual({
      target: 1,
      params: ['staged', 6],
      rowMode: 'array',
    });
    expect(atOnce.calls).toContainEqual({
      target: SELECT,
      params: [{nested: [1, 'two']}, 4],
      rowMode: undefined,
    });
  });

  it('get the same answers in each of those ways for sessions made at random', async () => {
    const random = (seed: number) => () => {
      seed = (seed * 1_103_515_245 + 12_345) % 2_147_483_648;
      return seed / 2_147_483_648;
    };
    // A session of statements and the requests around them. It follows the
    // transaction it expects to be open, so that most statements name the
    // right one, and some name none, a finished one, or one never begun.
    const randomSession = (
      next: () => number,
    ): (StatementRequest | WorkerRequest)[] => {
      const pick = <Item>(items: readonly Item[]): Item =>
        items[Math.floor(next() * items.length)]!;
      const messages: (StatementRequest | WorkerRequest)[] = [];
      let id = 2;
      let active: string | undefined;
      let begun = 0;
      const transaction = (): string | 0 =>
        next() < 0.8 ? (active ?? 0) : pick([0, 'tx-999', `tx-${begun}`]);
      const params = (): JsonValue[] =>
        Array.from({length: Math.floor(next() * 4)}, () =>
          pick<JsonValue>([
            null,
            true,
            1,
            2.5,
            'text',
            '',
            {nested: [1, 'two']},
            [1, [2]],
          ]),
        );
      const request = (
        method: WorkerRequest['method'],
        requestParams?: unknown,
      ): WorkerRequest =>
        ({v: PROTOCOL_VERSION, id: id++, method, params: requestParams}) as WorkerRequest;
      for (let count = 5 + Math.floor(next() * 30); count > 0; count--) {
        const roll = next();
        if (roll < 0.45) {
          messages.push(
            statement(
              id++,
              pick([SELECT, SELECT, UPDATE, UPDATE, RETURNING, FAIL, 'BREAK']),
              params(),
              transaction(),
              pick([0, 0, 1]),
            ),
          );
        } else if (roll < 0.65) {
          messages.push(
            execution(id++, pick([1, 1, 2, 9]), params(), transaction(), pick([0, 1])),
          );
        } else if (roll < 0.75) {
          messages.push(request('prepareSql', {sql: pick([SELECT, UPDATE])}));
        } else if (roll < 0.85) {
          messages.push(request('beginTransaction'));
          active ??= `tx-${++begun}`;
        } else if (roll < 0.93) {
          const transactionId =
            next() < 0.85 ? (active ?? `tx-${begun}`) : 'tx-999';
          messages.push(
            request(pick(['commitTransaction', 'rollbackTransaction']), {
              transactionId,
            }),
          );
          if (transactionId === active) {
            active = undefined;
          }
        } else {
          messages.push(
            next() < 0.5 ? request('schema') : request('execSql', {sql: 'SELECT 1'}),
          );
        }
      }
      return messages;
    };
    const run = async (
      session: (StatementRequest | WorkerRequest)[],
      how: 'at once' | 'in turn' | 'as requests',
    ) => {
      const {engine, calls} = flatEngine();
      const scope = new FakeScope();
      startWorker({scope, durableEngineFactory: async () => engine});
      scope.send(initRequest());
      if (how !== 'in turn') {
        await nextTask();
      }
      for (const message of session) {
        const before = scope.posted.length;
        scope.send(
          structuredClone(
            how === 'as requests' && Array.isArray(message)
              ? statementRequest(message)
              : message,
          ),
        );
        expect(scope.posted.length).toBe(how === 'in turn' ? 0 : before + 1);
      }
      // The queue drains before the next task, and what it published is
      // announced in the task after that.
      await nextTask();
      await nextTask();
      return {posted: scope.posted, calls};
    };

    const next = random(2026);
    let flat = 0;
    let results = 0;
    let failures = 0;
    for (let sessions = 0; sessions < 60; sessions++) {
      const session = randomSession(next);
      const atOnce = await run(session, 'at once');
      expect(answers(atOnce).map(idOf)).toEqual(
        Array.from({length: session.length + 1}, (_, index) => index + 1),
      );
      for (const how of ['in turn', 'as requests'] as const) {
        const other = await run(session, how);
        expect(other.posted).toEqual(atOnce.posted);
        expect(other.calls).toEqual(atOnce.calls);
      }
      for (const message of answers(atOnce)) {
        if (Array.isArray(message)) {
          expect(isStatementResponse(message, true)).toBe(true);
          flat += 1;
        } else if ('ok' in message && message.ok) {
          results += 1;
        } else {
          failures += 1;
        }
      }
    }
    // The sessions met every kind of answer, many times over.
    expect(flat).toBeGreaterThan(200);
    expect(results).toBeGreaterThan(200);
    expect(failures).toBeGreaterThan(200);
  });

  it('wait behind a request that is queued ahead of them', async () => {
    const {engine, calls} = flatEngine();
    const {scope} = await openHost(engine);

    // A second init waits for the engine's promise, so what follows it in the
    // same turn must wait for it too.
    scope.send(initRequest(2));
    scope.send(statement(3, SELECT, [3]));
    scope.send(execution(4, 1, [4]));
    scope.send(statement(5, UPDATE, ['five', 5]));
    expect(scope.posted).toEqual([]);
    expect(calls).toEqual([]);

    await waitForPosted(scope, 4);
    expect(answers(scope).map(idOf)).toEqual([2, 3, 4, 5]);
    expect(calls.map(({params}) => params)).toEqual([[3], ['five', 5]]);
    expect(scope.posted[2]).toMatchObject({
      id: 4,
      ok: false,
      error: {code: 'PREPARED_STATEMENT_NOT_FOUND'},
    });

    // With the queue drained, the next is served at once again.
    scope.send(statement(6, SELECT, [6]));
    expect(idOf(scope.posted.at(-1)!)).toBe(6);
  });

  it('are refused when malformed, with the id when there is one, and never reach the engine', async () => {
    const {engine, calls} = flatEngine();
    const {scope} = await openHost(engine);
    const sparse: unknown[] = statement(21, SELECT, [1, 2, 3]);
    delete sparse[STATEMENT_PARAMS + 1];
    const malformed: [message: unknown[], id: number][] = [
      [[99, 8, STATEMENT_SQL, SELECT, 0, 0], 8],
      [[PROTOCOL_VERSION, 9, STATEMENT_PREPARED, 'not an id', 0, 0], 9],
      [[PROTOCOL_VERSION, 10, STATEMENT_SQL, 3, 0, 0], 10],
      [[PROTOCOL_VERSION, 11, 2, SELECT, 0, 0], 11],
      [[PROTOCOL_VERSION, 12, STATEMENT_SQL, SELECT, '', 0], 12],
      [[PROTOCOL_VERSION, 13, STATEMENT_SQL, SELECT, undefined, 0], 13],
      [[PROTOCOL_VERSION, 14, STATEMENT_SQL, SELECT, 0, 2], 14],
      [[PROTOCOL_VERSION, 15, STATEMENT_SQL, SELECT, 0, true], 15],
      [[PROTOCOL_VERSION, 16, STATEMENT_SQL, SELECT, 0, 0, 1n], 16],
      [[PROTOCOL_VERSION, 17, STATEMENT_SQL, SELECT, 0, 0, Number.NaN], 17],
      [[PROTOCOL_VERSION, 18, STATEMENT_SQL, SELECT, 0, 0, undefined], 18],
      [[PROTOCOL_VERSION, 19, STATEMENT_SQL, SELECT, 0, 0, [() => 1]], 19],
      [[PROTOCOL_VERSION, 20, STATEMENT_SQL, SELECT, 0], 20],
      [sparse, 21],
      [[PROTOCOL_VERSION, 0, STATEMENT_SQL, SELECT, 0, 0], 0],
      [[PROTOCOL_VERSION, 1.5, STATEMENT_SQL, SELECT, 0, 0], 0],
      [[PROTOCOL_VERSION, '22', STATEMENT_SQL, SELECT, 0, 0], 0],
      [[PROTOCOL_VERSION], 0],
      [[], 0],
    ];
    for (const [message, id] of malformed) {
      scope.posted.length = 0;
      scope.send(message);
      expect(scope.posted).toEqual([
        {
          v: PROTOCOL_VERSION,
          id,
          ok: false,
          error: {
            code: 'PROTOCOL_MISMATCH',
            message: 'The worker received an invalid TinyJoin protocol request',
          },
        },
      ]);
    }
    expect(calls).toEqual([]);
    expect(scope.closed).toBe(false);

    // The host goes on serving.
    scope.send(statement(23, SELECT, [1]));
    expect(idOf(scope.posted.at(-1)!)).toBe(23);
  });

  it('answer a failure with the error it raised, and go on serving', async () => {
    const {engine} = flatEngine();
    const {scope} = await openHost(engine);

    scope.send(statement(2, FAIL, ['STORAGE_ENGINE_POISONED']));
    scope.send(statement(3, 'BREAK'));
    scope.send(execution(4, 1, []));
    scope.send(statement(5, SELECT, [5], 'tx-1'));
    expect(scope.posted).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 2,
        ok: false,
        error: {
          code: 'STORAGE_ENGINE_POISONED',
          message: 'The engine refused STORAGE_ENGINE_POISONED',
        },
      },
      {
        v: PROTOCOL_VERSION,
        id: 3,
        ok: false,
        error: {code: 'WORKER_OPERATION_FAILED', message: 'The engine broke'},
      },
      {
        v: PROTOCOL_VERSION,
        id: 4,
        ok: false,
        error: {
          code: 'PREPARED_STATEMENT_NOT_FOUND',
          message: 'The statement is not prepared',
        },
      },
      {
        v: PROTOCOL_VERSION,
        id: 5,
        ok: false,
        error: {
          code: 'TRANSACTION_NOT_ACTIVE',
          message: 'The TinyJoin transaction is no longer active',
        },
      },
    ]);
    // The transaction's check comes before the engine, as for any request.
    expect(engine.executeSql).toHaveBeenCalledTimes(2);

    scope.send(statement(6, SELECT, [6]));
    expect(idOf(scope.posted.at(-1)!)).toBe(6);
  });

  it('fail a statement whose result cannot be posted', async () => {
    const {engine, calls} = flatEngine();
    const {scope} = await openHost(engine);
    const post = vi.spyOn(scope, 'postMessage');
    const notPosted = (id: number): Posted => ({
      v: PROTOCOL_VERSION,
      id,
      ok: false,
      error: {
        code: 'WORKER_OPERATION_FAILED',
        message: 'The result could not be cloned',
      },
    });
    for (const request of [
      statement(2, SELECT, [2]),
      statement(3, UPDATE, ['not posted', 3]),
    ]) {
      post.mockImplementationOnce(() => {
        throw new Error('The result could not be cloned');
      });
      scope.send(request);
      expect(scope.posted.at(-1)).toEqual(notPosted(request[1]));
    }
    expect(answers(scope)).toHaveLength(2);

    // One that waited its turn fails in the same way, whichever form its
    // result has, and the requests behind it are served as if it had not.
    // Here a second init waits for the engine's promise, and the results of
    // the two statements behind it are the ones that cannot be posted.
    post.mockImplementation((message: Posted) => {
      const result = Array.isArray(message) || ('ok' in message && message.ok);
      if (result && (idOf(message) === 5 || idOf(message) === 6)) {
        throw new Error('The result could not be cloned');
      }
      scope.posted.push(message);
    });
    scope.posted.length = 0;
    calls.length = 0;
    scope.send(initRequest(4));
    scope.send(statement(5, SELECT, [5]));
    scope.send(statement(6, UPDATE, ['not posted', 6]));
    scope.send(statement(7, SELECT, [7]));
    scope.send(statement(8, UPDATE, ['posted', 8]));
    expect(scope.posted).toEqual([]);
    await vi.waitFor(() => expect(answers(scope)).toHaveLength(5));
    expect(answers(scope)).toEqual([
      {v: PROTOCOL_VERSION, id: 4, ok: true, result: {revision: 1}},
      notPosted(5),
      notPosted(6),
      // The update whose result was not posted had run all the same.
      [
        PROTOCOL_VERSION,
        7,
        3,
        2,
        1,
        sqlData([{name: 'id', dataTypeID: 20}], [{id: 7}]),
      ],
      {
        v: PROTOCOL_VERSION,
        id: 8,
        ok: true,
        result: {
          command: 'UPDATE',
          revision: 3,
          rowCount: 1,
          tables: ['posts'],
          keys: {posts: [{id: 8}]},
          data: sqlData([], []),
        },
      },
    ]);
    expect(calls.map(({params}) => params)).toEqual([
      [5],
      ['not posted', 6],
      [7],
      ['posted', 8],
    ]);
  });

  it('are refused before the engine is opened, as their requests are', async () => {
    const scope = new FakeScope();
    startWorker({scope, durableEngineFactory: async () => flatEngine().engine});
    scope.send(statement(1, SELECT, [1]));
    await waitForPosted(scope, 1);
    expect(scope.posted).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 1,
        ok: false,
        error: {
          code: 'WORKER_NOT_INITIALIZED',
          message: 'Initialize the TinyJoin worker before sending other requests',
        },
      },
    ]);
  });

  it('answer the request form of a statement with a flat result too, but never a script', async () => {
    const {engine} = flatEngine();
    const scriptResults = [
      {
        command: 'SELECT',
        revision: 0,
        rowCount: 0,
        tables: [],
        keys: {},
        data: sqlData([], []),
      },
    ];
    vi.mocked(engine.execSql)
      .mockReturnValueOnce([])
      .mockReturnValueOnce(scriptResults);
    const {scope} = await openHost(engine);

    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'executeSql',
      params: {sql: SELECT, params: [2]},
    } satisfies WorkerRequest);
    expect(scope.posted[0]).toEqual([
      PROTOCOL_VERSION,
      2,
      3,
      0,
      1,
      expect.any(String),
    ]);
    // The engine is called as it always was for a request.
    expect(engine.executeSql).toHaveBeenCalledExactlyOnceWith(SELECT, [2], undefined);

    // A script's results are an array of results, whatever it holds.
    for (const [id, result] of [
      [3, []],
      [4, scriptResults],
    ] as const) {
      scope.send({
        v: PROTOCOL_VERSION,
        id,
        method: 'execSql',
        params: {sql: 'SELECT 1'},
      } satisfies WorkerRequest);
      expect(scope.posted.at(-1)).toEqual({
        v: PROTOCOL_VERSION,
        id,
        ok: true,
        result,
      });
    }
  });

  it('are served for code in the Worker under the names it gives, without a message', async () => {
    const {engine, calls} = flatEngine();
    let open!: (engine: WorkerEngine) => void;
    const scope = new FakeScope();
    const host = startWorker({
      scope,
      durableEngineFactory: () =>
        new Promise((resolve) => {
          open = resolve;
        }),
    });
    const initialized = host.request(initRequest());
    await Promise.resolve();
    // Nothing can be served at once while init waits for the engine.
    expect(host.statementNow(statement(1, SELECT, [1]), SELECT, undefined)).toBeUndefined();
    expect(calls).toEqual([]);
    open(engine);
    await initialized;

    // Behind a queued request it is not served either, and nothing happens
    // until that request has had its turn.
    const queued = host.request(initRequest());
    const select = statement(2, SELECT, [2]);
    expect(host.statementNow(select, SELECT, undefined)).toBeUndefined();
    expect(calls).toEqual([]);
    await queued;
    expect(host.statementNow(select, SELECT, undefined)).toEqual([
      0,
      0,
      3,
      0,
      1,
      sqlData([{name: 'id', dataTypeID: 20}], [{id: 2}]),
    ]);

    await host.request({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'prepareSql',
      params: {sql: UPDATE},
    });
    await host.request({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'beginTransaction',
      params: undefined,
    });

    // The caller's statement and transaction stand in for the request's own,
    // and the result comes back as the engine returned it, for the caller to
    // address.
    const request = execution(7, 41, ['renamed', 3], 'owner/tx-1');
    expect(host.statementNow(request, 1, 'tx-1')).toEqual([
      0, 0, 1, 0, 1, 'posts', 1, 'id', 3,
    ]);
    expect(calls.at(-1)).toEqual({
      target: 1,
      params: ['renamed', 3],
      rowMode: undefined,
    });
    expect(
      host.statementNow(statement(8, RETURNING, [4], 'owner/tx-1'), RETURNING, 'tx-1'),
    ).toMatchObject({command: 'INSERT', keys: {posts: [{id: 4}]}});

    // A failure is thrown as it was raised, by this host's own checks or by
    // the engine, for the caller to make the error of a response from.
    const thrown = (run: () => unknown): unknown => {
      try {
        run();
      } catch (error) {
        return error;
      }
      return undefined;
    };
    const refused = thrown(() => host.statementNow(request, 1, undefined));
    expect(refused).toBeInstanceOf(Error);
    expect({...(refused as Error), message: (refused as Error).message}).toEqual({
      code: 'TRANSACTION_ACTIVE',
      message: 'Use the active TinyJoin transaction for this operation',
    });
    expect(calls).toHaveLength(3);
    const failed = thrown(() =>
      host.statementNow(statement(9, FAIL, ['RECOVERY_REQUIRED'], 'x'), FAIL, 'tx-1'),
    );
    expect(failed).toBe(vi.mocked(engine.executeSql).mock.results.at(-1)!.value);
    expect(failed).toMatchObject({
      code: 'RECOVERY_REQUIRED',
      message: 'The engine refused RECOVERY_REQUIRED',
    });

    // Nothing was posted: the caller holds every outcome.
    expect(answers(scope)).toEqual([]);
  });
});
