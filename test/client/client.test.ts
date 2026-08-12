import {describe, expect, it, vi} from 'vitest';

import {Client} from '../../src/client/client.ts';
import {ClientError} from '../../src/client/error.ts';
import {PROTOCOL_VERSION, type WorkerRequest} from '../../src/protocol.ts';
import {FakeWorker} from '../helpers/fake-worker.ts';

function respondingWorker(): FakeWorker {
  const worker = new FakeWorker();
  worker.onPost = (message) => {
    queueMicrotask(() => {
      if (message.method === 'init') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: {
            revision: 0,
            sourceConfigured: message.params.source !== undefined,
          },
        });
      } else if (message.method === 'query') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: {revision: 2, rows: [{id: 1, title: 'hello'}]},
        });
      } else if (message.method === 'close') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: undefined,
        });
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
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: {revision, sourceConfigured: false},
        });
      } else if (message.method === 'executeSql') {
        const command = message.params.sql
          .trim()
          .split(/\s+/, 1)[0]!
          .toUpperCase();
        if (message.params.transactionId === undefined) {
          revision += 1;
        }
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: {
            command,
            revision,
            rowCount: 1,
            rows: [{id: message.params.params[0] ?? 1}],
            tables: ['posts'],
          },
        });
      } else if (message.method === 'beginTransaction') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: {transactionId: `tx-${nextTransactionId++}`},
        });
      } else if (message.method === 'commitTransaction') {
        revision += 1;
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: {revision, tables: ['posts']},
        });
      } else if (message.method === 'rollbackTransaction') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: undefined,
        });
      } else if (message.method === 'querySql' || message.method === 'query') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: {revision, rows: [{id: 1, title: 'hello'}]},
        });
      } else if (message.method === 'close') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: undefined,
        });
      }
    });
  };
  return worker;
}

