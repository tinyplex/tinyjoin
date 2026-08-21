import {describe, expect, it, vi} from 'vitest';

import {
  Client,
  create,
  type Transaction,
} from '../../src/client/client.ts';
import {PROTOCOL_VERSION, type WorkerRequest} from '../../src/protocol.ts';
import {FakeWorker} from '../helpers/fake-worker.ts';

const ID_FIELD = [{name: 'id', dataTypeID: 20}];
const POST_FIELDS = [
  {name: 'id', dataTypeID: 20},
  {name: 'title', dataTypeID: 25},
];

function respondingWorker(): FakeWorker {
  const worker = new FakeWorker();
  worker.onPost = (message) => {
    queueMicrotask(() => {
      if (message.method === 'init') {
        respondOk(worker, message, {revision: 0});
      } else if (message.method === 'query') {
        respondOk(worker, message, {
          revision: 2,
          rows: [{id: 1, title: 'hello'}],
          fields: POST_FIELDS,
        });
      } else if (message.method === 'close') {
        respondOk(worker, message, undefined);
      }
    });
  };
  return worker;
}

function writableWorker(): FakeWorker {
  const worker = new FakeWorker();
  let revision = 0;
  let nextTransactionId = 1;
  worker.onPost = (message) => {
    queueMicrotask(() => {
      if (message.method === 'init') {
        respondOk(worker, message, {revision});
      } else if (message.method === 'executeSql') {
        const command = sqlCommand(message.params.sql);
        const writes = /^(?:ALTER|CREATE|DELETE|DROP|INSERT|UPDATE)$/.test(
          command,
        );
        if (writes && message.params.transactionId === undefined) {
          revision += 1;
        }
        const isSelect = command === 'SELECT';
        const returnsId = isSelect || /RETURNING\s+id/i.test(message.params.sql);
        respondOk(worker, message, {
          command,
          fields: returnsId ? ID_FIELD : [],
          revision,
          rowCount: isSelect || writes ? 1 : 0,
          rows: returnsId
            ? [{id: message.params.params[0] ?? 1}]
            : [],
          tables: writes ? ['posts'] : [],
        });
      } else if (message.method === 'execSql') {
        if (message.params.transactionId === undefined) {
          revision += 1;
        }
        respondOk(worker, message, [
          {
            command: 'CREATE',
            fields: [],
            revision,
            rowCount: 0,
            rows: [],
            tables: ['posts'],
          },
          {
            command: 'INSERT',
            fields: [],
            revision,
            rowCount: 1,
            rows: [],
            tables: ['posts'],
          },
          {
            command: 'SELECT',
            fields: ID_FIELD,
            revision,
            rowCount: 1,
            rows: [{id: 1}],
            tables: [],
          },
        ]);
      } else if (message.method === 'beginTransaction') {
        respondOk(worker, message, {
          transactionId: `tx-${nextTransactionId++}`,
        });
      } else if (message.method === 'commitTransaction') {
        revision += 1;
        respondOk(worker, message, {revision, tables: ['posts']});
      } else if (
        message.method === 'replaceTable' ||
        message.method === 'applyBatch'
      ) {
        revision += 1;
        respondOk(worker, message, {revision, tables: ['posts']});
      } else if (message.method === 'rollbackTransaction') {
        respondOk(worker, message, undefined);
      } else if (message.method === 'query') {
        respondOk(worker, message, {
          revision,
          rows: [{id: 1, title: 'hello'}],
          fields: POST_FIELDS,
        });
      } else if (message.method === 'close') {
        respondOk(worker, message, undefined);
      }
    });
  };
  return worker;
}

function respondOk(
  worker: FakeWorker,
  message: WorkerRequest,
  result: unknown,
): void {
  worker.respond({
    v: PROTOCOL_VERSION,
    id: message.id,
    ok: true,
    result,
  });
}

function sqlCommand(sql: string): string {
  return sql.trim().split(/\s+/, 1)[0]!.toUpperCase();
}

