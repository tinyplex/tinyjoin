import {afterEach, describe, expect, it, onTestFinished, vi} from 'vitest';

import {
  Client,
  create,
  type PreparedStatement,
  type Transaction,
} from '../../src/client/client.ts';
import {
  PROTOCOL_VERSION,
  STATEMENT_PREPARED,
  STATEMENT_SQL,
  isStatementRequest,
  isWorkerRequest,
  type Results,
  type RowMode,
  type SqlResult,
  type WorkerRequest,
} from '../../src/protocol.ts';
import {reactions} from './reactions.ts';
import {NO_ROWS, StatementWorker, flatResponse} from './statement-forms.ts';
import {unhandledRejections} from './unhandled.ts';

const ID_FIELD = [{name: 'id', dataTypeID: 20}];

function respondingWorker(): StatementWorker {
  const worker = new StatementWorker();
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

function writableWorker(): StatementWorker {
  const worker = new StatementWorker();
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
        // A write outside a transaction commits, and so crosses as an object.
        worker.respondStatement(
          message.id,
          sqlResult(
            {
              command,
              fields: returnsId ? ID_FIELD : [],
              revision,
              rowCount: isSelect || writes ? 1 : 0,
              rows: returnsId ? [{id: message.params.params[0] ?? 1}] : [],
              tables: writes ? ['posts'] : [],
              keys: {},
            },
            message.params.rowMode,
          ),
          writes && message.params.transactionId === undefined,
        );
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
        worker.respondStatement(
          message.id,
          sqlResult(
            {
              command,
              fields: isSelect ? ID_FIELD : [],
              revision,
              rowCount: 1,
              rows: isSelect ? [{id: message.params.params[0] ?? 1}] : [],
              tables: writes ? ['posts'] : [],
              keys: {},
            },
            message.params.rowMode,
          ),
          writes && message.params.transactionId === undefined,
        );
      } else if (message.method === 'closePrepared') {
        preparedSql.delete(message.params.statementId);
        respondOk(worker, message, undefined);
      } else if (message.method === 'execSql') {
        if (message.params.transactionId === undefined) {
          revision += 1;
        }
        respondOk(
          worker,
          message,
          [
            {
              command: 'CREATE',
              fields: [],
              revision,
              rowCount: 0,
              rows: [],
              tables: ['posts'],
              keys: {},
            },
            {
              command: 'INSERT',
              fields: [],
              revision,
              rowCount: 1,
              rows: [],
              tables: ['posts'],
              keys: {},
            },
            {
              command: 'SELECT',
              fields: ID_FIELD,
              revision,
              rowCount: 1,
              rows: [{id: 1}],
              tables: [],
              keys: {},
            },
          ].map((result) => sqlResult(result, message.params.rowMode)),
        );
      } else if (message.method === 'beginTransaction') {
        respondOk(worker, message, {
          transactionId: `tx-${nextTransactionId++}`,
        });
      } else if (message.method === 'commitTransaction') {
        revision += 1;
        respondOk(worker, message, {revision});
      } else if (message.method === 'rollbackTransaction') {
        respondOk(worker, message, undefined);
      } else if (message.method === 'close') {
        respondOk(worker, message, undefined);
      }
    });
  };
  return worker;
}

// A result as a Worker sends it, with its fields and rows as JSON text, and
// each row as an array when the request asked for them.
function sqlResult(
  {fields, rows, ...header}: {
    command: string;
    fields: {name: string; dataTypeID: number}[];
    revision: number;
    rowCount: number;
    rows: Record<string, unknown>[];
    tables: string[];
    keys: SqlResult['keys'];
  },
  rowMode?: RowMode,
): SqlResult {
  return {
    ...header,
    data: JSON.stringify({
      fields,
      rows:
        rowMode === 'array'
          ? rows.map((row) => fields.map(({name}) => row[name] ?? null))
          : rows,
    }),
  };
}

function respondOk(
  worker: StatementWorker,
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

type StatementMessage = Extract<
  WorkerRequest,
  {method: 'executeSql' | 'executePrepared'}
>;

// A Worker that answers everything but statements, which it holds for the
// test to answer in whatever order, and with whatever outcome, it needs.
function holdingWorker(): {
  worker: StatementWorker;
  statements: StatementMessage[];
} {
  const worker = new StatementWorker();
  const statements: StatementMessage[] = [];
  let revision = 0;
  let nextStatementId = 1;
  let nextTransactionId = 1;
  worker.onPost = (message) => {
    if (message.method === 'executeSql' || message.method === 'executePrepared') {
      statements.push(message);
      return;
    }
    queueMicrotask(() => {
      if (message.method === 'init') {
        respondOk(worker, message, {revision});
      } else if (message.method === 'prepareSql') {
        respondOk(worker, message, {statementId: nextStatementId++});
      } else if (message.method === 'beginTransaction') {
        respondOk(worker, message, {
          transactionId: `tx-${nextTransactionId++}`,
        });
      } else if (message.method === 'commitTransaction') {
        revision += 1;
        respondOk(worker, message, {revision});
      } else {
        respondOk(worker, message, undefined);
      }
    });
  };
  return {worker, statements};
}

// The same Worker, which holds each script it is sent as well.
function scriptHoldingWorker(): {
  worker: StatementWorker;
  statements: StatementMessage[];
  scripts: Extract<WorkerRequest, {method: 'execSql'}>[];
} {
  const held = holdingWorker();
  const scripts: Extract<WorkerRequest, {method: 'execSql'}>[] = [];
  const answer = held.worker.onPost!;
  held.worker.onPost = (message) => {
    if (message.method === 'execSql') {
      scripts.push(message);
    } else {
      answer(message);
    }
  };
  return {...held, scripts};
}

// The same Worker, which leaves the database opening until the test says it
// has opened.
function openingWorker(): {
  worker: StatementWorker;
  statements: StatementMessage[];
  opened: () => void;
} {
  const held = holdingWorker();
  let init: WorkerRequest | undefined;
  const answer = held.worker.onPost!;
  held.worker.onPost = (message) => {
    if (message.method === 'init') {
      init = message;
    } else {
      answer(message);
    }
  };
  return {...held, opened: () => respondOk(held.worker, init!, {revision: 0})};
}

function refuse(worker: StatementWorker, id: number, code: string): void {
  worker.respond({
    v: PROTOCOL_VERSION,
    id,
    ok: false,
    error: {code, message: `${code} refused the statement`},
  });
}

// A write's result, as the Worker reports one that published nothing.
function written(
  command: string,
  rowCount: number,
  tables: string[] = [],
  keys: SqlResult['keys'] = {},
  revision = 0,
): SqlResult {
  return {command, revision, rowCount, tables, keys, data: NO_ROWS};
}

// Lets every promise reaction already due, and those they make due, run.
const settled = (): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, 0));

