import {describe, expect, it, vi} from 'vitest';

import {Client} from '../../src/client/client.ts';
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
          result: {revision: 0},
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
      (opfsWorker.posted[0] as Extract<WorkerRequest, {method: 'init'}>)
        .params.storage,
    ).toEqual({kind: 'opfs', name: 'application-cache'});

    await memoryClient.close();
    await opfsClient.close();
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
            result: {revision: 0},
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