describe('Client', () => {
  it('selects memory by default and forwards explicit OPFS storage', async () => {
    const memoryWorker = respondingWorker();
    const memoryClient = new Client({worker: memoryWorker});
    await memoryClient.ready();
    expect(
      (memoryWorker.posted[0] as Extract<WorkerRequest, {method: 'init'}>)
        .params.storage,
    ).toEqual({kind: 'memory'});

    const opfsWorker = respondingWorker();
    const opfsClient = new Client({
      worker: opfsWorker,
      storage: {kind: 'opfs', name: 'application-cache'},
    });
    await opfsClient.ready();
    expect(
      (opfsWorker.posted[0] as Extract<WorkerRequest, {method: 'init'}>).params
        .storage,
    ).toEqual({kind: 'opfs', name: 'application-cache'});

    await memoryClient.close();
    await opfsClient.close();
  });

  it('forwards a serializable built-in source descriptor during init', async () => {
    const worker = respondingWorker();
    const source = {
      kind: 'supabase' as const,
      url: 'https://example.supabase.co',
      publishableKey: 'sb_publishable_example',
      tables: [{table: 'posts', primaryKey: ['id']}],
    };
    const client = new Client({worker, source});
    await client.ready();

    const init = worker.posted[0] as Extract<WorkerRequest, {method: 'init'}>;
    expect(init.params.source).toEqual(source);
    expect(structuredClone(init.params.source)).toEqual(source);
    await client.close();
  });

  it.each([
    {revision: 0},
    {revision: 0, sourceConfigured: 'yes'},
    {revision: -1, sourceConfigured: false},
  ])('rejects an invalid v3 initialization result', async (result) => {
    const worker = new FakeWorker();
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() =>
          worker.respond({
            v: PROTOCOL_VERSION,
            id: message.id,
            ok: true,
            result,
          }),
        );
      }
    };
    const client = new Client({worker});

    await expect(client.ready()).rejects.toMatchObject({
      name: 'ClientError',
      code: 'PROTOCOL_MISMATCH',
    });
    expect(worker.terminated).toBe(true);
  });

  it('supports an awaitable Supabase-style query chain', async () => {
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

  it('executes one atomic writable SQL statement and tracks its revision', async () => {
    const worker = writableWorker();
    const client = new Client({worker});

    await expect(
      client.exec<{id: number}>(
        'INSERT INTO posts (id) VALUES ($1) RETURNING id',
        [7],
      ),
    ).resolves.toEqual({
      command: 'INSERT',
      revision: 1,
      rowCount: 1,
      rows: [{id: 7}],
      tables: ['posts'],
    });
    expect(client.getRevision()).toBe(1);
    const requests = worker.posted as WorkerRequest[];
    expect(
      requests.find(
        (message): message is Extract<WorkerRequest, {method: 'executeSql'}> =>
          message.method === 'executeSql',
      )?.params,
    ).toEqual({
      sql: 'INSERT INTO posts (id) VALUES ($1) RETURNING id',
      params: [7],
    });
    await client.close();
  });

  it('scopes transaction operations to one token and commits after pending work', async () => {
    const worker = writableWorker();
    const client = new Client({worker});

    const value = await client.transaction(async (transaction) => {
      const pendingInsert = transaction.exec(
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
    expect(
      (transactionRequests[2] as Extract<WorkerRequest, {method: 'query'}>)
        .params.transactionId,
    ).toBe('tx-1');
    expect(
      (
        transactionRequests[3] as Extract<
          WorkerRequest,
          {method: 'commitTransaction'}
        >
      ).params.transactionId,
    ).toBe('tx-1');
    expect(client.getRevision()).toBe(1);
    await client.close();
  });

  it('rolls a transaction back when its callback fails', async () => {
    const worker = writableWorker();
    const client = new Client({worker});
    const failure = new Error('application failed');

    await expect(
      client.transaction(async (transaction) => {
        await transaction.exec('DELETE FROM posts WHERE id = $1', [1]);
        throw failure;
      }),
    ).rejects.toBe(failure);
    expect(
      (worker.posted as WorkerRequest[])
        .filter((message) =>
          [
            'beginTransaction',
            'executeSql',
            'commitTransaction',
            'rollbackTransaction',
          ].includes(message.method),
        )
        .map((message) => message.method),
    ).toEqual(['beginTransaction', 'executeSql', 'rollbackTransaction']);
    expect(client.getRevision()).toBe(0);
    await client.close();
  });

  it('serializes transaction callbacks and rejects use outside their lifetime', async () => {
    const worker = writableWorker();
    const client = new Client({worker});
    const order: string[] = [];
    let escaped:
      | Parameters<Parameters<Client['transaction']>[0]>[0]
      | undefined;

    await Promise.all([
      client.transaction(async (transaction) => {
        escaped = transaction;
        order.push('first-start');
        await transaction.exec('INSERT INTO posts (id) VALUES (1)');
        order.push('first-end');
      }),
      client.transaction(async (transaction) => {
        order.push('second-start');
        await transaction.exec('INSERT INTO posts (id) VALUES (2)');
        order.push('second-end');
      }),
    ]);

    expect(order).toEqual([
      'first-start',
      'first-end',
      'second-start',
      'second-end',
    ]);
    expect(() => escaped!.query('SELECT * FROM posts')).toThrowError(
      expect.objectContaining({code: 'TRANSACTION_CLOSED'}),
    );
    expect(() => client.transaction(undefined as never)).toThrowError(
      'TinyGres transaction requires a callback',
    );
    await client.close();
  });

  it('rejects local writes when initialization reports a configured source', async () => {
    const worker = respondingWorker();
    const client = new Client({
      worker,
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });
    await client.ready();

    await expect(
      client.exec('INSERT INTO posts (id) VALUES (1)'),
    ).rejects.toMatchObject({
      name: 'ClientError',
      code: 'SOURCE_DATABASE_READ_ONLY',
    });
    await expect(client.transaction(() => undefined)).rejects.toMatchObject({
      name: 'ClientError',
      code: 'SOURCE_DATABASE_READ_ONLY',
    });
    expect(
      (worker.posted as WorkerRequest[]).some((message) =>
        ['executeSql', 'beginTransaction'].includes(message.method),
      ),
    ).toBe(false);
    await client.close();
  });

  it('notifies only subscriptions affected by a worker mutation', async () => {
    const worker = respondingWorker();
    const client = new Client({worker});
    await client.ready();
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
  });

  it('replays, caches, and waits for the current live sync state', async () => {
    const worker = respondingWorker();
    const client = new Client({
      worker,
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });
    const listener = vi.fn();
    client.subscribeToSyncState(listener);
    expect(listener).toHaveBeenCalledWith({phase: 'idle'});
    await client.ready();

    const synced = client.whenSynced();
    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {
        phase: 'live-best-effort',
        sourceId: 'supabase:test',
        lastReconciledAt: '2026-08-11T00:00:00.000Z',
      },
    });

    await expect(synced).resolves.toEqual({
      phase: 'live-best-effort',
      sourceId: 'supabase:test',
      lastReconciledAt: '2026-08-11T00:00:00.000Z',
    });
    expect(client.getSyncState()).toEqual({
      phase: 'live-best-effort',
      sourceId: 'supabase:test',
      lastReconciledAt: '2026-08-11T00:00:00.000Z',
    });
    await client.close();
  });

  it('settles internal sync waiters before calling user listeners', async () => {
    const worker = respondingWorker();
    const client = new Client({
      worker,
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });
    await client.ready();
    client.subscribeToSyncState((state) => {
      if (state.phase === 'live-best-effort') {
        throw new Error('listener failed');
      }
    });
    const synced = client.whenSynced();

    expect(() =>
      worker.respond({
        v: PROTOCOL_VERSION,
        event: 'syncStateChanged',
        payload: {phase: 'live-best-effort'},
      }),
    ).toThrow('listener failed');
    await expect(synced).resolves.toMatchObject({phase: 'live-best-effort'});
    await client.close();
  });

  it('recognizes a source owned by an app-provided worker', async () => {
    const worker = new FakeWorker();
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() =>
          worker.respond({
            v: PROTOCOL_VERSION,
            id: message.id,
            ok: true,
            result: {revision: 0, sourceConfigured: true},
          }),
        );
      } else if (message.method === 'close') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: undefined,
        });
      }
    };
    const client = new Client({worker});
    await client.ready();
    const synced = client.whenSynced();
    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {phase: 'live-best-effort', sourceId: 'custom'},
    });
    await expect(synced).resolves.toMatchObject({phase: 'live-best-effort'});
    await client.close();
  });

  it('rejects unsatisfiable sync waits with structured client errors', async () => {
    const noSource = new Client({worker: respondingWorker()});
    await expect(noSource.whenSynced()).rejects.toMatchObject({
      name: 'ClientError',
      code: 'SYNC_SOURCE_NOT_CONFIGURED',
    });
    await noSource.close();

    const timeout = new Client({
      worker: respondingWorker(),
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });
    await expect(timeout.whenSynced({timeoutMs: 0})).rejects.toMatchObject({
      name: 'ClientError',
      code: 'SYNC_TIMEOUT',
      retryable: true,
    });
    await timeout.close();

    const aborted = new Client({
      worker: respondingWorker(),
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });
    const controller = new AbortController();
    const waiting = aborted.whenSynced({signal: controller.signal});
    controller.abort();
    await expect(waiting).rejects.toMatchObject({
      name: 'ClientError',
      code: 'SYNC_ABORTED',
    });
    await aborted.close();
  });

  it('applies timeout and abort while local initialization is unresolved', async () => {
    const worker = new FakeWorker();
    worker.onPost = (message) => {
      if (message.method === 'close') {
        worker.respond({
          v: PROTOCOL_VERSION,
          id: message.id,
          ok: true,
          result: undefined,
        });
      }
    };
    const client = new Client({
      worker,
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });

    await expect(client.whenSynced({timeoutMs: 0})).rejects.toMatchObject({
      code: 'SYNC_TIMEOUT',
    });
    const controller = new AbortController();
    const waiting = client.whenSynced({signal: controller.signal});
    controller.abort();
    await expect(waiting).rejects.toMatchObject({code: 'SYNC_ABORTED'});

    const closedWait = client.whenSynced();
    const closing = client.close();
    await expect(closedWait).rejects.toMatchObject({code: 'CLIENT_CLOSED'});

    const init = worker.posted[0] as Extract<WorkerRequest, {method: 'init'}>;
    worker.respond({
      v: PROTOCOL_VERSION,
      id: init.id,
      ok: true,
      result: {revision: 0, sourceConfigured: true},
    });
    await client.ready();
    await closing;
  });

  it('rejects sync waits on terminal source error and client close', async () => {
    const worker = respondingWorker();
    const client = new Client({
      worker,
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });
    await client.ready();
    const failed = client.whenSynced();
    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {
        phase: 'error',
        error: {
          code: 'SUPABASE_PERMISSION_DENIED',
          message: 'permission denied',
          details: {relation: 'public.posts'},
        },
      },
    });
    await expect(failed).rejects.toMatchObject({
      name: 'ClientError',
      code: 'SUPABASE_PERMISSION_DENIED',
    });
    let secondFailure: ClientError | undefined;
    try {
      await client.whenSynced();
    } catch (error) {
      secondFailure = error instanceof ClientError ? error : undefined;
    }
    expect(secondFailure?.details).toEqual({relation: 'public.posts'});
    if (
      secondFailure?.details &&
      !Array.isArray(secondFailure.details) &&
      typeof secondFailure.details === 'object'
    ) {
      secondFailure.details.relation = 'mutated.by.caller';
    }
    expect(client.getSyncState().error?.details).toEqual({
      relation: 'public.posts',
    });
    await client.close();

    const closingClient = new Client({
      worker: respondingWorker(),
      source: {
        kind: 'supabase',
        url: 'https://example.supabase.co',
        publishableKey: 'sb_publishable_example',
        tables: [{table: 'posts', primaryKey: ['id']}],
      },
    });
    await closingClient.ready();
    const pending = closingClient.whenSynced();
    const closing = closingClient.close();
    await expect(pending).rejects.toMatchObject({
      name: 'ClientError',
      code: 'CLIENT_CLOSED',
    });
    await closing;
  });

  it('closes the RPC session and underlying worker', async () => {
    const worker = respondingWorker();
    const client = new Client({worker});
    await client.close();

    expect(worker.terminated).toBe(true);
    await client.close();
    expect(
      worker.posted.filter(
        (message) => (message as WorkerRequest).method === 'close',
      ),
    ).toHaveLength(1);
  });

  it('makes concurrent close calls await the same worker cleanup', async () => {
    const worker = new FakeWorker();
    let closeRequest: Extract<WorkerRequest, {method: 'close'}> | undefined;
    worker.onPost = (message) => {
      if (message.method === 'init') {
        queueMicrotask(() =>
          worker.respond({
            v: PROTOCOL_VERSION,
            id: message.id,
            ok: true,
            result: {revision: 0, sourceConfigured: false},
          }),
        );
      } else if (message.method === 'close') {
        closeRequest = message;
      }
    };
    const client = new Client({worker});
    await client.ready();

    const first = client.close();
    const second = client.close();
    expect(second).toBe(first);
    await vi.waitFor(() => expect(closeRequest).toBeDefined());
    expect(worker.terminated).toBe(false);

    worker.respond({
      v: PROTOCOL_VERSION,
      id: closeRequest!.id,
      ok: true,
      result: undefined,
    });
    await second;
    expect(worker.terminated).toBe(true);
  });
});