// Everything about a value that its reader could tell apart: whether each
// object and array is a plain one, its own keys in their order and what kind
// of property each is, and each value, with -0 apart from 0. Two results are
// the same exactly when these are equal. toStrictEqual() cannot say so here,
// because it takes two objects to differ when their `constructor` does, which
// in a result's keys may be the name of a table or a column.
const anatomy = (value: unknown): unknown =>
  typeof value === 'object' && value !== null
    ? [
        Object.getPrototypeOf(value) ===
        (Array.isArray(value) ? Array.prototype : Object.prototype)
          ? Array.isArray(value)
            ? 'array'
            : 'object'
          : 'neither',
        Reflect.ownKeys(value).map((key) => {
          const {value: held, ...kind} = Object.getOwnPropertyDescriptor(
            value,
            key,
          )!;
          return [key, kind, anatomy(held)];
        }),
      ]
    : [typeof value, Object.is(value, -0) ? '-0' : value];

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
    const worker = new StatementWorker();
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

  it.each([
    ['WORKER_ERROR', (worker: StatementWorker) => worker.emitError('terminal crash')],
    ['WORKER_MESSAGE_ERROR', (worker: StatementWorker) => worker.emitMessageError()],
    ['PROTOCOL_MISMATCH', (worker: StatementWorker) => worker.emitInvalidMessage({invalid: true})],
  ] as const)('retains initialization state after %s until explicit cleanup', async (code, fail) => {
    const worker = writableWorker();
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts');
    worker.onPost = () => undefined;
    const pendingQuery = client.query('INSERT INTO posts VALUES (1)');
    const pendingExecution = statement.execute();
    const queryFailure = expect(pendingQuery).rejects.toMatchObject({code});
    const executionFailure = expect(pendingExecution).rejects.toMatchObject({code});
    await vi.waitFor(() => expect(worker.posted).toHaveLength(4));
    fail(worker);
    await Promise.all([queryFailure, executionFailure]);

    expect(worker.terminated).toBe(true);
    expect(client.ready).toBe(true);
    expect(client.closed).toBe(false);
    expect(statement.closed).toBe(false);
    await expect(client.waitReady).resolves.toBeUndefined();
    for (const operation of [
      client.query('SELECT id FROM posts'),
      client.exec('SELECT id FROM posts'),
      client.prepare('SELECT id FROM posts'),
      statement.execute(),
      client.transaction(() => undefined),
    ]) {
      await expect(operation).rejects.toMatchObject({code: 'WORKER_TERMINATED'});
    }

    const closing = client.close();
    expect(client.ready).toBe(false);
    expect(statement.closed).toBe(true);
    await expect(closing).rejects.toMatchObject({code: 'WORKER_TERMINATED'});
    expect(client.closed).toBe(true);
    await expect(client.query('SELECT id FROM posts')).rejects.toMatchObject({code: 'CLIENT_CLOSED'});
    await expect(statement.execute()).rejects.toMatchObject({code: 'CLIENT_CLOSED'});
    await expect(statement.close()).resolves.toBeUndefined();
    expect(worker.posted).toHaveLength(4);
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

    const ambiguousWorker = new StatementWorker();
    await expect(
      create('memory://', {
        dataDir: 'opfs://also-here',
        worker: ambiguousWorker,
      }),
    ).rejects.toThrow('either positionally or in options.dataDir');
    expect(ambiguousWorker.posted).toEqual([]);

    for (const dataDir of ['idb://database', 'opfs://', 'opfs://bad/name']) {
      const worker = new StatementWorker();
      await expect(create(dataDir, {worker})).rejects.toThrowError(TypeError);
      expect(worker.posted).toEqual([]);
    }

    for (const unsupported of [
      {storage: {kind: 'opfs', name: 'old-shape'}},
      {parsers: {}},
    ]) {
      const workerFactory = vi.fn(() => new StatementWorker());
      await expect(
        create({...unsupported, workerFactory} as never),
      ).rejects.toThrow('client options support only');
      expect(workerFactory).not.toHaveBeenCalled();
    }

    const inheritedOptionsFactory = vi.fn(() => new StatementWorker());
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
      const worker = new StatementWorker();
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
      keys: {},
    });
    expect(client.getRevision()).toBe(1);
    // A statement goes as one flat array: the version, its id, its operation,
    // its text, no transaction, object rows, and then its parameters.
    expect(worker.posted[1]).toEqual([
      PROTOCOL_VERSION,
      2,
      STATEMENT_SQL,
      'INSERT INTO posts (id) VALUES ($1) RETURNING id',
      0,
      0,
      7,
    ]);

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
      keys: {},
    });
    // The Worker writes array rows, so the request asks for them.
    expect(worker.posted[2]).toEqual([
      PROTOCOL_VERSION,
      3,
      STATEMENT_SQL,
      'SELECT id FROM posts',
      0,
      1,
    ]);
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
    expect(worker.posted[2]).toEqual([
      PROTOCOL_VERSION,
      3,
      STATEMENT_PREPARED,
      1,
      0,
      0,
      7,
    ]);

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

    const methods = worker.requests().map((request) => request.method);
    expect(methods).toContain('executePrepared');
    expect(methods.indexOf('executePrepared')).toBeLessThan(
      methods.indexOf('commitTransaction'),
    );
    // Inside a transaction, the statement's array names it.
    expect(worker.posted[methods.indexOf('executePrepared')]).toEqual([
      PROTOCOL_VERSION,
      expect.any(Number),
      STATEMENT_PREPARED,
      1,
      'tx-1',
      0,
      'transaction',
      1,
    ]);
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
    const worker = new StatementWorker();
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
    worker.respondStatement(
      execution!.id,
      sqlResult({
        command: 'SELECT',
        fields: ID_FIELD,
        revision: 0,
        rowCount: 1,
        rows: [{id: 4}],
        tables: [],
        keys: {},
      }),
    );
    await pending;
    await vi.waitFor(() => expect(statementClose).toBeDefined());
    respondOk(worker, statementClose!, undefined);
    await firstClose;
    await client.close();
  });

  it('finishes an accepted statement close before beginning a transaction', async () => {
    const worker = new StatementWorker();
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

    worker.respondStatement(
      execution!.id,
      sqlResult({
        command: 'SELECT',
        fields: ID_FIELD,
        revision: 0,
        rowCount: 1,
        rows: [{id: 1}],
        tables: [],
        keys: {},
      }),
    );
    await executionPromise;
    await vi.waitFor(() => expect(statementClose).toBeDefined());
    expect(begin).toBeUndefined();

    respondOk(worker, statementClose!, undefined);
    await closePromise;
    await vi.waitFor(() => expect(begin).toBeDefined());
    respondOk(worker, begin!, {transactionId: 'tx-1'});
    await vi.waitFor(() => expect(commit).toBeDefined());
    respondOk(worker, commit!, {revision: 0});
    await transactionPromise;
    expect(worker.requests().map((request) => request.method)).toEqual([
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
    const worker = new StatementWorker();
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
    respondOk(worker, commit!, {revision: 0});
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
      firstWorker
        .requests()
        .some((request) => request.method === 'closePrepared'),
    ).toBe(false);
    await secondClient.close();
  });

  it('refuses a closed statement in a transaction, with its client\'s error first', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    const closed = await client.prepare('SELECT id FROM posts');
    const open = await client.prepare('SELECT id FROM posts');
    await closed.close();

    const transaction = client.transaction(async (transaction) => {
      const posted = worker.posted.length;
      expect(() => transaction.execute(closed)).toThrowError(
        expect.objectContaining({code: 'PREPARED_STATEMENT_CLOSED'}),
      );
      expect(worker.posted).toHaveLength(posted);
      await expect(transaction.execute(open)).resolves.toMatchObject({
        command: 'SELECT',
      });

      // A client that closes seals the statements it still has. Either kind
      // is then refused for the client, before anything of its own.
      const closing = client.close();
      expect(open.closed).toBe(true);
      for (const statement of [open, closed]) {
        expect(() => transaction.execute(statement)).toThrowError(
          expect.objectContaining({code: 'CLIENT_CLOSED'}),
        );
        await expect(statement.execute()).rejects.toMatchObject({
          code: 'CLIENT_CLOSED',
        });
      }
      await closing;
    });
    // The callback's own checks passed: only its commit found the Worker gone.
    await expect(transaction).rejects.toMatchObject({code: 'WORKER_TERMINATED'});
    expect(
      worker.requests().filter((request) => request.method === 'executePrepared'),
    ).toHaveLength(1);
  });

  it('rejects a prepare result that races client close', async () => {
    const worker = new StatementWorker();
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
      worker.requests().some((request) => request.method === 'closePrepared'),
    ).toBe(false);
  });

  it('parameterizes sql tagged-template values without interpolation', async () => {
    const worker = writableWorker();
    const client = await create({worker});

    await client.sql<{id: number}>`SELECT id FROM posts WHERE id = ${7}`;
    expect(worker.posted[1]).toEqual([
      PROTOCOL_VERSION,
      2,
      STATEMENT_SQL,
      'SELECT id FROM posts WHERE id = $1',
      0,
      0,
      7,
    ]);
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
        keys: {},
      },
      {
        affectedRows: 1,
        command: 'INSERT',
        fields: [],
        revision: 1,
        rowCount: 1,
        rows: [],
        tables: ['posts'],
        keys: {},
      },
      {
        affectedRows: 0,
        command: 'SELECT',
        fields: ID_FIELD,
        revision: 1,
        rowCount: 1,
        rows: [{id: 1}],
        tables: [],
        keys: {},
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
    const transactionRequests = worker
      .requests()
      .filter(
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
      payload: {revision: 5, tables: ['posts'], keys: {}},
    });

    expect(posts).toHaveBeenCalledWith({revision: 5, tables: ['posts'], keys: {}});
    expect(users).not.toHaveBeenCalled();
    expect(client.getRevision()).toBe(5);
    await client.close();
  });

  it('unions changed keys across coalesced events', async () => {
    const worker = respondingWorker();
    const client = await create({worker});
    const listener = vi.fn();
    client.subscribe({}, listener);

    // Two events arriving before the listener runs must merge into one key set, and a key
    // touched by both must be reported once.
    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 5, tables: ['posts'], keys: {posts: [{id: 1}]}},
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {
        revision: 6,
        tables: ['posts', 'users'],
        keys: {posts: [{id: 1}, {id: 2}], users: [{id: 7}]},
      },
    });

    expect(listener).toHaveBeenLastCalledWith({
      revision: 6,
      tables: ['posts', 'users'],
      keys: {posts: [{id: 1}, {id: 2}], users: [{id: 7}]},
    });
    await client.close();
  });

  it('withholds a table whose keys any coalesced event could not report', async () => {
    const worker = respondingWorker();
    const client = await create({worker});
    const listener = vi.fn();
    client.subscribe({}, listener);

    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 5, tables: ['posts'], keys: {posts: [{id: 1}]}},
    });
    // A write too large to name its keys poisons the table for the whole coalesced event:
    // a subscriber must not read a partial list as a complete one.
    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 6, tables: ['posts'], keys: {}},
    });

    expect(listener).toHaveBeenLastCalledWith({
      revision: 6,
      tables: ['posts'],
      keys: {},
    });
    await client.close();
  });

  it('reports no keys alongside a reset, which requires a full re-query', async () => {
    const worker = respondingWorker();
    const client = await create({worker});
    const listener = vi.fn();
    client.subscribe({}, listener);

    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 5, tables: ['posts'], keys: {posts: [{id: 1}]}},
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'resync',
      payload: {revision: 6, tables: [], keys: {}},
    });

    expect(listener).toHaveBeenLastCalledWith({
      revision: 6,
      tables: [],
      keys: {},
      reset: true,
    });
    await client.close();
  });

  it('checks the database and reports the first problem found', async () => {
    const worker = writableWorker();
    const answer = worker.onPost!;
    let checks = 0;
    worker.onPost = (message) => {
      if (message.method !== 'check') return answer(message);
      queueMicrotask(() => {
        checks += 1;
        if (checks === 1) return respondOk(worker, message, undefined);
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: false,
          error: {code: 'STORAGE_CORRUPT', message: 'An index lost a row'},
        });
      });
    };
    const client = await create({worker});

    await expect(client.check()).resolves.toBeUndefined();
    await expect(client.check()).rejects.toMatchObject({
      code: 'STORAGE_CORRUPT',
      message: 'An index lost a row',
    });
    await client.transaction(async () => {
      await expect(client.check()).rejects.toMatchObject({
        code: 'TRANSACTION_ACTIVE',
      });
    });
    expect(checks).toBe(2);
    await client.close();
  });

  it('reads the schema the Worker returns, outside a transaction', async () => {
    const worker = writableWorker();
    const answer = worker.onPost!;
    const schema = {
      version: 2,
      tables: [
        {
          name: 'posts',
          columns: [
            {name: 'id', type: 'integer', nullable: false},
            {name: 'done', type: 'boolean', nullable: false, default: false},
          ],
          primaryKey: ['id'],
          indexes: [{name: 'posts_done', columns: ['done'], unique: false}],
          foreignKeys: [],
        },
      ],
    };
    worker.onPost = (message) => {
      if (message.method !== 'schema') return answer(message);
      queueMicrotask(() => respondOk(worker, message, schema));
    };
    const client = await create({worker});

    await expect(client.getSchema()).resolves.toEqual(schema);
    await client.transaction(async () => {
      await expect(client.getSchema()).rejects.toMatchObject({
        code: 'TRANSACTION_ACTIVE',
      });
    });
    await client.close();
  });

  it('sets the schema, dropping nothing unless asked, outside a transaction', async () => {
    const worker = writableWorker();
    const answer = worker.onPost!;
    const params: unknown[] = [];
    worker.onPost = (message) => {
      if (message.method !== 'setSchema') return answer(message);
      params.push(message.params);
      queueMicrotask(() => respondOk(worker, message, params.length === 1));
    };
    const client = await create({worker});
    const schema = {version: 1, tables: []};

    await expect(client.setSchema(schema)).resolves.toBe(true);
    await expect(client.setSchema(schema, {drop: true})).resolves.toBe(false);
    expect(params).toEqual([
      {schema, drop: false},
      {schema, drop: true},
    ]);
    await client.transaction(async () => {
      await expect(client.setSchema(schema)).rejects.toMatchObject({
        code: 'TRANSACTION_ACTIVE',
      });
    });
    await client.close();
  });

  it('makes concurrent close calls await the same worker cleanup', async () => {
    const worker = new StatementWorker();
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
  it('gives a flat response the Results that its result object gives', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const keyed = (text: string): SqlResult['keys'] =>
      JSON.parse(text) as SqlResult['keys'];
    const rows = (fields: unknown[], values: unknown[]): string =>
      JSON.stringify({fields, rows: values});
    // Each result is answered twice, once in each form a Worker may send it in.
    const cases: [name: string, result: SqlResult, columns?: string[]][] = [
      ['a write that changed nothing', written('INSERT', 0, [], {}, 3)],
      ['a write too large to name its keys', written('UPDATE', 1500, ['posts'])],
      [
        'a write that names its keys',
        written('DELETE', 4, ['posts'], {
          posts: [{id: 1}, {id: 'two'}, {id: null}, {id: true}],
        }),
      ],
      [
        'a write to a table with a composite key',
        written('UPDATE', 2, ['memberships'], {
          memberships: [
            {org: 'a', user: 1},
            {org: 'b', user: 2},
          ],
        }),
      ],
      [
        'a write with a fractional key at the last safe revision',
        written(
          'INSERT',
          1,
          ['readings'],
          {readings: [{at: -0.5}]},
          Number.MAX_SAFE_INTEGER,
        ),
      ],
      [
        'a write that reports keys but changed none',
        written('UPDATE', 0, ['posts'], {posts: []}),
        ['id'],
      ],
      [
        'a write to a table named __proto__',
        written('INSERT', 1, ['__proto__'], keyed('{"__proto__":[{"id":1}]}')),
      ],
      [
        'a write whose key has a column named __proto__',
        written(
          'UPDATE',
          2,
          ['posts'],
          keyed('{"posts":[{"__proto__":1,"id":2},{"__proto__":"x","id":3}]}'),
        ),
      ],
      [
        'a write to a table named as something every object inherits',
        written('DELETE', 1, ['constructor'], keyed('{"constructor":[{"id":1}]}')),
      ],
      [
        'a write whose key has columns named as things every object inherits',
        written(
          'UPDATE',
          2,
          ['toString'],
          keyed(
            '{"toString":[{"org":"a","constructor":1,"valueOf":2},{"org":"b","constructor":3,"valueOf":4}]}',
          ),
        ),
      ],
      [
        // The Worker lists a key's columns as the table declares them, and an
        // object lists those with numbers for names before the rest.
        'a write whose key has columns with numbers for names',
        written('INSERT', 1, ['grid'], keyed('{"grid":[{"y":1,"10":2,"2":3}]}')),
        ['y', '10', '2'],
      ],
      [
        'a read',
        {
          command: 'SELECT',
          revision: 2,
          rowCount: 2,
          tables: [],
          keys: {},
          data: rows(ID_FIELD, [{id: 1}, {id: 2}]),
        },
      ],
      [
        'a read of array rows',
        {
          command: 'SELECT',
          revision: 2,
          rowCount: 2,
          tables: [],
          keys: {},
          data: rows(ID_FIELD, [[1], [2]]),
        },
      ],
      [
        'a read that found nothing',
        {
          command: 'SELECT',
          revision: 2,
          rowCount: 0,
          tables: [],
          keys: {},
          data: rows(ID_FIELD, []),
        },
      ],
      [
        'a read whose count is not the number of its rows',
        {
          command: 'SELECT',
          revision: 2,
          rowCount: 7,
          tables: [],
          keys: {},
          data: rows(ID_FIELD, [{id: 1}]),
        },
      ],
      [
        'a read of no columns',
        {
          command: 'SELECT',
          revision: 2,
          rowCount: 0,
          tables: [],
          keys: {},
          data: NO_ROWS,
        },
      ],
    ];

    for (const [name, result, columns] of cases) {
      const asObject = client.query('SELECT 1');
      const asFlat = client.query('SELECT 1');
      const [first, second] = statements.splice(0);
      const flat = flatResponse(second!.id, result, columns);
      expect(flat, name).toBeDefined();
      respondOk(worker, first!, result);
      worker.respond(flat!);
      const [fromObject, fromFlat] = await Promise.all([asObject, asFlat]);

      // The same in every property and value, and in the order of every
      // object's keys.
      expect(anatomy(fromFlat), name).toEqual(anatomy(fromObject));
      expect(Object.keys(fromFlat), name).toEqual([
        'rows',
        'fields',
        'affectedRows',
        'command',
        'rowCount',
        'revision',
        'tables',
        'keys',
      ]);
      expect(Object.keys(fromFlat), name).toEqual(Object.keys(fromObject));
      expect(Object.keys(fromFlat.keys), name).toEqual(
        Object.keys(fromObject.keys),
      );
      expect(Object.getPrototypeOf(fromFlat.keys), name).toBe(Object.prototype);
      for (const table of Object.keys(fromObject.keys)) {
        expect(Object.hasOwn(fromFlat.keys, table), name).toBe(true);
        const flatRows = fromFlat.keys[table]!;
        fromObject.keys[table]!.forEach((row, index) => {
          expect(Object.keys(flatRows[index]!), name).toEqual(Object.keys(row));
          expect(Object.getPrototypeOf(flatRows[index]), name).toBe(
            Object.prototype,
          );
          for (const column of Object.keys(row)) {
            expect(Object.hasOwn(flatRows[index]!, column), name).toBe(true);
          }
        });
      }
      // Each call has arrays and objects of its own, as a parsed result has.
      expect(fromFlat.rows, name).not.toBe(fromObject.rows);
      expect(fromFlat.tables, name).not.toBe(fromObject.tables);
    }
    // Nothing was written through a prototype on the way.
    expect(Object.keys(Object.prototype)).toEqual([]);
    expect(Object.getPrototypeOf(Object.prototype)).toBeNull();
    await client.close();
  });

  it('sends array rows and a transaction with a flat statement, and reads its rows', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare<[number]>('SELECT id FROM posts');

    const outcome = client.transaction(async (transaction) => {
      const queried = transaction.query<[number]>('SELECT id FROM posts', [], {
        rowMode: 'array',
      });
      const executed = transaction.execute(statement, [5], {rowMode: 'array'});
      const tagged = transaction.sql<{id: number}>`SELECT id FROM posts WHERE id = ${6}`;
      return Promise.all([queried, executed, tagged]);
    });
    await vi.waitFor(() => expect(statements).toHaveLength(3));
    expect(worker.posted.slice(-3)).toEqual([
      [PROTOCOL_VERSION, 4, STATEMENT_SQL, 'SELECT id FROM posts', 'tx-1', 1],
      [PROTOCOL_VERSION, 5, STATEMENT_PREPARED, 1, 'tx-1', 1, 5],
      [
        PROTOCOL_VERSION,
        6,
        STATEMENT_SQL,
        'SELECT id FROM posts WHERE id = $1',
        'tx-1',
        0,
        6,
      ],
    ]);
    for (const [index, message] of statements.entries()) {
      worker.respondStatement(
        message.id,
        sqlResult(
          {
            command: 'SELECT',
            fields: ID_FIELD,
            revision: 0,
            rowCount: 1,
            rows: [{id: index}],
            tables: [],
            keys: {},
          },
          message.params.rowMode,
        ),
      );
    }

    const [queried, executed, tagged] = await outcome;
    expect(queried.rows).toEqual([[0]]);
    expect(executed.rows).toEqual([[1]]);
    expect(tagged.rows).toEqual([{id: 2}]);

    // An execution outside a transaction asks for array rows the same way.
    const direct = statement.execute([7], {rowMode: 'array'});
    expect(worker.posted.at(-1)).toEqual([
      PROTOCOL_VERSION,
      8,
      STATEMENT_PREPARED,
      1,
      0,
      1,
      7,
    ]);
    const asked = statements.at(-1)!;
    worker.respondStatement(
      asked.id,
      sqlResult(
        {
          command: 'SELECT',
          fields: ID_FIELD,
          revision: 0,
          rowCount: 1,
          rows: [{id: 7}],
          tables: [],
          keys: {},
        },
        asked.params.rowMode,
      ),
    );
    await expect(direct).resolves.toMatchObject({rows: [[7]]});
    await statement.close();
    await client.close();
  });

  it('follows the revision of a flat response, and of a commit', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    expect(client.getRevision()).toBe(0);

    const read = client.query('SELECT id FROM posts');
    worker.respond([PROTOCOL_VERSION, statements.shift()!.id, 3, 9, 0, NO_ROWS]);
    await read;
    expect(client.getRevision()).toBe(9);

    // A response that saw an earlier revision does not take it back.
    const stale = client.query('UPDATE posts SET id = 1 WHERE false');
    worker.respond([PROTOCOL_VERSION, statements.shift()!.id, 1, 4, 0]);
    await expect(stale).resolves.toMatchObject({revision: 4});
    expect(client.getRevision()).toBe(9);

    // A commit answers with the revision it published, and nothing else.
    worker.onPost = ((answer) => (message: WorkerRequest) => {
      if (message.method !== 'commitTransaction') return answer(message);
      queueMicrotask(() => respondOk(worker, message, {revision: 12}));
    })(worker.onPost!);
    await client.transaction(async (transaction) => {
      const staged = transaction.query('DELETE FROM posts');
      worker.respond([PROTOCOL_VERSION, statements.shift()!.id, 2, 9, 3, 'posts']);
      await staged;
      expect(client.getRevision()).toBe(9);
    });
    expect(client.getRevision()).toBe(12);
    await client.close();
  });

  it('fails alone a read whose rows its Worker wrote unreadable', async () => {
    // A Worker the application supplied is checked in full, rows included.
    const {worker, statements} = holdingWorker();
    const client = await create({worker});

    const unreadable = client.query('SELECT id FROM posts');
    const misshapen = client.query('SELECT id FROM posts');
    const readable = client.query('SELECT id FROM posts');
    const [first, second, third] = statements;
    worker.respond([PROTOCOL_VERSION, first!.id, 3, 0, 1, '{"fields":']);
    worker.respond([PROTOCOL_VERSION, second!.id, 3, 0, 1, '{"fields":[]}']);
    worker.respond([PROTOCOL_VERSION, third!.id, 3, 0, 0, NO_ROWS]);

    for (const failed of [unreadable, misshapen]) {
      await expect(failed).rejects.toMatchObject({
        code: 'BRIDGE_SERIALIZATION_ERROR',
        message: 'WASM returned an invalid structured SQL result',
      });
    }
    await expect(readable).resolves.toMatchObject({command: 'SELECT', rows: []});
    expect(worker.terminated).toBe(false);
    await client.close();
  });

  it('rejects a commit answered with more than its revision', async () => {
    const {worker} = holdingWorker();
    worker.onPost = ((answer) => (message: WorkerRequest) => {
      if (message.method !== 'commitTransaction') return answer(message);
      queueMicrotask(() =>
        respondOk(worker, message, {revision: 1, tables: ['posts'], keys: {}}),
      );
    })(worker.onPost!);
    const client = await create({worker});

    await expect(client.transaction(() => undefined)).rejects.toMatchObject({
      code: 'PROTOCOL_MISMATCH',
    });
    expect(worker.terminated).toBe(true);
    await client.close().catch(() => undefined);
  });

  it('has the Worker refuse a statement that is not text with an array of parameters', async () => {
    // A Worker that checks each request as TinyJoin's own does.
    const worker = writableWorker();
    worker.onPost = ((answer) => (message: WorkerRequest) => {
      const posted = worker.posted.at(-1);
      if (isStatementRequest(posted) || isWorkerRequest(posted)) {
        return answer(message);
      }
      queueMicrotask(() =>
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: false,
          error: {
            code: 'PROTOCOL_MISMATCH',
            message: 'The worker received an invalid TinyJoin protocol request',
          },
        }),
      );
    })(worker.onPost!);
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts WHERE id = $1');
    const refused = {
      code: 'PROTOCOL_MISMATCH',
      message: 'The worker received an invalid TinyJoin protocol request',
    };

    // None of these is a statement the page can lay out flat and have the
    // Worker take for another: each is refused, and the connection goes on.
    for (const params of ['ab', {length: 1, 0: 7}, null, 7, [1, undefined]]) {
      await expect(
        client.query('SELECT id FROM posts WHERE id = $1', params as never),
      ).rejects.toMatchObject(refused);
      await expect(statement.execute(params as never)).rejects.toMatchObject(
        refused,
      );
    }
    await expect(client.query(7 as never)).rejects.toMatchObject(refused);
    await expect(client.query(undefined as never)).rejects.toMatchObject(refused);
    await client.transaction(async (transaction) => {
      await expect(
        transaction.query('SELECT 1', 'ab' as never),
      ).rejects.toMatchObject(refused);
      await expect(
        transaction.execute(statement, {length: 0} as never),
      ).rejects.toMatchObject(refused);
    });
    await expect(statement.execute([7])).resolves.toMatchObject({rows: [{id: 7}]});
    expect(worker.terminated).toBe(false);
    await statement.close();
    await client.close();
  });

  it('sends a direct statement once the database is ready, and never to a busy one', async () => {
    const {worker, statements} = holdingWorker();
    const answerSelect = (): void => {
      const message = statements.shift()!;
      worker.respondStatement(
        message.id,
        sqlResult(
          {
            command: 'SELECT',
            fields: ID_FIELD,
            revision: 0,
            rowCount: 1,
            rows: [{id: 1}],
            tables: [],
            keys: {},
          },
          message.params.rowMode,
        ),
      );
    };
    const rowModeOnly = 'support only rowMode';

    // Before the database is ready, a statement waits for it, and a mistake
    // in its options is found only then.
    const client = new Client({worker});
    const early = client.query('SELECT id FROM posts');
    const mistaken = client.query('SELECT id FROM posts', [], {
      parsers: {},
    } as never);
    const earlyRows = client.query<[number]>('SELECT id FROM posts', [], {
      rowMode: 'array',
    });
    const refused = expect(mistaken).rejects.toThrow(rowModeOnly);
    expect(worker.requests().map((request) => request.method)).toEqual(['init']);
    await client.waitReady;
    await vi.waitFor(() => expect(statements).toHaveLength(2));
    // Each is then sent as it was asked for, in the order it was asked for.
    expect(worker.posted.slice(1)).toEqual([
      [PROTOCOL_VERSION, 2, STATEMENT_SQL, 'SELECT id FROM posts', 0, 0],
      [PROTOCOL_VERSION, 3, STATEMENT_SQL, 'SELECT id FROM posts', 0, 1],
    ]);
    answerSelect();
    answerSelect();
    await expect(early).resolves.toMatchObject({rows: [{id: 1}]});
    await expect(earlyRows).resolves.toMatchObject({rows: [[1]]});
    await refused;

    // Once it is ready, a statement is posted before query() returns.
    const prompt = client.query('SELECT id FROM posts');
    expect(statements).toHaveLength(1);
    answerSelect();
    await prompt;
    await expect(
      client.query('SELECT id FROM posts', [], {rowMode: 'rows'} as never),
    ).rejects.toThrow(rowModeOnly);

    // While a transaction is active, the client itself sends nothing.
    await client.transaction(async () => {
      const posted = worker.posted.length;
      await expect(client.query('SELECT id FROM posts')).rejects.toMatchObject({
        code: 'TRANSACTION_ACTIVE',
      });
      await expect(
        client.sql`SELECT id FROM posts WHERE id = ${1}`,
      ).rejects.toMatchObject({code: 'TRANSACTION_ACTIVE'});
      expect(worker.posted).toHaveLength(posted);
    });
    await client.close();
    await expect(client.query('SELECT id FROM posts')).rejects.toMatchObject({
      code: 'CLIENT_CLOSED',
    });
  });

  it('waits for every statement in flight before it commits', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');

    const transaction = client.transaction((tx) => {
      void tx.query('INSERT INTO posts VALUES (1)');
      void tx.execute(statement, [2]);
      void tx.query('INSERT INTO posts VALUES (3)');
    });
    await vi.waitFor(() => expect(statements).toHaveLength(3));
    await settled();
    expect(transactionMethods(worker)).not.toContain('commitTransaction');

    // In any order: the commit follows the last of them, and not before.
    worker.respondStatement(statements[2]!.id, written('INSERT', 1, ['posts']));
    worker.respondStatement(statements[0]!.id, written('INSERT', 1, ['posts']));
    await settled();
    expect(transactionMethods(worker)).not.toContain('commitTransaction');
    worker.respondStatement(statements[1]!.id, written('INSERT', 1, ['posts']));

    await transaction;
    expect(transactionMethods(worker).at(-1)).toBe('commitTransaction');
    await statement.close();
    await client.close();
  });

  it('rolls back at once when a statement in flight fails while its transaction settles', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');

    const transaction = client.transaction((tx) => {
      void tx.query('INSERT INTO posts VALUES (1)');
      void tx.execute(statement, [2]);
      void tx.query('INSERT INTO posts VALUES (3)');
    });
    const outcome = expect(transaction).rejects.toMatchObject({
      code: 'SECOND_FAILED',
    });
    await vi.waitFor(() => expect(statements).toHaveLength(3));
    await settled();
    expect(transactionMethods(worker)).not.toContain('rollbackTransaction');

    // The second fails while the first and third are still in flight. The
    // transaction does not wait for them: it rolls back, and fails with the
    // error of the statement that nobody awaited.
    const rejections = await unhandledRejections(async () => {
      refuse(worker, statements[1]!.id, 'SECOND_FAILED');
      await outcome;
      expect(transactionMethods(worker).at(-1)).toBe('rollbackTransaction');
      // What the others then do changes nothing, and raises nothing.
      refuse(worker, statements[0]!.id, 'FIRST_FAILED');
      worker.respondStatement(statements[2]!.id, written('INSERT', 1, ['posts']));
    });
    expect(rejections).toEqual([]);
    expect(transactionMethods(worker)).not.toContain('commitTransaction');
    await statement.close();
    await client.close();
  });

  it('leaves a statement that failed before its transaction settles to its caller', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');
    let finish: () => void = () => undefined;

    const rejections = await unhandledRejections(async () => {
      const transaction = client.transaction(async (tx) => {
        // Neither is awaited, and both fail while the callback is busy with
        // something else. The callback chose not to look, so the transaction
        // commits, as it does when a failure is caught.
        void tx.query('INSERT INTO posts VALUES (1)');
        void tx.execute(statement, [2]);
        await new Promise<void>((resolve) => {
          finish = resolve;
        });
      });
      await vi.waitFor(() => expect(statements).toHaveLength(2));
      refuse(worker, statements[0]!.id, 'FIRST_FAILED');
      refuse(worker, statements[1]!.id, 'SECOND_FAILED');
      await settled();
      finish();
      await transaction;
    });

    // A tracked statement never raises an unhandled rejection by itself.
    expect(rejections).toEqual([]);
    expect(transactionMethods(worker).at(-1)).toBe('commitTransaction');
    await statement.close();
    await client.close();
  });

  it('fails a transaction whose callback returns in the turn its statement failed', async () => {
    // This Worker answers within the microtask queue, as an in-process stand-in
    // may, so a failure can arrive after the callback has returned and before
    // the transaction has looked at what is in flight.
    const worker = writableWorker();
    const answer = worker.onPost!;
    worker.onPost = (message) => {
      if (message.method !== 'executeSql' && message.method !== 'executePrepared') {
        return answer(message);
      }
      const code =
        message.method === 'executeSql' ? 'QUERY_FAILED' : 'EXECUTION_FAILED';
      queueMicrotask(() => refuse(worker, message.id, code));
    };
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');
    const query = (tx: Transaction): void =>
      void tx.query('INSERT INTO posts VALUES (1)');
    const execution = (tx: Transaction): void =>
      void tx.execute(statement, [1]);

    // When two fail within the turn, the transaction fails with the first of
    // them.
    const runs: [run: (tx: Transaction) => void, code: string][] = [
      [query, 'QUERY_FAILED'],
      [execution, 'EXECUTION_FAILED'],
      [(tx) => (query(tx), execution(tx)), 'QUERY_FAILED'],
      [(tx) => (execution(tx), query(tx)), 'EXECUTION_FAILED'],
    ];
    for (const [run, code] of runs) {
      const rejections = await unhandledRejections(async () => {
        await expect(client.transaction(run)).rejects.toMatchObject({code});
      });
      expect(rejections).toEqual([]);
      expect(transactionMethods(worker).at(-1)).toBe('rollbackTransaction');
    }
    await statement.close();
    await client.close();
  });

  it('fails a transaction with the statement that failed within the turn, whenever the others did', async () => {
    // The first statement fails within the microtask queue, and the second
    // before its request has even been posted, as only a stand-in on the page
    // can answer. The second's turn ends before the transaction looks at what
    // is in flight, and the first's does not.
    const worker = writableWorker();
    const answer = worker.onPost!;
    worker.onPost = (message) => {
      if (message.method === 'executeSql') {
        queueMicrotask(() => refuse(worker, message.id, 'WITHIN_THE_TURN'));
      } else if (message.method === 'executePrepared') {
        refuse(worker, message.id, 'AT_ONCE');
      } else {
        answer(message);
      }
    };
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');

    const rejections = await unhandledRejections(async () => {
      await expect(
        client.transaction((tx) => {
          void tx.query('INSERT INTO posts VALUES (1)');
          void tx.execute(statement, [2]);
        }),
      ).rejects.toMatchObject({code: 'WITHIN_THE_TURN'});
      // A statement refused at once is past by the time its callback returns.
      await client.transaction((tx) => {
        void tx.execute(statement, [3]);
      });
    });

    expect(rejections).toEqual([]);
    expect(transactionMethods(worker).slice(-3)).toEqual([
      'beginTransaction',
      'executePrepared',
      'commitTransaction',
    ]);
    await statement.close();
    await client.close();
  });

  it('raises no unhandled rejection for a prepared execution that nobody awaits', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts WHERE id = $1');

    const rejections = await unhandledRejections(async () => {
      void statement.execute([1]);
      refuse(worker, statements[0]!.id, 'BIND_ERROR');
      await settled();
      // A direct query is not tracked, and is its caller's own to handle.
      void client.query('SELECT id FROM posts');
      refuse(worker, statements[1]!.id, 'DIRECT_FAILED');
    });

    expect(rejections).toMatchObject([{code: 'DIRECT_FAILED'}]);
    await statement.close();
    await client.close();
  });

  it('closes a statement only once its executions in flight have settled, whatever their outcome', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts WHERE id = $1');
    const closes = (): number =>
      worker.requests().filter((request) => request.method === 'closePrepared')
        .length;

    const failing = statement.execute([1]);
    const succeeding = statement.execute([2]);
    const failed = expect(failing).rejects.toMatchObject({code: 'BIND_ERROR'});
    const closing = statement.close();
    expect(statement.closed).toBe(true);
    await settled();
    expect(closes()).toBe(0);

    refuse(worker, statements[0]!.id, 'BIND_ERROR');
    await failed;
    await settled();
    expect(closes()).toBe(0);

    // Whoever awaits the last execution reads its result before the close
    // goes on to its request.
    let closesWhenRead = -1;
    void succeeding.then(() => {
      closesWhenRead = closes();
    });
    worker.respondStatement(
      statements[1]!.id,
      sqlResult({
        command: 'SELECT',
        fields: ID_FIELD,
        revision: 0,
        rowCount: 1,
        rows: [{id: 2}],
        tables: [],
        keys: {},
      }),
    );
    await succeeding;
    await closing;
    expect(closesWhenRead).toBe(0);
    expect(closes()).toBe(1);
    await client.close();
  });

  it('closes a statement only once an execution that outlived its transaction has settled', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');
    const closes = (): number =>
      worker.requests().filter((request) => request.method === 'closePrepared')
        .length;

    // The second execution fails and ends the transaction while the first is
    // still unanswered.
    let first: Promise<unknown> | undefined;
    await expect(
      client.transaction(async (tx) => {
        first = tx.execute(statement, [1]);
        const second = tx.execute(statement, [2]);
        refuse(worker, statements[1]!.id, 'CONSTRAINT');
        await second;
      }),
    ).rejects.toMatchObject({code: 'CONSTRAINT'});

    const closing = statement.close();
    await settled();
    expect(closes()).toBe(0);

    // Whoever awaits that execution reads its result before the close goes on
    // to its request, as with an execution outside any transaction.
    let closesWhenRead = -1;
    void first!.then(() => {
      closesWhenRead = closes();
    });
    worker.respondStatement(statements[0]!.id, written('INSERT', 1, ['posts']));
    await first;
    await closing;
    expect(closesWhenRead).toBe(0);
    expect(closes()).toBe(1);
    await client.close();
  });

  it('settles what waits on statements when their connection is lost', async () => {
    // A close that waits for an execution in flight.
    const closer = holdingWorker();
    const closingClient = await create({worker: closer.worker});
    const statement = await closingClient.prepare('SELECT id FROM posts');
    const execution = statement.execute();
    const closing = statement.close();
    // A transaction that waits for two statements nobody awaits.
    const settler = holdingWorker();
    const settlingClient = await create({worker: settler.worker});
    const other = await settlingClient.prepare('INSERT INTO posts VALUES ($1)');
    const transaction = settlingClient.transaction((tx) => {
      void tx.query('INSERT INTO posts VALUES (1)');
      void tx.execute(other, [2]);
    });
    await vi.waitFor(() => expect(settler.statements).toHaveLength(2));
    await settled();

    const rejections = await unhandledRejections(async () => {
      const outcomes = [
        expect(execution).rejects.toMatchObject({code: 'WORKER_ERROR'}),
        // The close goes on to its request, which a lost connection refuses.
        expect(closing).rejects.toMatchObject({code: 'WORKER_TERMINATED'}),
        expect(transaction).rejects.toMatchObject({
          code: 'WORKER_ERROR',
          message: 'settler crashed',
        }),
      ];
      closer.worker.emitError('closer crashed');
      settler.worker.emitError('settler crashed');
      await Promise.all(outcomes);
    });

    expect(rejections).toEqual([]);
    expect(statement.closed).toBe(true);
    expect(transactionMethods(settler.worker)).toEqual([
      'beginTransaction',
      'executeSql',
      'executePrepared',
    ]);
    await closingClient.close().catch(() => undefined);
    await settlingClient.close().catch(() => undefined);
  });

  it('waits for a script in flight before it commits', async () => {
    const {worker, statements, scripts} = scriptHoldingWorker();
    const client = await create({worker});

    // A script alone, which nobody awaits.
    const alone = client.transaction((tx) => {
      void tx.exec('INSERT INTO posts VALUES (1)');
    });
    await vi.waitFor(() => expect(scripts).toHaveLength(1));
    await settled();
    expect(transactionMethods(worker)).not.toContain('commitTransaction');
    respondOk(worker, scripts[0]!, []);
    await alone;
    expect(transactionMethods(worker).at(-1)).toBe('commitTransaction');

    // A script beside a statement, answered in either order: the commit waits
    // for whichever of them is answered last.
    for (const scriptFirst of [false, true]) {
      const beside = client.transaction((tx) => {
        void tx.exec('INSERT INTO posts VALUES (2)');
        void tx.query('INSERT INTO posts VALUES (3)');
      });
      await vi.waitFor(() => expect(statements).toHaveLength(1));
      await settled();
      const answers = [
        (): void =>
          worker.respondStatement(
            statements.shift()!.id,
            written('INSERT', 1, ['posts']),
          ),
        (): void => respondOk(worker, scripts.at(-1)!, []),
      ];
      if (scriptFirst) {
        answers.reverse();
      }
      answers[0]!();
      await settled();
      expect(transactionMethods(worker).at(-1)).toBe('executeSql');
      answers[1]!();
      await beside;
      expect(transactionMethods(worker).at(-1)).toBe('commitTransaction');
    }
    await client.close();
  });

  it('rolls back when a script in flight fails while its transaction settles', async () => {
    const {worker, statements, scripts} = scriptHoldingWorker();
    const client = await create({worker});

    const rejections = await unhandledRejections(async () => {
      // Alone, and then with a statement still in flight beside it.
      const alone = client.transaction((tx) => {
        void tx.exec('INSERT INTO posts VALUES (1)');
      });
      await vi.waitFor(() => expect(scripts).toHaveLength(1));
      await settled();
      refuse(worker, scripts[0]!.id, 'SCRIPT_FAILED');
      await expect(alone).rejects.toMatchObject({code: 'SCRIPT_FAILED'});
      expect(transactionMethods(worker).at(-1)).toBe('rollbackTransaction');

      const beside = client.transaction((tx) => {
        void tx.query('INSERT INTO posts VALUES (2)');
        void tx.exec('INSERT INTO posts VALUES (3)');
      });
      await vi.waitFor(() => expect(scripts).toHaveLength(2));
      await settled();
      refuse(worker, scripts[1]!.id, 'SCRIPT_FAILED');
      await expect(beside).rejects.toMatchObject({code: 'SCRIPT_FAILED'});
      expect(transactionMethods(worker).at(-1)).toBe('rollbackTransaction');
      // What the statement then does changes nothing, and raises nothing.
      worker.respondStatement(statements[0]!.id, written('INSERT', 1, ['posts']));
    });

    expect(rejections).toEqual([]);
    expect(transactionMethods(worker)).not.toContain('commitTransaction');
    await client.close();
  });

  it('refuses a statement while a rollback is in flight, and after one that failed', async () => {
    const {worker} = holdingWorker();
    const answer = worker.onPost!;
    let rollback: WorkerRequest | undefined;
    worker.onPost = (message) => {
      // The first rollback is held for the test to refuse.
      if (message.method === 'rollbackTransaction' && rollback === undefined) {
        rollback = message;
      } else {
        answer(message);
      }
    };
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');
    const closed = expect.objectContaining({code: 'TRANSACTION_CLOSED'});

    await expect(
      client.transaction(async (tx) => {
        const rolling = tx.rollback();
        const posted = worker.posted.length;
        const refusesEverything = (): void => {
          expect(() => tx.query('INSERT INTO posts VALUES (1)')).toThrowError(closed);
          expect(() => tx.execute(statement, [1])).toThrowError(closed);
          expect(() => tx.sql`INSERT INTO posts VALUES (${1})`).toThrowError(closed);
          expect(() => tx.exec('INSERT INTO posts VALUES (1)')).toThrowError(closed);
          expect(() => tx.rollback()).toThrowError(closed);
        };

        // The rollback has been asked for and not answered. The transaction
        // is not closed yet, and takes nothing more.
        expect(tx.closed).toBe(false);
        refusesEverything();
        refuse(worker, rollback!.id, 'ROLLBACK_FAILED');
        await rolling.catch(() => undefined);
        // It failed, and the transaction takes nothing more all the same.
        refusesEverything();
        expect(worker.posted).toHaveLength(posted);
      }),
    ).rejects.toMatchObject({code: 'ROLLBACK_FAILED'});
    await statement.close();
    await client.close();
  });

  it('sends a statement that waited for the database, whatever began while it waited', async () => {
    // A statement asked for while the database is opening waits for it. Its
    // wait ends with the check that lets it go, and it is sent a turn later
    // without another look, as every request that waited is. A transaction
    // may have begun in that turn. The statement is sent behind the
    // transaction's own request, and what the Worker makes of it there is
    // the Worker's to say.
    const busy = openingWorker();
    const client = new Client({worker: busy.worker});
    let finish: () => void = () => undefined;
    const transaction = client.transaction(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
    );
    await settled();
    const waited = client.query('SELECT id FROM posts');
    busy.opened();
    await vi.waitFor(() => expect(busy.statements).toHaveLength(1));
    expect(transactionMethods(busy.worker)).toEqual([
      'beginTransaction',
      'executeSql',
    ]);
    refuse(busy.worker, busy.statements[0]!.id, 'TRANSACTION_ACTIVE');
    await expect(waited).rejects.toMatchObject({
      code: 'TRANSACTION_ACTIVE',
      message: 'TRANSACTION_ACTIVE refused the statement',
    });
    finish();
    await transaction;
    await client.close();

    // The client may have been closed in that turn instead. The statement
    // was asked for first, so it is sent first, and runs. This Worker answers
    // each request in the order it arrived, as a Worker does.
    const closing = openingWorker();
    const hold = closing.worker.onPost!;
    closing.worker.onPost = (message) => {
      if (message.method !== 'executeSql') {
        return hold(message);
      }
      queueMicrotask(() =>
        closing.worker.respondStatement(message.id, {
          command: 'SELECT',
          revision: 0,
          rowCount: 0,
          tables: [],
          keys: {},
          data: NO_ROWS,
        }),
      );
    };
    const closed = new Client({worker: closing.worker});
    const last = closed.query('SELECT id FROM posts');
    const done = closed.waitReady.then(() => closed.close());
    closing.opened();
    await expect(last).resolves.toMatchObject({command: 'SELECT', rows: []});
    await done;
    expect(closing.worker.requests().map((request) => request.method)).toEqual([
      'init',
      'executeSql',
      'close',
    ]);
  });

  it('reads a changed key under a name that an object inherits and may not assign', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    // For the length of this test, Object.prototype holds a property that may
    // not be assigned, as each of its properties is on a page that has frozen
    // it, and two that are computed, as a library may define them. Assigning
    // such a name to an object throws, or leaves the object as it was, where
    // parsing JSON defines the property. Nothing is asserted until they are
    // gone, since the test runner's own objects inherit them too.
    const prototype = Object.prototype as Record<string, unknown>;
    const names = ['a frozen name', 'a computed name', 'an observed name'];
    const observed: unknown[] = [];
    const restore = (): void => {
      for (const name of names) {
        delete prototype[name];
      }
    };
    // Whatever becomes of this test, nothing it defines outlives it.
    onTestFinished(restore);
    Object.defineProperty(prototype, names[0]!, {
      value: 'inherited',
      writable: false,
      configurable: true,
    });
    Object.defineProperty(prototype, names[1]!, {
      get: () => 'inherited',
      configurable: true,
    });
    Object.defineProperty(prototype, names[2]!, {
      get: () => 'inherited',
      set: (value: unknown) => observed.push(value),
      configurable: true,
    });
    const read: [
      table: string,
      column: string,
      fromFlat: Results,
      fromObject: Results,
    ][] = [];
    try {
      for (const name of names) {
        // As a column of the key, after another, and as the table.
        for (const [table, column] of [
          ['posts', name],
          [name, 'tag'],
        ] as const) {
          const result = written('UPDATE', 1, [table], {
            [table]: [{id: 7, [column]: 'eight'}],
          });
          const asFlat = client.query('UPDATE posts SET tag = $1', ['eight']);
          const asObject = client.query('UPDATE posts SET tag = $1', ['eight']);
          const [first, second] = statements.splice(0);
          worker.respond(flatResponse(first!.id, result)!);
          respondOk(worker, second!, result);
          read.push([table, column, ...(await Promise.all([asFlat, asObject]))]);
        }
      }
    } finally {
      restore();
    }

    expect(read).toHaveLength(6);
    for (const [table, column, fromFlat, fromObject] of read) {
      expect(anatomy(fromFlat), `${table}.${column}`).toEqual(
        anatomy(fromObject),
      );
      // Each is a property of the object's own, as any other name is.
      expect(Object.getOwnPropertyDescriptor(fromFlat.keys, table)).toEqual({
        value: [{id: 7, [column]: 'eight'}],
        writable: true,
        enumerable: true,
        configurable: true,
      });
      const [row] = fromFlat.keys[table]!;
      expect(Object.keys(row!)).toEqual(['id', column]);
      expect(Object.getOwnPropertyDescriptor(row, column)).toEqual({
        value: 'eight',
        writable: true,
        enumerable: true,
        configurable: true,
      });
    }
    // No setter that the names inherit was called on the way.
    expect(observed).toEqual([]);
    await client.close();
  });

  it('settles a transaction, and then a close, whose reads had rows it could not read', async () => {
    // A Worker the application supplied is checked in full, rows included.
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts');

    const rejections = await unhandledRejections(async () => {
      const transaction = client.transaction((tx) => {
        void tx.query('SELECT id FROM posts');
        void tx.execute(statement);
      });
      const outcome = expect(transaction).rejects.toMatchObject({
        code: 'BRIDGE_SERIALIZATION_ERROR',
      });
      await vi.waitFor(() => expect(statements).toHaveLength(2));
      await settled();
      for (const message of statements) {
        worker.respond([PROTOCOL_VERSION, message.id, 3, 0, 1, '{"fields":']);
      }
      await outcome;
    });

    // Each failed alone, and was counted as settled all the same: the
    // transaction rolled back, and the statement's close waits for nothing.
    expect(rejections).toEqual([]);
    expect(transactionMethods(worker).at(-1)).toBe('rollbackTransaction');
    await statement.close();
    expect(worker.requests().at(-1)!.method).toBe('closePrepared');
    expect(worker.terminated).toBe(false);
    await client.close();
  });

  it('refuses a mistake in the options of any statement, and counts none of them in flight', async () => {
    const {worker} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts');
    const posted = worker.posted.length;
    const rowModeOnly = 'support only rowMode';
    const mistakes = [{parsers: {}}, {rowMode: 'rows'}, null] as never[];

    for (const mistaken of mistakes) {
      await expect(statement.execute([], mistaken)).rejects.toThrow(rowModeOnly);
      await client.transaction((tx) => {
        expect(() => tx.query('SELECT 1', [], mistaken)).toThrow(rowModeOnly);
        expect(() => tx.execute(statement, [], mistaken)).toThrow(rowModeOnly);
      });
    }

    // Nothing was sent for any of them, and nothing waits for one: each
    // transaction went on to commit, and the statement's close to its request.
    expect(
      worker
        .requests()
        .slice(posted)
        .map((request) => request.method),
    ).toEqual(mistakes.flatMap(() => ['beginTransaction', 'commitTransaction']));
    await statement.close();
    expect(worker.requests().at(-1)!.method).toBe('closePrepared');
    await client.close();
  });

  it('checks its client again once the options of a prepared execution have been read', async () => {
    const worker = writableWorker();
    const client = await create({worker});
    const statement = await client.prepare('SELECT id FROM posts');
    const posted = worker.posted.length;

    // Reading the options runs the caller's code, which here closes the
    // client. The execution is refused for that, and is not sent.
    let closing: Promise<void> | undefined;
    await expect(
      statement.execute([], {
        get rowMode() {
          closing ??= client.close();
          return 'array' as const;
        },
      }),
    ).rejects.toMatchObject({code: 'CLIENT_CLOSED'});
    await closing;

    expect(
      worker
        .requests()
        .slice(posted)
        .map((request) => request.method),
    ).toEqual(['close']);
  });

  it('sends more parameters than a statement takes in its request object, and counts it as any other', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('INSERT INTO posts VALUES ($1)');
    // One more than a statement can refer to, which its engine refuses.
    const tooMany = Array.from({length: 1025}, (_, index) => index);

    // Outside a transaction, the request is the one such a statement always
    // had: its text and its parameters, and nothing else.
    const direct = client.query('INSERT INTO posts VALUES ($1)', tooMany);
    expect(worker.posted.at(-1)).toStrictEqual({
      v: PROTOCOL_VERSION,
      id: 3,
      method: 'executeSql',
      params: {sql: 'INSERT INTO posts VALUES ($1)', params: tooMany},
    });
    refuse(worker, statements.shift()!.id, 'INVALID_QUERY');
    await expect(direct).rejects.toMatchObject({code: 'INVALID_QUERY'});

    // Inside one, it names the transaction, and array rows when they are
    // asked for. Nobody awaits it here, so its failure is the transaction's.
    const rejections = await unhandledRejections(async () => {
      const transaction = client.transaction((tx) => {
        void tx.execute(statement, tooMany, {rowMode: 'array'});
      });
      await vi.waitFor(() => expect(statements).toHaveLength(1));
      expect(worker.posted.at(-1)).toStrictEqual({
        v: PROTOCOL_VERSION,
        id: 5,
        method: 'executePrepared',
        params: {
          statementId: 1,
          params: tooMany,
          transactionId: 'tx-1',
          rowMode: 'array',
        },
      });
      await settled();
      refuse(worker, statements.shift()!.id, 'BIND_ERROR');
      await expect(transaction).rejects.toMatchObject({code: 'BIND_ERROR'});
    });

    expect(rejections).toEqual([]);
    expect(transactionMethods(worker).at(-1)).toBe('rollbackTransaction');
    // It was counted as settled, so the statement's close waits for nothing.
    await statement.close();
    expect(worker.requests().at(-1)!.method).toBe('closePrepared');
    await client.close();
  });

  it('attaches no reaction to a statement that it counts in flight', async () => {
    const {worker, statements} = holdingWorker();
    const client = await create({worker});
    const statement = await client.prepare('UPDATE posts SET id = $1');
    // Answers each statement held as an UPDATE of one row, whose table and key
    // follow, or of none.
    const answer = (...changed: (string | number)[]): void => {
      for (const message of statements.splice(0)) {
        worker.respond([
          PROTOCOL_VERSION,
          message.id,
          1,
          0,
          changed.length > 0 ? 1 : 0,
          ...changed,
        ]);
      }
    };

    // A transaction's statements, by their text or prepared, and a prepared
    // statement's execution outside one, are each sent, answered and counted
    // as settled with nothing attached to their promises. Following every
    // promise instead, as was once done, costs a reaction for each statement.
    let attached = -1;
    await client.transaction(async (tx) => {
      let sent: Promise<unknown>[] = [];
      attached = reactions(() => {
        sent = [
          tx.execute(statement, [1]),
          tx.query('UPDATE posts SET id = $1', [2]),
          tx.sql`UPDATE posts SET id = ${3}`,
        ];
        answer('posts', 1, 'id', 1);
      });
      await Promise.all(sent);
    });
    expect(attached).toBe(0);

    let direct: Promise<unknown> | undefined;
    expect(
      reactions(() => {
        direct = statement.execute([4]);
        answer();
      }),
    ).toBe(0);
    await expect(direct).resolves.toMatchObject({command: 'UPDATE', rowCount: 0});
    await statement.close();
    await client.close();
  });

  it('counts a statement once, whatever its Worker\'s stand-in does as it is posted', async () => {
    // Only a stand-in on the page can answer a statement before its
    // postMessage returns, and only a broken one then throws, or throws what
    // no message can be made of. Either way the statement settles once, as a
    // promise does, and the transaction still knows what it has in flight.
    const {worker, statements} = holdingWorker();
    const hold = worker.onPost!;
    let posting: 'held' | 'answered, then thrown' | 'thrown, past describing' =
      'held';
    worker.onPost = (message) => {
      if (message.method !== 'executeSql' || posting === 'held') {
        return hold(message);
      }
      if (posting === 'thrown, past describing') {
        throw Object.create(null);
      }
      worker.respondStatement(message.id, written('INSERT', 1, ['posts']));
      throw new Error('thrown after it answered');
    };
    const client = await create({worker});

    const rejections = await unhandledRejections(async () => {
      const transaction = client.transaction(async (tx) => {
        void tx.query('INSERT INTO posts VALUES (1)');
        posting = 'answered, then thrown';
        await expect(
          tx.query('INSERT INTO posts VALUES (2)'),
        ).resolves.toMatchObject({command: 'INSERT', rowCount: 1});
        posting = 'thrown, past describing';
        await expect(
          tx.query('INSERT INTO posts VALUES (3)'),
        ).rejects.toBeInstanceOf(TypeError);
        posting = 'held';
      });
      const outcome = expect(transaction).rejects.toMatchObject({
        code: 'FIRST_FAILED',
      });
      await vi.waitFor(() => expect(statements).toHaveLength(1));
      await settled();
      // The first statement is still in flight, and the transaction waits.
      expect(transactionMethods(worker)).not.toContain('commitTransaction');
      expect(transactionMethods(worker)).not.toContain('rollbackTransaction');
      refuse(worker, statements[0]!.id, 'FIRST_FAILED');
      await outcome;
    });

    expect(rejections).toEqual([]);
    expect(transactionMethods(worker).at(-1)).toBe('rollbackTransaction');
    await client.close();
  });
});

