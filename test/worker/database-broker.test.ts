import {afterEach, describe, expect, it, vi} from 'vitest';
import {
  PROTOCOL_VERSION,
  STATEMENT_PARAMS,
  STATEMENT_PREPARED,
  STATEMENT_SQL,
  type JsonPrimitive,
  type JsonValue,
  type RpcMethod,
  type SerializedError,
  type SqlResult,
  type StatementRequest,
  type StatementResponse,
  type StatementResult,
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

// What a client is sent: a response, or a statement's flat response.
type Reply = WorkerResponse | StatementResponse;
const idOf = (reply: Reply): number =>
  Array.isArray(reply) ? reply[1] : reply.id;

// A statement's result that published nothing, as the engine's bridge returns
// it: a new array each time, its first two slots left for whoever posts it.
const flat = (key: JsonPrimitive): StatementResult => [
  0,
  0,
  1,
  1,
  1,
  'items',
  1,
  'id',
  key,
];
const published: SqlResult = {
  command: 'UPDATE',
  revision: 2,
  rowCount: 1,
  tables: ['items'],
  keys: {items: [{id: 1}]},
  data: '{"fields":[],"rows":[]}',
};

function harness(
  options: {rollbackFailure?: boolean; servesNow?: boolean} = {},
) {
  const responses = new Map<string, Reply[]>();
  const lifetimes = new Map<string, () => void>();
  const channels: FakeChannel[] = [];
  const routed: unknown[] = [];
  const record = (client: string, response: Reply): void => {
    const list = responses.get(client) ?? [];
    list.push(response);
    responses.set(client, list);
  };
  class FakeChannel {
    closed = false;
    constructor(readonly name: string) {
      channels.push(this);
    }
    postMessage(value: {epoch: string; response: Reply}): void {
      if (this.closed) throw new Error('A closed fixture channel was used');
      // A channel delivers a clone, which a response must survive.
      routed.push(structuredClone(value));
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
  let preparedStatements = 70;
  const request = vi.fn(
    async (method: RpcMethod, _params: unknown): Promise<unknown> => {
      if (method === 'beginTransaction') return {transactionId: 'tx-1'};
      if (method === 'commitTransaction') return {revision: 1};
      if (method === 'prepareSql') return {statementId: ++preparedStatements};
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
  // And one that serves a statement request at once: it returns the result,
  // here flat and keyed by the statement's first parameter.
  const statementNow = vi.fn(
    (
      request: StatementRequest,
      _target: string | number,
      _transactionId: string | undefined,
    ): SqlResult | StatementResult | undefined =>
      flat((request[STATEMENT_PARAMS] ?? null) as JsonPrimitive),
  );
  const rpc: Parameters<typeof createDatabaseBroker>[0] = {
    request: request as never,
    ...(options.servesNow ? {requestNow, statementNow} : {}),
  };
  const onFailure = vi.fn((_error: SerializedError) => broker.close());
  const noteResult = vi.fn();
  const broker = createDatabaseBroker(
    rpc,
    epoch,
    owner,
    (response) => record(owner, response),
    () => 1,
    noteResult,
    onFailure,
  );
  return {
    request,
    requestNow,
    statementNow,
    noteResult,
    onFailure,
    routed,
    serveLocal: broker.serveLocal,
    serveStatement: broker.serveStatement,
    send(client: string, request: WorkerRequest): void {
      broker.receive({
        kind: 'request',
        client,
        epoch,
        compatibility: COORDINATION_COMPATIBILITY,
        request,
      });
    },
    response(client: string, id: number): Reply | undefined {
      return responses.get(client)?.find((response) => idOf(response) === id);
    },
    received(client: string): Reply[] {
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
const sql = 'SELECT id FROM items WHERE id = $1';
const query = (
  id: number,
  params: JsonValue[] = [],
  inTransaction = false,
): WorkerRequest => ({
  v: PROTOCOL_VERSION,
  id,
  method: 'executeSql',
  params: {
    sql,
    params,
    ...(inTransaction ? {transactionId: `${epoch}/tx-1`} : {}),
  },
});
// The same statement, and a prepared one's execution, as statement requests.
const statement = (
  id: number,
  params: JsonValue[] = [],
  transaction: string | 0 = 0,
): StatementRequest => [
  PROTOCOL_VERSION,
  id,
  STATEMENT_SQL,
  sql,
  transaction,
  0,
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
const prepare = (id: number): WorkerRequest => ({
  v: PROTOCOL_VERSION,
  id,
  method: 'prepareSql',
  params: {sql},
});
const commit = (id: number): WorkerRequest => ({
  v: PROTOCOL_VERSION,
  id,
  method: 'commitTransaction',
  params: {transactionId: `${epoch}/tx-1`},
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
    const posted: Reply[] = [];
    const post = (response: Reply): void => void posted.push(response);
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
      expect(posted.map(idOf)).toEqual([1, 7]);

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

describe('database broker statement requests', () => {
  const collect = () => {
    const posted: Reply[] = [];
    return {posted, post: (response: Reply): void => void posted.push(response)};
  };

  it("serves the owner tab's statement request at once, under the engine's own names", async () => {
    const broker = harness({servesNow: true});
    const {posted, post} = collect();
    try {
      // The statement's text is its own name, and nothing names a transaction.
      const select = statement(1, [1]);
      expect(broker.serveStatement(select, post)).toBe(true);
      expect(broker.statementNow).toHaveBeenLastCalledWith(select, sql, undefined);
      expect(broker.statementNow.mock.calls[0]![0]).toBe(select);
      // The flat result is its own response, once it is addressed.
      expect(posted).toEqual([
        [PROTOCOL_VERSION, 1, 1, 1, 1, 'items', 1, 'id', 1],
      ]);

      // A prepared statement goes by the id the engine gave it, which the
      // client never sees: its own is the id of the request that prepared it.
      broker.send(owner, prepare(2));
      await vi.waitFor(() =>
        expect(broker.response(owner, 2)).toMatchObject({
          ok: true,
          result: {statementId: 2},
        }),
      );
      const prepared = execution(3, 2, [3, 'three'], 0, 1);
      expect(broker.serveStatement(prepared, post)).toBe(true);
      expect(broker.statementNow).toHaveBeenLastCalledWith(prepared, 71, undefined);
      expect(posted.at(-1)).toEqual([
        PROTOCOL_VERSION,
        3,
        1,
        1,
        1,
        'items',
        1,
        'id',
        3,
      ]);

      // One the engine does not hold is left to be prepared again, or refused,
      // when its request takes its turn.
      expect(broker.serveStatement(execution(4, 9, [4]), post)).toBe(false);
      expect(broker.statementNow).toHaveBeenCalledTimes(2);

      // The owner's transaction goes by the host's token for it. A stale one
      // is left for the queue to report.
      broker.send(owner, begin(5));
      await vi.waitFor(() =>
        expect(broker.response(owner, 5)).toMatchObject({
          ok: true,
          result: {transactionId: `${epoch}/tx-1`},
        }),
      );
      const inside = execution(6, 2, [6], `${epoch}/tx-1`);
      expect(broker.serveStatement(inside, post)).toBe(true);
      expect(broker.statementNow).toHaveBeenLastCalledWith(inside, 71, 'tx-1');
      expect(broker.serveStatement(statement(7, [7], 'old-owner/tx-1'), post)).toBe(
        false,
      );
      // One that names no transaction while its client's is open reaches the
      // host as it is, which refuses it.
      const outside = statement(8, [8]);
      expect(broker.serveStatement(outside, post)).toBe(true);
      expect(broker.statementNow).toHaveBeenLastCalledWith(outside, sql, undefined);
      expect(posted.map(idOf)).toEqual([1, 3, 6, 8]);

      // A flat result published nothing, so none of these held a revision.
      expect(broker.noteResult.mock.calls).toEqual([
        [{statementId: 2}],
        [{transactionId: `${epoch}/tx-1`}],
      ]);
      // Nothing went through the queue or the request the array stands for.
      expect(broker.requestNow).not.toHaveBeenCalled();
      expect(broker.request.mock.calls.map(([method]) => method)).toEqual([
        'prepareSql',
        'beginTransaction',
      ]);
    } finally {
      broker.close();
    }
  });

  it('leaves a statement request for the queue whenever its request would wait', async () => {
    const broker = harness({servesNow: true});
    const {posted, post} = collect();
    try {
      // The host has requests of its own ahead: it declines, and nothing is
      // posted.
      broker.statementNow.mockReturnValueOnce(undefined);
      expect(broker.serveStatement(statement(1, [1]), post)).toBe(false);

      // A transaction named when none is open is left to be reported.
      expect(broker.serveStatement(statement(2, [2], `${epoch}/tx-1`), post)).toBe(
        false,
      );

      // Another client's transaction holds the owner's statements back,
      // whichever transaction they name.
      broker.send(follower, begin(3));
      await vi.waitFor(() =>
        expect(broker.response(follower, 3)).toMatchObject({ok: true}),
      );
      expect(broker.serveStatement(statement(4, [4]), post)).toBe(false);
      expect(broker.serveStatement(statement(5, [5], `${epoch}/tx-1`), post)).toBe(
        false,
      );

      // So does anything waiting in the queue, here the owner's own statement
      // behind that transaction.
      broker.send(follower, commit(6));
      await vi.waitFor(() =>
        expect(broker.response(follower, 6)).toMatchObject({ok: true}),
      );
      broker.send(follower, begin(7));
      await vi.waitFor(() =>
        expect(broker.response(follower, 7)).toMatchObject({ok: true}),
      );
      broker.send(owner, query(8, [8]));
      broker.send(follower, commit(9));
      // The commit is in the queue behind the statement, and then in flight.
      expect(broker.serveStatement(statement(10, [10]), post)).toBe(false);
      await vi.waitFor(() =>
        expect(broker.response(owner, 8)).toMatchObject({ok: true}),
      );

      // And a client that has gone, until it has been forgotten, which waits
      // for the end of the transaction that is open, here the owner's own.
      await vi.waitFor(() =>
        expect(broker.response(follower, 9)).toMatchObject({ok: true}),
      );
      broker.send(owner, begin(11));
      await vi.waitFor(() =>
        expect(broker.response(owner, 11)).toMatchObject({ok: true}),
      );
      const inside = statement(12, [12], `${epoch}/tx-1`);
      expect(broker.serveStatement(inside, post)).toBe(true);
      broker.abandon(follower);
      await new Promise<void>((resolve) => setImmediate(resolve));
      expect(broker.serveStatement(statement(13, [13], `${epoch}/tx-1`), post)).toBe(
        false,
      );
      broker.send(owner, commit(14));
      await vi.waitFor(() =>
        expect(broker.response(owner, 14)).toMatchObject({ok: true}),
      );
      await vi.waitFor(() =>
        expect(broker.serveStatement(statement(15, [15]), post)).toBe(true),
      );

      expect(posted.map(idOf)).toEqual([12, 15]);
      expect(broker.statementNow).toHaveBeenCalledTimes(3);
    } finally {
      broker.close();
    }
    // A closed owner serves nothing.
    expect(broker.serveStatement(statement(16, [16]), post)).toBe(false);
    // Nor does one whose host cannot serve at once.
    const queued = harness();
    try {
      expect(queued.serveStatement(statement(1, [1]), post)).toBe(false);
    } finally {
      queued.close();
    }
    expect(posted.map(idOf)).toEqual([12, 15]);
  });

  it('leaves a statement request alone while a request is in flight, or one waits in the queue', async () => {
    const broker = harness({servesNow: true});
    const {posted, post} = collect();
    try {
      // Another client's request is with the host, which has yet to answer.
      // Nothing waits in the queue and no transaction is open, and still the
      // owner's statement must not run before that request has settled.
      let answer!: (result: unknown) => void;
      broker.request.mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            answer = resolve;
          }),
      );
      broker.requestNow.mockReturnValueOnce(undefined as never);
      broker.send(follower, query(1, [1]));
      expect(broker.request).toHaveBeenLastCalledWith('executeSql', {
        sql,
        params: [1],
      });
      expect(broker.serveStatement(statement(2, [2]), post)).toBe(false);
      await new Promise<void>((resolve) => setImmediate(resolve));
      expect(broker.serveStatement(statement(2, [2]), post)).toBe(false);
      expect(broker.statementNow).not.toHaveBeenCalled();
      answer(flat(1));
      await vi.waitFor(() =>
        expect(broker.response(follower, 1)).toBeDefined(),
      );
      await vi.waitFor(() =>
        expect(broker.serveStatement(statement(3, [3]), post)).toBe(true),
      );

      // The owner's own transaction is open, and another client's statement
      // waits in the queue for it to end. Nothing in the queue can run, so
      // nothing is in flight, and still the owner's next statement takes its
      // turn through the queue, as its request would.
      broker.send(owner, begin(4));
      await vi.waitFor(() =>
        expect(broker.response(owner, 4)).toMatchObject({ok: true}),
      );
      broker.requestNow.mockClear();
      broker.send(follower, query(5, [5]));
      expect(broker.response(follower, 5)).toBeUndefined();
      expect(broker.requestNow).not.toHaveBeenCalled();
      const inside = statement(6, [6], `${epoch}/tx-1`);
      expect(broker.serveStatement(inside, post)).toBe(false);
      await new Promise<void>((resolve) => setImmediate(resolve));
      expect(broker.serveStatement(inside, post)).toBe(false);
      expect(broker.statementNow).toHaveBeenCalledTimes(1);

      // Once the transaction has ended and the queue has drained, the owner's
      // statements are served at once again.
      broker.send(owner, commit(7));
      await vi.waitFor(() =>
        expect(broker.response(follower, 5)).toMatchObject({ok: true}),
      );
      await vi.waitFor(() =>
        expect(broker.serveStatement(statement(8, [8]), post)).toBe(true),
      );
      expect(posted.map(idOf)).toEqual([3, 8]);
    } finally {
      broker.close();
    }
  });

  it('answers a result that is not flat in a response, and notes its revision', () => {
    const broker = harness({servesNow: true});
    const {posted, post} = collect();
    try {
      broker.statementNow.mockReturnValueOnce({...published});
      expect(broker.serveStatement(statement(1, [1]), post)).toBe(true);
      expect(posted).toEqual([
        {v: PROTOCOL_VERSION, id: 1, ok: true, result: published},
      ]);
      expect(broker.noteResult).toHaveBeenCalledExactlyOnceWith(published);
    } finally {
      broker.close();
    }
  });

  it('answers a failure with its error, and retires the owner when the engine is beyond use', () => {
    const broker = harness({servesNow: true});
    const {posted, post} = collect();
    try {
      // The host throws the error a response would carry.
      broker.statementNow.mockImplementationOnce(() => {
        throw Object.assign(new Error('Use the active transaction'), {
          code: 'TRANSACTION_ACTIVE',
          retryable: false,
        });
      });
      expect(broker.serveStatement(statement(1, [1]), post)).toBe(true);
      expect(posted.at(-1)).toEqual({
        v: PROTOCOL_VERSION,
        id: 1,
        ok: false,
        error: {
          code: 'TRANSACTION_ACTIVE',
          message: 'Use the active transaction',
          retryable: false,
        },
      });
      // An error without a code is still an answer.
      broker.statementNow.mockImplementationOnce(() => {
        throw new Error('Something else');
      });
      expect(broker.serveStatement(statement(2, [2]), post)).toBe(true);
      expect(posted.at(-1)).toEqual({
        v: PROTOCOL_VERSION,
        id: 2,
        ok: false,
        error: {code: 'WORKER_OPERATION_FAILED', message: 'Something else'},
      });
      expect(broker.onFailure).not.toHaveBeenCalled();
      expect(broker.noteResult).not.toHaveBeenCalled();

      // Each of the codes that leave the engine beyond use is posted, and
      // then retires the owner, which serves nothing more.
      for (const [index, code] of [
        'RECOVERY_REQUIRED',
        'STORAGE_COMMIT_OUTCOME_UNKNOWN',
        'STORAGE_ENGINE_POISONED',
      ].entries()) {
        const failing = harness({servesNow: true});
        try {
          failing.statementNow.mockImplementationOnce(() => {
            throw Object.assign(new Error(`Injected ${code}`), {code});
          });
          expect(failing.serveStatement(statement(3 + index, [3]), post)).toBe(true);
          const error = {code, message: `Injected ${code}`};
          expect(posted.at(-1)).toEqual({
            v: PROTOCOL_VERSION,
            id: 3 + index,
            ok: false,
            error,
          });
          expect(failing.onFailure).toHaveBeenCalledExactlyOnceWith(error);
          expect(failing.serveStatement(statement(9, [9]), post)).toBe(false);
        } finally {
          failing.close();
        }
      }
      expect(posted.map(idOf)).toEqual([1, 2, 3, 4, 5]);
    } finally {
      broker.close();
    }
  });

  it('posts the same response however a statement reached the host', async () => {
    const broker = harness({servesNow: true});
    const {posted, post} = collect();
    try {
      // Served at once from its array, at once from the request it stands
      // for, and from the queue: the host returns the same flat result, and
      // each becomes the same response.
      broker.requestNow.mockReturnValueOnce({ok: true, value: flat(5)} as never);
      broker.request.mockResolvedValueOnce(flat(5));
      expect(broker.serveStatement(statement(1, [5]), post)).toBe(true);
      expect(broker.serveLocal(query(2, [5]), post)).toBe(true);
      // A queued statement goes to the host by request.
      broker.requestNow.mockReturnValueOnce(undefined as never);
      broker.send(owner, query(3, [5]));
      await vi.waitFor(() => expect(broker.response(owner, 3)).toBeDefined());
      const all = [...posted, broker.response(owner, 3)!];
      expect(all).toEqual(
        [1, 2, 3].map((id) => [PROTOCOL_VERSION, id, 1, 1, 1, 'items', 1, 'id', 5]),
      );
      expect(broker.noteResult).not.toHaveBeenCalled();

      // And so does a result that is not flat.
      broker.statementNow.mockReturnValueOnce({...published});
      broker.requestNow.mockReturnValueOnce({
        ok: true,
        value: {...published},
      } as never);
      broker.request.mockResolvedValueOnce({...published});
      expect(broker.serveStatement(statement(4, [5]), post)).toBe(true);
      expect(broker.serveLocal(query(5, [5]), post)).toBe(true);
      broker.requestNow.mockReturnValueOnce(undefined as never);
      broker.send(owner, query(6, [5]));
      await vi.waitFor(() => expect(broker.response(owner, 6)).toBeDefined());
      expect([...posted.slice(2), broker.response(owner, 6)!]).toEqual(
        [4, 5, 6].map((id) => ({
          v: PROTOCOL_VERSION,
          id,
          ok: true,
          result: published,
        })),
      );
      expect(broker.noteResult.mock.calls).toEqual([
        [published],
        [published],
        [published],
      ]);
    } finally {
      broker.close();
    }
  });

  it("answers another tab's statement with the flat response its page reads", async () => {
    const broker = harness({servesNow: true});
    try {
      // Another tab's statement arrives as the request its array stood for.
      // Served at once, its flat result crosses the channel as the response
      // itself, beside the owner's epoch.
      broker.requestNow.mockReturnValueOnce({ok: true, value: flat(1)} as never);
      broker.send(follower, query(1, [1]));
      const first = [PROTOCOL_VERSION, 1, 1, 1, 1, 'items', 1, 'id', 1];
      expect(broker.routed).toEqual([{epoch, response: first}]);

      // From the queue it crosses the same way, here inside that tab's
      // transaction, under the host's token for it.
      broker.send(follower, begin(2));
      await vi.waitFor(() =>
        expect(broker.response(follower, 2)).toMatchObject({ok: true}),
      );
      broker.request.mockResolvedValueOnce(flat(4));
      broker.requestNow.mockReturnValueOnce(undefined as never);
      broker.send(follower, query(4, [4], true));
      expect(broker.response(follower, 4)).toBeUndefined();
      await vi.waitFor(() =>
        expect(broker.response(follower, 4)).toEqual([
          PROTOCOL_VERSION,
          4,
          1,
          1,
          1,
          'items',
          1,
          'id',
          4,
        ]),
      );
      expect(broker.routed.at(-1)).toEqual({
        epoch,
        response: [PROTOCOL_VERSION, 4, 1, 1, 1, 'items', 1, 'id', 4],
      });
      expect(broker.request).toHaveBeenLastCalledWith('executeSql', {
        sql,
        params: [4],
        transactionId: 'tx-1',
      });

      // A result that is not flat crosses in a response, as it always has,
      // and a commit crosses with its revision alone.
      broker.requestNow.mockReturnValueOnce({
        ok: true,
        value: {...published},
      } as never);
      broker.send(follower, query(5, [5], true));
      expect(broker.routed.at(-1)).toEqual({
        epoch,
        response: {v: PROTOCOL_VERSION, id: 5, ok: true, result: published},
      });
      broker.send(follower, commit(6));
      await vi.waitFor(() =>
        expect(broker.response(follower, 6)).toEqual({
          v: PROTOCOL_VERSION,
          id: 6,
          ok: true,
          result: {revision: 1},
        }),
      );
      // The flat results noted nothing; the published result and the commit
      // each held a revision.
      expect(broker.noteResult.mock.calls).toEqual([
        [{transactionId: `${epoch}/tx-1`}],
        [published],
        [{revision: 1}],
      ]);
    } finally {
      broker.close();
    }
  });

  it("takes a script's results for results, however many there are", async () => {
    const broker = harness({servesNow: true});
    const {posted, post} = collect();
    const script = (id: number): WorkerRequest => ({
      v: PROTOCOL_VERSION,
      id,
      method: 'execSql',
      params: {sql: 'SELECT 1; SELECT 2'},
    });
    try {
      // A script's results are an array too, even an empty one, but never a
      // flat result.
      for (const [id, results] of [
        [1, []],
        [2, [published, published]],
      ] as const) {
        broker.requestNow.mockReturnValueOnce({ok: true, value: results} as never);
        expect(broker.serveLocal(script(id), post)).toBe(true);
        expect(posted.at(-1)).toEqual({
          v: PROTOCOL_VERSION,
          id,
          ok: true,
          result: results,
        });
        expect(broker.noteResult).toHaveBeenLastCalledWith(results);
      }
      broker.request.mockResolvedValueOnce([published]);
      broker.send(follower, script(3));
      await vi.waitFor(() =>
        expect(broker.response(follower, 3)).toEqual({
          v: PROTOCOL_VERSION,
          id: 3,
          ok: true,
          result: [published],
        }),
      );
    } finally {
      broker.close();
    }
  });
});