describe('Client', () => {
  it('uses memory by default and exposes promise-backed readiness', async () => {
    const worker = respondingWorker();
    const client = new Client({worker});

    expect(client.ready).toBe(false);
    expect(client.closed).toBe(false);
    await client.waitReady;
    expect(client.ready).toBe(true);
    expect(
      (worker.posted[0] as Extract<WorkerRequest, {method: 'init'}>).params
        .storage,
    ).toEqual({kind: 'memory'});

    await client.close();
    expect(client.ready).toBe(false);
    expect(client.closed).toBe(true);
  });

  it('creates only after initialization and resolves OPFS data directories', async () => {
    const worker = new FakeWorker();
    let init: Extract<WorkerRequest, {method: 'init'}> | undefined;
    worker.onPost = (message) => {
      if (message.method === 'init') {
        init = message;
      } else if (message.method === 'close') {
        respondOk(worker, message, undefined);
      }
    };

    let resolved = false;
    const creating = create('opfs://application-cache', {worker}).then(
      (client) => {
        resolved = true;
        return client;
      },
    );
    await vi.waitFor(() => expect(init).toBeDefined());
    expect(resolved).toBe(false);
    expect(init!.params.storage).toEqual({
      kind: 'opfs',
      name: 'application-cache',
    });

    respondOk(worker, init!, {revision: 3});
    const client = await creating;
    expect(client.ready).toBe(true);
    expect(client.getRevision()).toBe(3);
    await client.close();
  });

  it('accepts dataDir in options and rejects ambiguous or unsupported storage', async () => {
    const optionsWorker = respondingWorker();
    const client = await create({
      dataDir: 'opfs://options-database',
      worker: optionsWorker,
    });
    expect(
      (optionsWorker.posted[0] as Extract<WorkerRequest, {method: 'init'}>)
        .params.storage,
    ).toEqual({kind: 'opfs', name: 'options-database'});
    await client.close();

    const ambiguousWorker = new FakeWorker();
    await expect(
      create('memory://', {
        dataDir: 'opfs://also-here',
        worker: ambiguousWorker,
      }),
    ).rejects.toThrow('either positionally or in options.dataDir');
    expect(ambiguousWorker.posted).toEqual([]);

    for (const dataDir of ['idb://database', 'opfs://', 'opfs://bad/name']) {
      const worker = new FakeWorker();
      await expect(create(dataDir, {worker})).rejects.toThrowError(TypeError);
      expect(worker.posted).toEqual([]);
    }

    for (const unsupported of [
      {storage: {kind: 'opfs', name: 'old-shape'}},
      {parsers: {}},
    ]) {
      const workerFactory = vi.fn(() => new FakeWorker());
      await expect(
        create({...unsupported, workerFactory} as never),
      ).rejects.toThrow('client options support only');
      expect(workerFactory).not.toHaveBeenCalled();
    }

    const inheritedOptionsFactory = vi.fn(() => new FakeWorker());
    const inheritedOptions = Object.assign(new Date(), {
      workerFactory: inheritedOptionsFactory,
    });
    await expect(create(inheritedOptions as never)).rejects.toThrow(
      'client options support only',
    );
    expect(inheritedOptionsFactory).not.toHaveBeenCalled();
  });

  it.each([{}, {revision: 0, extra: true}, {revision: -1}])(
    'rejects an invalid initialization result',
    async (result) => {
      const worker = new FakeWorker();
      worker.onPost = (message) => {
        if (message.method === 'init') {
          queueMicrotask(() => respondOk(worker, message, result));
        }
      };

      await expect(create({worker})).rejects.toMatchObject({
        name: 'ClientError',
        code: 'PROTOCOL_MISMATCH',
      });
      expect(worker.terminated).toBe(true);
    },
  );

  it('supports an awaitable fluent query chain', async () => {
    const worker = respondingWorker();
    const client = new Client({worker});

    const response = await client
      .from<{id: number; title: string}>('posts')
      .select('id, title')
      .eq('id', 1);

    expect(response).toEqual({
      data: [{id: 1, title: 'hello'}],
      error: null,
      revision: 2,
    });
    expect(
      (worker.posted[1] as Extract<WorkerRequest, {method: 'query'}>).params
        .plan,
    ).toEqual({
      table: 'posts',
      columns: ['id', 'title'],
      filters: [{column: 'id', operator: 'eq', value: 1}],
    });
  });

  it('uses query for parameterized reads and writes with SQL results', async () => {
    const worker = writableWorker();
    const client = await create({worker});

    await expect(
      client.query<{id: number}>(
        'INSERT INTO posts (id) VALUES ($1) RETURNING id',
        [7],
      ),
    ).resolves.toEqual({
      affectedRows: 1,
      command: 'INSERT',
      fields: ID_FIELD,
      revision: 1,
      rowCount: 1,
      rows: [{id: 7}],
      tables: ['posts'],
    });
    expect(client.getRevision()).toBe(1);
    expect(
      (worker.posted[1] as Extract<WorkerRequest, {method: 'executeSql'}>)
        .params,
    ).toEqual({
      sql: 'INSERT INTO posts (id) VALUES ($1) RETURNING id',
      params: [7],
    });

    await expect(
      client.query<unknown[]>('SELECT id FROM posts', [], {
        rowMode: 'array',
      }),
    ).resolves.toMatchObject({
      affectedRows: 0,
      command: 'SELECT',
      fields: ID_FIELD,
      rowCount: 1,
      rows: [[1]],
      tables: [],
    });
    await client.close();
  });

  it('parameterizes sql tagged-template values without interpolation', async () => {
    const worker = writableWorker();
    const client = await create({worker});

    await client.sql<{id: number}>`SELECT id FROM posts WHERE id = ${7}`;
    expect(
      (worker.posted[1] as Extract<WorkerRequest, {method: 'executeSql'}>)
        .params,
    ).toEqual({sql: 'SELECT id FROM posts WHERE id = $1', params: [7]});
    expect(() =>
      client.sql(['SELECT 1'] as unknown as TemplateStringsArray, 1),
    ).toThrow('must be used as a tagged template');
    await client.close();
  });

  it('executes a parameterless SQL script and normalizes each result', async () => {
    const worker = writableWorker();
    const client = await create({worker});

    await expect(
      client.exec('CREATE TABLE posts; INSERT INTO posts; SELECT id FROM posts'),
    ).resolves.toEqual([
      {
        affectedRows: 0,
        command: 'CREATE',
        fields: [],
        revision: 1,
        rows: [],
        tables: ['posts'],
      },
      {
        affectedRows: 1,
        command: 'INSERT',
        fields: [],
        revision: 1,
        rowCount: 1,
        rows: [],
        tables: ['posts'],
      },
      {
        affectedRows: 0,
        command: 'SELECT',
        fields: ID_FIELD,
        revision: 1,
        rowCount: 1,
        rows: [{id: 1}],
        tables: [],
      },
    ]);
    expect(
      (worker.posted[1] as Extract<WorkerRequest, {method: 'execSql'}>).params,
    ).toEqual({
      sql: 'CREATE TABLE posts; INSERT INTO posts; SELECT id FROM posts',
    });
    await client.close();
  });

  it('tracks revisions returned by non-SQL bulk writes', async () => {
    const worker = writableWorker();
    const client = await create({worker});

    await expect(
      client.replaceTable({name: 'posts', primaryKey: ['id']}, [{id: 1}]),
    ).resolves.toEqual({revision: 1, tables: ['posts']});
    expect(client.getRevision()).toBe(1);
    await expect(
      client.applyBatch({
        changes: [{type: 'delete', table: 'posts', key: {id: 1}}],
      }),
    ).resolves.toEqual({revision: 2, tables: ['posts']});
    expect(client.getRevision()).toBe(2);
    await client.close();
  });

  it('rejects unsupported options before executing SQL', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    const requestCount = worker.posted.length;

    await expect(
      client.query('INSERT INTO posts VALUES (1)', [], {
        parsers: {},
      } as never),
    ).rejects.toThrow('support only rowMode');
    expect(worker.posted).toHaveLength(requestCount);
    await client.close();
  });

  it('rejects array row mode when field metadata is unavailable', async () => {
    const worker = writableWorker();
    worker.onPost = (message) => {
      queueMicrotask(() => {
        if (message.method === 'init') {
          respondOk(worker, message, {revision: 0});
        } else if (message.method === 'executeSql') {
          respondOk(worker, message, {
            command: 'SELECT',
            fields: [],
            revision: 0,
            rowCount: 1,
            rows: [{id: 1}],
            tables: [],
          });
        } else if (message.method === 'close') {
          respondOk(worker, message, undefined);
        }
      });
    };
    const client = await create({worker});

    await expect(
      client.query('SELECT * FROM untyped', [], {rowMode: 'array'}),
    ).rejects.toMatchObject({code: 'ROW_METADATA_UNAVAILABLE'});
    await client.close();
  });

  it('scopes transaction operations and commits after pending work', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    let escaped: Transaction | undefined;

    const value = await client.transaction(async (transaction) => {
      escaped = transaction;
      expect(transaction.closed).toBe(false);
      const pendingInsert = transaction.query(
        'INSERT INTO posts (id) VALUES ($1)',
        [9],
      );
      const selected = await transaction
        .from<{id: number}>('posts')
        .select('id')
        .eq('id', 9);
      await pendingInsert;
      return selected.data?.[0]?.id;
    });

    expect(value).toBe(1);
    expect(escaped!.closed).toBe(true);
    const transactionRequests = (worker.posted as WorkerRequest[]).filter(
      (message) =>
        message.method === 'beginTransaction' ||
        message.method === 'executeSql' ||
        message.method === 'query' ||
        message.method === 'commitTransaction',
    );
    expect(transactionRequests.map((message) => message.method)).toEqual([
      'beginTransaction',
      'executeSql',
      'query',
      'commitTransaction',
    ]);
    expect(
      (transactionRequests[1] as Extract<WorkerRequest, {method: 'executeSql'}>)
        .params.transactionId,
    ).toBe('tx-1');

    const requestCount = worker.posted.length;
    expect(() => escaped!.query('SELECT * FROM posts')).toThrowError(
      expect.objectContaining({code: 'TRANSACTION_CLOSED'}),
    );
    expect(worker.posted).toHaveLength(requestCount);
    await client.close();
  });

  it('allows explicit rollback to resolve without committing', async () => {
    const worker = writableWorker();
    const client = await create({worker});

    await expect(
      client.transaction(async (transaction) => {
        await transaction.query('DELETE FROM posts WHERE id = $1', [1]);
        await transaction.rollback();
        expect(transaction.closed).toBe(true);
        return 'discarded';
      }),
    ).resolves.toBe('discarded');
    expect(transactionMethods(worker)).toEqual([
      'beginTransaction',
      'executeSql',
      'rollbackTransaction',
    ]);
    await client.close();
  });

  it('rolls back when its callback fails', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    const failure = new Error('application failed');

    await expect(
      client.transaction(async (transaction) => {
        await transaction.query('DELETE FROM posts WHERE id = $1', [1]);
        throw failure;
      }),
    ).rejects.toBe(failure);
    expect(transactionMethods(worker)).toEqual([
      'beginTransaction',
      'executeSql',
      'rollbackTransaction',
    ]);
    await client.close();
  });

  it('retries cleanup after an explicit rollback request fails', async () => {
    const worker = writableWorker();
    const originalOnPost = worker.onPost!;
    let rollbacks = 0;
    worker.onPost = (message) => {
      if (message.method !== 'rollbackTransaction') {
        originalOnPost(message);
        return;
      }
      rollbacks += 1;
      queueMicrotask(() => {
        if (rollbacks === 1) {
          worker.respond({
            v: PROTOCOL_VERSION,
            id: message.id,
            ok: false,
            error: {code: 'ROLLBACK_FAILED', message: 'rollback failed'},
          });
        } else {
          respondOk(worker, message, undefined);
        }
      });
    };
    const client = await create({worker});

    await expect(
      client.transaction(async (transaction) => {
        try {
          await transaction.rollback();
        } catch {
          // The outer transaction must still surface and clean up this failure.
        }
      }),
    ).rejects.toMatchObject({code: 'ROLLBACK_FAILED'});
    expect(rollbacks).toBe(2);
    await client.close();
  });

  it('serializes transaction callbacks', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    const order: string[] = [];

    await Promise.all([
      client.transaction(async (transaction) => {
        order.push('first-start');
        await transaction.query('INSERT INTO posts (id) VALUES (1)');
        order.push('first-end');
      }),
      client.transaction(async (transaction) => {
        order.push('second-start');
        await transaction.query('INSERT INTO posts (id) VALUES (2)');
        order.push('second-end');
      }),
    ]);

    expect(order).toEqual([
      'first-start',
      'first-end',
      'second-start',
      'second-end',
    ]);
    expect(() => client.transaction(undefined as never)).toThrowError(
      'TinyGres transaction requires a callback',
    );
    await client.close();
  });

  it('notifies only subscriptions affected by a worker mutation', async () => {
    const worker = respondingWorker();
    const client = await create({worker});
    const posts = vi.fn();
    const users = vi.fn();
    client.subscribe({tables: ['posts']}, posts);
    client.subscribe({tables: ['users']}, users);

    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 5, tables: ['posts']},
    });

    expect(posts).toHaveBeenCalledWith({revision: 5, tables: ['posts']});
    expect(users).not.toHaveBeenCalled();
    expect(client.getRevision()).toBe(5);
    await client.close();
  });

  it('makes concurrent close calls await the same worker cleanup', async () => {
    const worker = new FakeWorker();
    let closeRequest: Extract<WorkerRequest, {method: 'close'}> | undefined;
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() => respondOk(worker, message, {revision: 0}));
      } else if (message.method === 'close') {
        closeRequest = message;
      }
    };
    const client = await create({worker});

    const first = client.close();
    const second = client.close();
    expect(second).toBe(first);
    await vi.waitFor(() => expect(closeRequest).toBeDefined());
    expect(worker.terminated).toBe(false);
    expect(client.ready).toBe(false);
    expect(client.closed).toBe(false);
    await expect(client.query('SELECT * FROM posts')).rejects.toMatchObject({
      code: 'CLIENT_CLOSED',
    });

    respondOk(worker, closeRequest!, undefined);
    await second;
    expect(worker.terminated).toBe(true);
    expect(client.closed).toBe(true);
    expect(client.ready).toBe(false);
  });
});

function transactionMethods(worker: FakeWorker): string[] {
  return (worker.posted as WorkerRequest[])
    .filter((message) =>
      [
        'beginTransaction',
        'executeSql',
        'commitTransaction',
        'rollbackTransaction',
      ].includes(message.method),
    )
    .map((message) => message.method);
}