describe('Client over the Worker that TinyJoin ships', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('reads a flat response it has checked only in its fixed slots', async () => {
    // create() without a Worker constructs the packaged one, whose results the
    // page trusts beyond their envelope. This stands in for it, and for the
    // short-lived Worker that warms the engine up, which is sent no request.
    const statements: StatementMessage[] = [];
    const databases: StatementWorker[] = [];
    vi.stubGlobal(
      'Worker',
      class extends StatementWorker {
        constructor() {
          super();
          this.onPost = (message) => {
            if (message.method === 'init') {
              databases.push(this);
              queueMicrotask(() => respondOk(this, message, {revision: 0}));
            } else if (message.method === 'executeSql') {
              statements.push(message);
            } else if (message.method === 'close') {
              queueMicrotask(() => respondOk(this, message, undefined));
            }
          };
        }
      },
    );
    const client = await create();
    const [worker] = databases;
    const unreadable = {
      code: 'PROTOCOL_MISMATCH',
      message:
        'The TinyJoin worker returned an invalid result for the requested operation',
    };

    // Rows that cannot be parsed fail the statement that asked for them, in
    // either form, and nothing else. So does text that is JSON, and is no
    // object holding an array of fields and an array of rows.
    for (const data of [
      'not JSON',
      'null',
      '5',
      '"rows"',
      '[]',
      '[[],[]]',
      '{}',
      '{"fields":[]}',
      '{"fields":[],"rows":{}}',
      '{"fields":{},"rows":[]}',
    ]) {
      const flat = client.query('SELECT id FROM posts');
      const object = client.query('SELECT id FROM posts');
      const [first, second] = statements.splice(0);
      worker!.respond([PROTOCOL_VERSION, first!.id, 3, 0, 1, data]);
      respondOk(worker!, second!, {
        command: 'SELECT',
        revision: 0,
        rowCount: 1,
        tables: [],
        keys: {},
        data,
      });
      await expect(flat, data).rejects.toMatchObject(unreadable);
      await expect(object, data).rejects.toMatchObject(unreadable);
    }
    expect(worker!.terminated).toBe(false);

    // The keys of a write are taken as the Worker wrote them.
    const write = client.query('UPDATE posts SET id = $1 WHERE false', [1]);
    worker!.respond([
      PROTOCOL_VERSION,
      statements.shift()!.id,
      1,
      0,
      2,
      'posts',
      1,
      'id',
      5,
      6,
    ]);
    await expect(write).resolves.toEqual({
      rows: [],
      fields: [],
      affectedRows: 2,
      command: 'UPDATE',
      rowCount: 2,
      revision: 0,
      tables: ['posts'],
      keys: {posts: [{id: 5}, {id: 6}]},
    });

    // A response whose fixed slots are wrong is no response at all.
    const mistaken = client.query('SELECT id FROM posts');
    worker!.respond([
      PROTOCOL_VERSION,
      statements.shift()!.id,
      3,
      0,
      -1,
      NO_ROWS,
    ] as never);
    await expect(mistaken).rejects.toMatchObject(unreadable);
    expect(worker!.terminated).toBe(true);
    await client.close().catch(() => undefined);
  });
});

function transactionMethods(worker: StatementWorker): string[] {
  return worker
    .requests()
    .filter((message) =>
      [
        'beginTransaction',
        'executeSql',
        'executePrepared',
        'execSql',
        'commitTransaction',
        'rollbackTransaction',
      ].includes(message.method),
    )
    .map((message) => message.method);
}
