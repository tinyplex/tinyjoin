import {afterEach, describe, expect, it, vi} from 'vitest';
import type {WorkerRpc} from '../../src/client/rpc.js';
import {
  PROTOCOL_VERSION,
  type JsonValue,
  type RpcMethod,
  type SerializedError,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.js';
import {
  COORDINATION_COMPATIBILITY,
  MAX_PENDING_REQUESTS,
  MAX_QUEUED_BYTES,
  clientChannel,
} from '../../src/worker/coordination-protocol.js';
import {createDatabaseBroker} from '../../src/worker/database-broker.js';

afterEach(() => vi.unstubAllGlobals());

const epoch = 'test-owner';
const owner = 'local-client';
const follower = 'remote-client';

function harness(
  options: {rollbackFailure?: boolean; servesNow?: boolean} = {},
) {
  const responses = new Map<string, WorkerResponse[]>();
  const lifetimes = new Map<string, () => void>();
  const channels: FakeChannel[] = [];
  const record = (client: string, response: WorkerResponse): void => {
    const list = responses.get(client) ?? [];
    list.push(response);
    responses.set(client, list);
  };
  class FakeChannel {
    closed = false;
    constructor(readonly name: string) {
      channels.push(this);
    }
    postMessage(value: {response: WorkerResponse}): void {
      if (this.closed) throw new Error('A closed fixture channel was used');
      record(this.name.slice('tinyjoin:client:'.length), value.response);
    }
    close(): void {
      this.closed = true;
    }
  }
  vi.stubGlobal('BroadcastChannel', FakeChannel);
  vi.stubGlobal('navigator', {
    locks: {
      request(
        name: string,
        options: {signal: AbortSignal},
        callback: () => void,
      ): Promise<void> {
        return new Promise((resolve, reject) => {
          const abort = (): void => {
            lifetimes.delete(name);
            reject(new DOMException('Aborted', 'AbortError'));
          };
          options.signal.addEventListener('abort', abort, {once: true});
          lifetimes.set(name, () => {
            options.signal.removeEventListener('abort', abort);
            lifetimes.delete(name);
            Promise.resolve().then(callback).then(resolve, reject);
          });
        });
      },
    },
  });
  const request = vi.fn(
    async (method: RpcMethod, _params: unknown): Promise<unknown> => {
      if (method === 'beginTransaction') return {transactionId: 'tx-1'};
      if (method === 'commitTransaction')
        return {revision: 1, tables: ['items'], keys: {}};
      if (method === 'rollbackTransaction') {
        if (options.rollbackFailure)
          throw Object.assign(new Error('Injected rollback failure'), {
            code: 'STORAGE_ENGINE_POISONED',
          });
        return undefined;
      }
      if (method === 'executeSql')
        return {
          command: 'SELECT',
          revision: 1,
          fields: [],
          rows: [],
          rowCount: 0,
          tables: [],
        };
      return undefined;
    },
  );
  // A host that serves statements at once, as the owner's own host does.
  const requestNow = vi.fn((method: RpcMethod, params: unknown) =>
    method === 'executeSql'
      ? {ok: true as const, value: {command: 'SELECT', params}}
      : undefined,
  );
  const rpc: WorkerRpc & {requestNow?: typeof requestNow} = {
    request: request as WorkerRpc['request'],
    onEvent: vi.fn(),
    dispose: vi.fn(),
    ...(options.servesNow ? {requestNow} : {}),
  };
  const onFailure = vi.fn((_error: SerializedError) => broker.close());
  const broker = createDatabaseBroker(
    rpc,
    epoch,
    owner,
    (response) => record(owner, response),
    () => 1,
    vi.fn(),
    onFailure,
  );
  return {
    request,
    requestNow,
    onFailure,
    serveLocal: broker.serveLocal,
    send(client: string, request: WorkerRequest): void {
      broker.receive({
        kind: 'request',
        client,
        epoch,
        compatibility: COORDINATION_COMPATIBILITY,
        request,
      });
    },
    response(client: string, id: number): WorkerResponse | undefined {
      return responses.get(client)?.find((response) => response.id === id);
    },
    received(client: string): WorkerResponse[] {
      return responses.get(client) ?? [];
    },
    abandon(client: string): void {
      const release = lifetimes.get(clientChannel(client));
      if (!release)
        throw new Error('The fixture client has no lifecycle watcher');
      release();
    },
    close(): void {
      broker.close();
      expect(channels.every((channel) => channel.closed)).toBe(true);
    },
  };
}

const begin = (id = 1): WorkerRequest => ({
  v: PROTOCOL_VERSION,
  id,
  method: 'beginTransaction',
  params: undefined,
});
const query = (
  id: number,
  params: JsonValue[] = [],
  inTransaction = false,
): WorkerRequest => ({
  v: PROTOCOL_VERSION,
  id,
  method: 'executeSql',
  params: {
    sql: 'SELECT id FROM items WHERE id = $1',
    params,
    ...(inTransaction ? {transactionId: `${epoch}/tx-1`} : {}),
  },
});

describe('database broker scheduling', () => {
  it('serves a statement at once only when nothing waits ahead of it', async () => {
    const broker = harness({servesNow: true});
    try {
      // Nothing waits, so the statement is answered before the call returns.
      broker.send(owner, query(1, [1]));
      expect(broker.response(owner, 1)).toMatchObject({ok: true});

      // A transaction's own statement is served at once too, with its engine
      // token in place of the one its client holds.
      broker.send(follower, begin(2));
      await vi.waitFor(() =>
        expect(broker.response(follower, 2)).toMatchObject({ok: true}),
      );
      broker.send(follower, query(3, [3], true));
      expect(broker.response(follower, 3)).toMatchObject({ok: true});
      expect(broker.requestNow).toHaveBeenLastCalledWith('executeSql', {
        sql: 'SELECT id FROM items WHERE id = $1',
        params: [3],
        transactionId: 'tx-1',
      });

      // Another client's statement waits for the transaction, and then the
      // transaction's next statement waits behind it in the queue.
      broker.send(owner, query(4, [4]));
      broker.send(follower, query(5, [5], true));
      expect(broker.response(owner, 4)).toBeUndefined();
      expect(broker.response(follower, 5)).toBeUndefined();
      await vi.waitFor(() =>
        expect(broker.response(follower, 5)).toMatchObject({ok: true}),
      );
      expect(broker.response(owner, 4)).toBeUndefined();
      expect(broker.requestNow).toHaveBeenCalledTimes(2);
    } finally {
      broker.close();
    }
  });

  it("serves the owner tab's statement without queueing it", async () => {
    const broker = harness({servesNow: true});
    const posted: WorkerResponse[] = [];
    const post = (response: WorkerResponse): void => void posted.push(response);
    const sql = 'SELECT id FROM items WHERE id = $1';
    try {
      expect(broker.serveLocal(query(1, [1]), post)).toBe(true);
      expect(posted).toEqual([
        {
          v: PROTOCOL_VERSION,
          id: 1,
          ok: true,
          result: {command: 'SELECT', params: {sql, params: [1]}},
        },
      ]);

      // A statement the engine does not hold is left to be prepared again,
      // and another client's transaction holds the owner's statements back.
      const unprepared: WorkerRequest = {
        v: PROTOCOL_VERSION,
        id: 2,
        method: 'executePrepared',
        params: {statementId: 2, params: []},
      };
      expect(broker.serveLocal(unprepared, post)).toBe(false);
      broker.send(follower, begin(3));
      await vi.waitFor(() =>
        expect(broker.response(follower, 3)).toMatchObject({ok: true}),
      );
      expect(broker.serveLocal(query(4, [4]), post)).toBe(false);
      broker.send(follower, {
        v: PROTOCOL_VERSION,
        id: 5,
        method: 'commitTransaction',
        params: {transactionId: `${epoch}/tx-1`},
      });
      await vi.waitFor(() =>
        expect(broker.response(follower, 5)).toMatchObject({ok: true}),
      );

      // The owner's own transaction is served with the engine's token, and a
      // stale token is left for the queue to report.
      broker.send(owner, begin(6));
      await vi.waitFor(() =>
        expect(broker.response(owner, 6)).toMatchObject({ok: true}),
      );
      expect(broker.serveLocal(query(7, [7], true), post)).toBe(true);
      expect(broker.requestNow).toHaveBeenLastCalledWith('executeSql', {
        sql,
        params: [7],
        transactionId: 'tx-1',
      });
      const stale = query(8, [8]);
      Object.assign(stale.params!, {transactionId: 'old-owner/tx-1'});
      expect(broker.serveLocal(stale, post)).toBe(false);
      expect(posted.map(({id}) => id)).toEqual([1, 7]);

      // An error that leaves the engine beyond use is posted, and then
      // retires the owner, which serves nothing more.
      broker.requestNow.mockReturnValueOnce({
        ok: false,
        error: Object.assign(new Error('Injected poisoning'), {
          code: 'STORAGE_ENGINE_POISONED',
        }),
      } as never);
      expect(broker.serveLocal(query(9, [9], true), post)).toBe(true);
      const error = {
        code: 'STORAGE_ENGINE_POISONED',
        message: 'Injected poisoning',
      };
      expect(posted.at(-1)).toEqual({
        v: PROTOCOL_VERSION,
        id: 9,
        ok: false,
        error,
      });
      expect(broker.onFailure).toHaveBeenCalledExactlyOnceWith(error);
      expect(broker.serveLocal(query(10, [10]), post)).toBe(false);
    } finally {
      broker.close();
    }
  });
});

describe('database broker failure and resource boundaries', () => {
  it('retires the owner when an abandoned transaction cannot be rolled back', async () => {
    const broker = harness({rollbackFailure: true});
    try {
      broker.send(follower, begin());
      await vi.waitFor(() =>
        expect(broker.response(follower, 1)).toMatchObject({ok: true}),
      );
      broker.send(owner, query(2));
      expect(broker.response(owner, 2)).toBeUndefined();
      broker.abandon(follower);
      await vi.waitFor(() =>
        expect(broker.onFailure).toHaveBeenCalledExactlyOnceWith(
          expect.objectContaining({code: 'STORAGE_ENGINE_POISONED'}),
        ),
      );
      // Retirement is the coordinator's signal to fail every outstanding RPC.
      // A failed cleanup must not permit the queued query to enter this engine.
      expect(broker.request.mock.calls.map(([method]) => method)).toEqual([
        'beginTransaction',
        'rollbackTransaction',
      ]);
      broker.send(owner, query(3));
      await new Promise<void>((resolve) => setImmediate(resolve));
      expect(broker.onFailure).toHaveBeenCalledTimes(1);
      expect(broker.request).toHaveBeenCalledTimes(2);
    } finally {
      broker.close();
    }
  });

  it('bounds a blocked queue including owner SQL while reserving space for commit', async () => {
    const broker = harness();
    try {
      broker.send(owner, begin());
      await vi.waitFor(() =>
        expect(broker.response(owner, 1)).toMatchObject({ok: true}),
      );
      for (let index = 0; index < MAX_PENDING_REQUESTS; index++)
        broker.send(follower, query(index + 1, [index]));
      expect(broker.received(follower)).toEqual([]);
      broker.send(follower, query(MAX_PENDING_REQUESTS + 1, [0]));
      expect(broker.response(follower, MAX_PENDING_REQUESTS + 1)).toMatchObject(
        {ok: false, error: {code: 'RESOURCE_LIMIT'}},
      );
      broker.send(owner, query(2, [0], true));
      expect(broker.response(owner, 2)).toMatchObject({
        ok: false,
        error: {code: 'RESOURCE_LIMIT'},
      });
      broker.send(owner, {
        v: PROTOCOL_VERSION,
        id: 3,
        method: 'commitTransaction',
        params: {transactionId: `${epoch}/tx-1`},
      });
      await vi.waitFor(() =>
        expect(broker.response(owner, 3)).toMatchObject({ok: true}),
      );
      await vi.waitFor(() =>
        expect(broker.received(follower)).toHaveLength(
          MAX_PENDING_REQUESTS + 1,
        ),
      );
      expect(
        broker.request.mock.calls.slice(0, 2).map(([method]) => method),
      ).toEqual(['beginTransaction', 'commitTransaction']);
      expect(
        broker.request.mock.calls.filter(([method]) => method === 'executeSql'),
      ).toHaveLength(MAX_PENDING_REQUESTS);
      expect(broker.onFailure).not.toHaveBeenCalled();
    } finally {
      broker.close();
    }
  });

  it('rejects oversized aliased input without expanding the graph or blocking cleanup', async () => {
    const broker = harness();
    try {
      broker.send(owner, begin());
      await vi.waitFor(() =>
        expect(broker.response(owner, 1)).toMatchObject({ok: true}),
      );
      let oversized: JsonValue = 'x'.repeat(MAX_QUEUED_BYTES / 2 + 1);
      for (let depth = 0; depth < 40; depth++)
        oversized = [oversized, oversized];
      broker.send(follower, query(1, [oversized]));
      expect(broker.response(follower, 1)).toMatchObject({
        ok: false,
        error: {code: 'RESOURCE_LIMIT'},
      });
      broker.send(owner, query(2, [oversized], true));
      expect(broker.response(owner, 2)).toMatchObject({
        ok: false,
        error: {code: 'RESOURCE_LIMIT'},
      });
      expect(broker.request.mock.calls.map(([method]) => method)).toEqual([
        'beginTransaction',
      ]);
      broker.send(owner, {
        v: PROTOCOL_VERSION,
        id: 3,
        method: 'rollbackTransaction',
        params: {transactionId: `${epoch}/tx-1`},
      });
      await vi.waitFor(() =>
        expect(broker.response(owner, 3)).toMatchObject({ok: true}),
      );
      broker.send(follower, query(2, [1]));
      await vi.waitFor(() =>
        expect(broker.response(follower, 2)).toMatchObject({ok: true}),
      );
      expect(broker.onFailure).not.toHaveBeenCalled();
    } finally {
      broker.close();
    }
  });
});
