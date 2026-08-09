import {describe, expect, it, vi} from 'vitest';

import {TinygresClient} from '../../src/client/client.ts';
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

describe('TinygresClient', () => {
  it('supports an awaitable Supabase-style query chain', async () => {
    const worker = respondingWorker();
    const client = new TinygresClient(worker);

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
    const client = new TinygresClient(worker);
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
    const client = new TinygresClient(worker);
    await client.close();

    expect(worker.terminated).toBe(true);
    await client.close();
    expect(
      worker.posted.filter(
        (message) => (message as WorkerRequest).method === 'close',
      ),
    ).toHaveLength(1);
  });
});
