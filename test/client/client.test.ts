import {describe, expect, it, vi} from 'vitest';

import {
  Client,
  create,
  type PreparedStatement,
  type Transaction,
} from '../../src/client/client.ts';
import {PROTOCOL_VERSION, type WorkerRequest} from '../../src/protocol.ts';
import {FakeWorker} from '../helpers/fake-worker.ts';

const ID_FIELD = [{name: 'id', dataTypeID: 20}];

function respondingWorker(): FakeWorker {
  const worker = new FakeWorker();
  worker.onPost = (message) => {
    queueMicrotask(() => {
      if (message.method === 'init') {
        respondOk(worker, message, {revision: 0});
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
  let nextStatementId = 1;
  const preparedSql = new Map<number, string>();
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
      } else if (message.method === 'prepareSql') {
        const statementId = nextStatementId++;
        preparedSql.set(statementId, message.params.sql);
        respondOk(worker, message, {statementId});
      } else if (message.method === 'executePrepared') {
        const command = sqlCommand(
          preparedSql.get(message.params.statementId) ?? 'SELECT',
        );
        const writes = /^(?:DELETE|INSERT|UPDATE)$/.test(command);
        if (writes && message.params.transactionId === undefined) {
          revision += 1;
        }
        const isSelect = command === 'SELECT';
        respondOk(worker, message, {
          command,
          fields: isSelect ? ID_FIELD : [],
          revision,
          rowCount: 1,
          rows: isSelect ? [{id: message.params.params[0] ?? 1}] : [],
          tables: writes ? ['posts'] : [],
        });
      } else if (message.method === 'closePrepared') {
        preparedSql.delete(message.params.statementId);
        respondOk(worker, message, undefined);
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
      } else if (message.method === 'rollbackTransaction') {
        respondOk(worker, message, undefined);
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

  it('prepares reusable statements and closes them idempotently', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    const statement = await client.prepare<{id: number}>(
      'SELECT id FROM posts WHERE id = $1',
    );

    expect(statement.closed).toBe(false);
    expect(
      (worker.posted[1] as Extract<WorkerRequest, {method: 'prepareSql'}>)
        .params,
    ).toEqual({sql: 'SELECT id FROM posts WHERE id = $1'});
    await expect(statement.execute([7])).resolves.toMatchObject({
      command: 'SELECT',
      rows: [{id: 7}],
      rowCount: 1,
    });
    expect(
      (worker.posted[2] as Extract<WorkerRequest, {method: 'executePrepared'}>)
        .params,
    ).toEqual({statementId: 1, params: [7]});

    const firstClose = statement.close();
    const secondClose = statement.close();
    expect(statement.closed).toBe(true);
    expect(secondClose).toBe(firstClose);
    await firstClose;
    expect(
      (worker.posted[3] as Extract<WorkerRequest, {method: 'closePrepared'}>)
        .params,
    ).toEqual({statementId: 1});
    const requestCount = worker.posted.length;
    await expect(statement.execute([8])).rejects.toMatchObject({
      code: 'PREPARED_STATEMENT_CLOSED',
    });
    await statement.close();
    expect(worker.posted).toHaveLength(requestCount);
    await client.close();
  });

  it('allows prepared execution only through the active transaction object', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    const statement = await client.prepare(
      'UPDATE posts SET title = $1 WHERE id = $2',
    );

    await client.transaction(async (transaction) => {
      await expect(client.prepare('SELECT id FROM posts')).rejects.toMatchObject({
        code: 'TRANSACTION_ACTIVE',
      });
      await expect(statement.execute(['direct', 1])).rejects.toMatchObject({
        code: 'TRANSACTION_ACTIVE',
      });
      await expect(statement.close()).rejects.toMatchObject({
        code: 'TRANSACTION_ACTIVE',
      });
      expect(statement.closed).toBe(false);
      void transaction.execute(statement, ['transaction', 1]);
    });

    const methods = (worker.posted as WorkerRequest[]).map(
      (request) => request.method,
    );
    expect(methods).toContain('executePrepared');
    expect(methods.indexOf('executePrepared')).toBeLessThan(
      methods.indexOf('commitTransaction'),
    );
    const execution = (worker.posted as WorkerRequest[]).find(
      (request): request is Extract<WorkerRequest, {method: 'executePrepared'}> =>
        request.method === 'executePrepared',
    );
    expect(execution?.params).toEqual({
      statementId: 1,
      params: ['transaction', 1],
      transactionId: 'tx-1',
    });
    await statement.close();
    await client.close();
  });

  it('keeps caught prepared failures inside the transaction callback recoverable', async () => {
    const worker = writableWorker();
    const originalOnPost = worker.onPost!;
    worker.onPost = (message) => {
      if (message.method !== 'executePrepared') {
        originalOnPost(message);
        return;
      }
      queueMicrotask(() =>
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: false,
          error: {code: 'BIND_ERROR', message: 'bad prepared parameters'},
        }),
      );
    };
    const client = await create({worker});
    const statement = await client.prepare('UPDATE posts SET title = $1');

    await client.transaction(async (transaction) => {
      await expect(transaction.execute(statement, [1])).rejects.toMatchObject({
        code: 'BIND_ERROR',
      });
      await transaction.query('SELECT id FROM posts');
    });
    expect(transactionMethods(worker)).toEqual([
      'beginTransaction',
      'executePrepared',
      'executeSql',
      'commitTransaction',
    ]);
    await statement.close();
    await client.close();
  });

  it('orders statement close after in-flight execution', async () => {
    const worker = new FakeWorker();
    let execution:
      | Extract<WorkerRequest, {method: 'executePrepared'}>
      | undefined;
    let statementClose:
      | Extract<WorkerRequest, {method: 'closePrepared'}>
      | undefined;
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() => respondOk(worker, message, {revision: 0}));
      } else if (message.method === 'prepareSql') {
        queueMicrotask(() => respondOk(worker, message, {statementId: 1}));
      } else if (message.method === 'executePrepared') {
        execution = message;
      } else if (message.method === 'closePrepared') {
        statementClose = message;
      } else if (message.method === 'close') {
        queueMicrotask(() => respondOk(worker, message, undefined));
      }
    };
    const client = await create({worker});
    const statement = await client.prepare<{id: number}>(
      'SELECT id FROM posts WHERE id = $1',
    );
    const pending = statement.execute([4]);
    await vi.waitFor(() => expect(execution).toBeDefined());

    const firstClose = statement.close();
    expect(statement.closed).toBe(true);
    expect(statementClose).toBeUndefined();
    respondOk(worker, execution!, {
      command: 'SELECT',
      fields: ID_FIELD,
      revision: 0,
      rowCount: 1,
      rows: [{id: 4}],
      tables: [],
    });
    await pending;
    await vi.waitFor(() => expect(statementClose).toBeDefined());
    respondOk(worker, statementClose!, undefined);
    await firstClose;
    await client.close();
  });

  it('finishes an accepted statement close before beginning a transaction', async () => {
    const worker = new FakeWorker();
    let execution:
      | Extract<WorkerRequest, {method: 'executePrepared'}>
      | undefined;
    let statementClose:
      | Extract<WorkerRequest, {method: 'closePrepared'}>
      | undefined;
    let begin:
      | Extract<WorkerRequest, {method: 'beginTransaction'}>
      | undefined;
    let commit:
      | Extract<WorkerRequest, {method: 'commitTransaction'}>
      | undefined;
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() => respondOk(worker, message, {revision: 0}));
      } else if (message.method === 'prepareSql') {
        queueMicrotask(() => respondOk(worker, message, {statementId: 1}));
      } else if (message.method === 'executePrepared') {
        execution = message;
      } else if (message.method === 'closePrepared') {
        statementClose = message;
      } else if (message.method === 'beginTransaction') {
        begin = message;
      } else if (message.method === 'commitTransaction') {
        commit = message;
      } else if (message.method === 'close') {
        queueMicrotask(() => respondOk(worker, message, undefined));
      }
    };
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts');
    const executionPromise = statement.execute();
    await vi.waitFor(() => expect(execution).toBeDefined());

    const closePromise = statement.close();
    const transactionPromise = client.transaction(() => undefined);
    await new Promise<void>((resolve) => queueMicrotask(resolve));
    expect(begin).toBeUndefined();

    respondOk(worker, execution!, {
      command: 'SELECT',
      fields: ID_FIELD,
      revision: 0,
      rowCount: 1,
      rows: [{id: 1}],
      tables: [],
    });
    await executionPromise;
    await vi.waitFor(() => expect(statementClose).toBeDefined());
    expect(begin).toBeUndefined();

    respondOk(worker, statementClose!, undefined);
    await closePromise;
    await vi.waitFor(() => expect(begin).toBeDefined());
    respondOk(worker, begin!, {transactionId: 'tx-1'});
    await vi.waitFor(() => expect(commit).toBeDefined());
    respondOk(worker, commit!, {revision: 0, tables: []});
    await transactionPromise;
    expect(
      (worker.posted as WorkerRequest[]).map((request) => request.method),
    ).toEqual([
      'init',
      'prepareSql',
      'executePrepared',
      'closePrepared',
      'beginTransaction',
      'commitTransaction',
    ]);
    await client.close();
  });

  it('settles failed closes without weakening the transaction reservation', async () => {
    const worker = new FakeWorker();
    let nextStatementId = 1;
    let failedClose:
      | Extract<WorkerRequest, {method: 'closePrepared'}>
      | undefined;
    let begin:
      | Extract<WorkerRequest, {method: 'beginTransaction'}>
      | undefined;
    let commit:
      | Extract<WorkerRequest, {method: 'commitTransaction'}>
      | undefined;
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() => respondOk(worker, message, {revision: 0}));
      } else if (message.method === 'prepareSql') {
        const statementId = nextStatementId++;
        queueMicrotask(() => respondOk(worker, message, {statementId}));
      } else if (
        message.method === 'closePrepared' &&
        message.params.statementId === 1
      ) {
        failedClose = message;
      } else if (message.method === 'closePrepared') {
        queueMicrotask(() => respondOk(worker, message, undefined));
      } else if (message.method === 'beginTransaction') {
        begin = message;
      } else if (message.method === 'commitTransaction') {
        commit = message;
      } else if (message.method === 'close') {
        queueMicrotask(() => respondOk(worker, message, undefined));
      }
    };
    const client = await create({worker});
    const first = await client.prepare('SELECT id FROM posts');
    const second = await client.prepare('SELECT id FROM posts');

    const closing = first.close();
    const transaction = client.transaction(() => undefined);
    await vi.waitFor(() => expect(failedClose).toBeDefined());
    expect(begin).toBeUndefined();
    worker.respond({
      v: PROTOCOL_VERSION,
      id: failedClose!.id,
      ok: false,
      error: {code: 'PREPARED_CLOSE_FAILED', message: 'close failed'},
    });
    await expect(closing).rejects.toMatchObject({
      code: 'PREPARED_CLOSE_FAILED',
    });

    await vi.waitFor(() => expect(begin).toBeDefined());
    await expect(second.close()).rejects.toMatchObject({
      code: 'TRANSACTION_ACTIVE',
    });
    expect(second.closed).toBe(false);
    respondOk(worker, begin!, {transactionId: 'tx-1'});
    await vi.waitFor(() => expect(commit).toBeDefined());
    respondOk(worker, commit!, {revision: 0, tables: []});
    await transaction;

    await second.close();
    await client.close();
  });

  it('rejects forged, foreign, escaped, and client-closed statement use', async () => {
    const firstWorker = writableWorker();
    const secondWorker = writableWorker();
    const firstClient = await create({worker: firstWorker});
    const secondClient = await create({worker: secondWorker});
    const statement = await firstClient.prepare('SELECT id FROM posts');
    const forged = {
      execute: vi.fn(),
      close: vi.fn(),
      closed: false,
    } as unknown as PreparedStatement;
    let escaped: Transaction | undefined;

    // A statement's methods belong to their own statement rather than to
    // whatever they are called on, so detaching one keeps working and a forged
    // receiver cannot redirect it. A forged statement is rejected where one is
    // accepted as an argument, which is what transaction.execute checks below.
    const {execute} = statement;
    await expect(execute()).resolves.toMatchObject({command: 'SELECT'});
    await expect(execute.call(forged, [])).resolves.toMatchObject({
      command: 'SELECT',
    });
    await secondClient.transaction((transaction) => {
      escaped = transaction;
      expect(() => transaction.execute(statement)).toThrowError(
        expect.objectContaining({code: 'PREPARED_STATEMENT_CLIENT_MISMATCH'}),
      );
      expect(() => transaction.execute(forged)).toThrowError(
        expect.objectContaining({code: 'INVALID_PREPARED_STATEMENT'}),
      );
    });
    expect(() => escaped!.execute(statement)).toThrowError(
      expect.objectContaining({code: 'TRANSACTION_CLOSED'}),
    );

    const firstClose = firstClient.close();
    expect(statement.closed).toBe(true);
    await firstClose;
    await expect(statement.execute()).rejects.toMatchObject({
      code: 'CLIENT_CLOSED',
    });
    await expect(statement.close()).resolves.toBeUndefined();
    expect(
      (firstWorker.posted as WorkerRequest[]).some(
        (request) => request.method === 'closePrepared',
      ),
    ).toBe(false);
    await secondClient.close();
  });

  it('rejects a prepare result that races client close', async () => {
    const worker = new FakeWorker();
    let prepareRequest: Extract<WorkerRequest, {method: 'prepareSql'}> | undefined;
    let closeRequest: Extract<WorkerRequest, {method: 'close'}> | undefined;
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() => respondOk(worker, message, {revision: 0}));
      } else if (message.method === 'prepareSql') {
        prepareRequest = message;
      } else if (message.method === 'close') {
        closeRequest = message;
      }
    };
    const client = await create({worker});
    const preparing = client.prepare('SELECT id FROM posts');
    await vi.waitFor(() => expect(prepareRequest).toBeDefined());
    const closing = client.close();
    await vi.waitFor(() => expect(closeRequest).toBeDefined());

    respondOk(worker, prepareRequest!, {statementId: 1});
    await expect(preparing).rejects.toMatchObject({code: 'CLIENT_CLOSED'});
    respondOk(worker, closeRequest!, undefined);
    await closing;
    expect(
      (worker.posted as WorkerRequest[]).some(
        (request) => request.method === 'closePrepared',
      ),
    ).toBe(false);
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
      client.query('SELECT * FROM missing_metadata', [], {rowMode: 'array'}),
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
      const selected = await transaction.query<{id: number}>(
        'SELECT id FROM posts WHERE id = $1',
        [9],
      );
      await pendingInsert;
      return selected.rows[0]?.id;
    });

    expect(value).toBe(9);
    expect(escaped!.closed).toBe(true);
    const transactionRequests = (worker.posted as WorkerRequest[]).filter(
      (message) =>
        message.method === 'beginTransaction' ||
        message.method === 'executeSql' ||
        message.method === 'commitTransaction',
    );
    expect(transactionRequests.map((message) => message.method)).toEqual([
      'beginTransaction',
      'executeSql',
      'executeSql',
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
      'TinyJoin transaction requires a callback',
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
        'executePrepared',
        'commitTransaction',
        'rollbackTransaction',
      ].includes(message.method),
    )
    .map((message) => message.method);
}
