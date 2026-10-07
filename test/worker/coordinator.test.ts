import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest';

import {
  PROTOCOL_VERSION,
  STATEMENT_COMMANDS,
  STATEMENT_PARAMS,
  STATEMENT_PREPARED,
  STATEMENT_SQL,
  isStatementResponse,
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
import {
  MAX_PENDING_REQUESTS,
  MAX_QUEUED_BYTES,
  clientChannel,
  ownerChannel,
} from '../../src/worker/coordination-protocol.ts';
import {startCoordinatedWorker} from '../../src/worker/coordinator.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {statementRequest, type WorkerScope} from '../../src/worker/host.ts';

// The coordinator opens its database through the private OPFS runtime, which
// these tests replace with an engine of their own.
const runtime = vi.hoisted(() => ({
  open: (_name: string): unknown => {
    throw new Error('No engine fixture is installed');
  },
}));

vi.mock('../../src/worker/opfs-loader.ts', () => ({
  createOpfsWasmEngine: async (name: string) => runtime.open(name),
}));

// A memory database opens the page-native engine in a host of its own, which
// these tests replace in the same way.
vi.mock('../../src/worker/engine.ts', async (loadActual) => ({
  ...(await loadActual<typeof import('../../src/worker/engine.ts')>()),
  createMemoryWasmEngine: async () => runtime.open('memory'),
}));

type Posted = WorkerResponse | StatementResponse | WorkerEvent;
type EngineCall = {
  target: string | number;
  sql: string;
  params: JsonValue[];
  rowMode: RowMode | undefined;
};

const SELECT = 'SELECT id FROM posts WHERE id = $1';
const UPDATE = 'UPDATE posts SET title = $1 WHERE id = $2';
const RETURNING = 'INSERT INTO posts (id) VALUES ($1) RETURNING id';
const FAIL = 'FAIL $1';

const sqlData = (fields: unknown[], rows: unknown[]): string =>
  JSON.stringify({fields, rows});

/**
 * The tabs of one origin, in one process: the Web Locks and BroadcastChannels
 * that the Workers of every tab share, a page for each tab, and the database
 * that whichever Worker owns it opens. Channels deliver clones in later tasks,
 * and locks are granted in later microtasks, as a browser's are.
 */
function origin() {
  const calls: EngineCall[] = [];
  // For each of those calls, the id of the statement request that the engine
  // was handed as it arrived, to read its parameters in place, or `undefined`
  // when it was handed parameters alone, as a request's are. A statement
  // reaches the engine in place only when it is served at once from its
  // array, which nothing else that a page can see tells apart: the answer is
  // the same either way. It is kept apart from the calls, which are the same
  // whichever way a statement came.
  const inPlace: (number | undefined)[] = [];
  const database = {revision: 0, opened: 0};

  // An engine that answers as the bridge does: a statement that published
  // nothing with the flat array of its result, and any other with a result.
  runtime.open = (): WorkerEngine => {
    database.opened += 1;
    let transactionActive = false;
    let staged = false;
    const prepared: string[] = [];
    const run = (
      sql: string,
      target: string | number,
      all: readonly JsonValue[],
      rowMode: RowMode | undefined,
      from = 0,
    ): SqlResult | StatementResult => {
      const params = all.slice(from);
      calls.push({target, sql, params, rowMode});
      inPlace.push(
        from === STATEMENT_PARAMS && all[0] === PROTOCOL_VERSION
          ? (all[1] as number)
          : undefined,
      );
      const command = sql.split(' ', 1)[0]!;
      if (command === 'FAIL') {
        throw Object.assign(new Error(`The engine refused ${String(params[0])}`), {
          code: String(params[0]),
        });
      }
      const key = params.find((value) => typeof value === 'number') ?? null;
      if (command === 'SELECT') {
        return [
          0,
          0,
          STATEMENT_COMMANDS.indexOf('SELECT'),
          // A flat result's revision is never news to the owner: one that
          // claims to be is still not noted.
          sql.includes('FUTURE') ? 99 : database.revision,
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
          database.revision,
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
        database.revision += 1;
      }
      return {
        command,
        revision: database.revision,
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
    return {
      executeSql: (sql, params, rowMode, from) =>
        run(sql, sql, params, rowMode, from),
      prepareSql: (sql) => prepared.push(sql),
      executePrepared: (statementId, params, rowMode, from) => {
        const sql = prepared[statementId - 1];
        if (sql === undefined) {
          throw Object.assign(new Error('The statement is not prepared'), {
            code: 'PREPARED_STATEMENT_NOT_FOUND',
          });
        }
        return run(sql, statementId, params, rowMode, from);
      },
      closePrepared: (statementId) => {
        delete prepared[statementId - 1];
      },
      execSql: () => [],
      beginTransaction: () => {
        transactionActive = true;
        staged = false;
      },
      commitTransaction: () => {
        transactionActive = false;
        if (staged) {
          database.revision += 1;
        }
        return {
          revision: database.revision,
          tables: staged ? ['posts'] : [],
          keys: {},
        };
      },
      rollbackTransaction: () => {
        transactionActive = false;
      },
      inTransaction: () => transactionActive,
      revision: () => database.revision,
      check: () => undefined,
      schema: () => ({version: 0, tables: []}),
      setSchema: () => ({revision: database.revision, tables: [], keys: {}}),
      close: () => undefined,
    };
  };

  // Web Locks: exclusive, granted in the order requested, each held until
  // its callback settles.
  type Holder = {name: string; clientId: string};
  const held = new Map<string, Holder>();
  const waiting = new Map<string, (() => void)[]>();
  let holders = 0;
  const release = (name: string): void => {
    held.delete(name);
    waiting.get(name)?.shift()?.();
  };
  const locks = {
    request(
      name: string,
      optionsOrCallback: {signal?: AbortSignal} | (() => unknown),
      maybeCallback?: () => unknown,
    ): Promise<unknown> {
      const callback = (maybeCallback ?? optionsOrCallback) as () => unknown;
      const signal = maybeCallback
        ? (optionsOrCallback as {signal?: AbortSignal}).signal
        : undefined;
      return new Promise((resolve, reject) => {
        const grant = (): void => {
          signal?.removeEventListener('abort', abort);
          held.set(name, {name, clientId: `environment-${++holders}`});
          Promise.resolve()
            .then(callback)
            .finally(() => release(name))
            .then(resolve, reject);
        };
        const abort = (): void => {
          const queue = waiting.get(name) ?? [];
          const at = queue.indexOf(grant);
          if (at >= 0) queue.splice(at, 1);
          reject(new DOMException('The lock request was aborted', 'AbortError'));
        };
        if (signal?.aborted) {
          abort();
          return;
        }
        signal?.addEventListener('abort', abort, {once: true});
        if (held.has(name)) {
          waiting.set(name, [...(waiting.get(name) ?? []), grant]);
        } else {
          // Held from now, so that a second request waits behind this one.
          held.set(name, {name, clientId: 'granting'});
          queueMicrotask(grant);
        }
      });
    },
    query: async () => ({
      held: [...held.values()].map((holder) => ({...holder, mode: 'exclusive'})),
      pending: [],
    }),
  };

  // BroadcastChannels: every other open channel of the same name receives a
  // clone, in a later task.
  const channels = new Set<FakeChannel>();
  class FakeChannel {
    onmessage: ((event: MessageEvent<unknown>) => void) | null = null;
    closed = false;
    constructor(readonly name: string) {
      channels.add(this);
    }
    postMessage(message: unknown): void {
      if (this.closed) {
        throw new DOMException('The channel is closed', 'InvalidStateError');
      }
      for (const other of channels) {
        if (other !== this && other.name === this.name) {
          const data = structuredClone(message);
          setTimeout(() => {
            if (!other.closed) other.onmessage?.({data} as MessageEvent<unknown>);
          }, 0);
        }
      }
    }
    close(): void {
      this.closed = true;
      channels.delete(this);
    }
  }
  vi.stubGlobal('BroadcastChannel', FakeChannel);
  vi.stubGlobal('navigator', {locks});

  /** A page and its Worker: what the page was posted, and a way to post to it. */
  const openTab = () => {
    const posted: Posted[] = [];
    const listeners = new Set<(event: MessageEvent<unknown>) => void>();
    const state = {closed: false};
    const scope: WorkerScope = {
      // A page receives a clone, taken as the message is posted.
      postMessage: (message) => void posted.push(structuredClone(message)),
      addEventListener: (_type, listener) => void listeners.add(listener),
      removeEventListener: (_type, listener) => void listeners.delete(listener),
      close: () => {
        state.closed = true;
      },
    };
    startCoordinatedWorker(scope);
    let nextId = 1;
    const send = (message: unknown): void => {
      const data = structuredClone(message);
      for (const listener of [...listeners]) {
        listener({data} as MessageEvent<unknown>);
      }
    };
    const answer = (id: number): Posted | undefined =>
      posted.find((message) => idOf(message) === id);
    const answered = async (id: number): Promise<Posted> => {
      await vi.waitFor(() => expect(answer(id)).toBeDefined(), {interval: 1});
      return answer(id)!;
    };
    const tab = {
      posted,
      state,
      send,
      answer,
      answered,
      id: (): number => nextId++,
      /** Sends a request, and waits for its result. */
      request: async (
        method: WorkerRequest['method'],
        params?: unknown,
      ): Promise<unknown> => {
        const id = nextId++;
        send({v: PROTOCOL_VERSION, id, method, params});
        const response = await answered(id);
        if (Array.isArray(response) || !('ok' in response) || !response.ok) {
          throw new Error(`Request ${method} failed: ${JSON.stringify(response)}`);
        }
        return response.result;
      },
      open: async (): Promise<void> => {
        await tab.request('init', {storage: {kind: 'opfs', name: 'shared'}});
      },
      answers: (): Posted[] =>
        posted.filter((message) => idOf(message) !== undefined),
      events: (): WorkerEvent[] =>
        posted.filter(
          (message): message is WorkerEvent => idOf(message) === undefined,
        ),
      close: async (): Promise<void> => {
        if (state.closed) return;
        send({v: PROTOCOL_VERSION, id: nextId++, method: 'close', params: undefined});
        await vi.waitFor(() => expect(state.closed).toBe(true), {interval: 1});
      },
    };
    tabs.push(tab);
    return tab;
  };
  const tabs: {close(): Promise<void>}[] = [];

  return {
    calls,
    inPlace,
    database,
    openTab,
    channel: (name: string) => new FakeChannel(name),
    /** The name of the lock each open database owner holds, and its holder. */
    locksHeld: (): string[] => [...held.keys()],
    closeAll: async (): Promise<void> => {
      for (const tab of tabs) await tab.close();
    },
  };
}

const idOf = (message: Posted): number | undefined =>
  Array.isArray(message) ? message[1] : 'id' in message ? message.id : undefined;

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

const nextTask = (): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, 0));

/** Lets the channels deliver, and the Workers answer, until all is quiet. */
const settled = async (): Promise<void> => {
  for (let turn = 0; turn < 6; turn++) await nextTask();
};

let tabs: ReturnType<typeof origin>;

beforeEach(() => {
  tabs = origin();
});

afterEach(async () => {
  await tabs.closeAll();
  vi.unstubAllGlobals();
});

describe('a database owner', () => {
  it("serves its tab's statement requests at once", async () => {
    const tab = tabs.openTab();
    await tab.open();
    tab.posted.length = 0;

    // With nothing waiting, each is answered before its message returns.
    tab.send(statement(10, SELECT, [7]));
    tab.send(statement(11, SELECT, [8], 0, 1));
    expect(tab.posted).toEqual([
      [PROTOCOL_VERSION, 10, 3, 0, 1, sqlData([{name: 'id', dataTypeID: 20}], [{id: 7}])],
      [PROTOCOL_VERSION, 11, 3, 0, 1, sqlData([{name: 'id', dataTypeID: 20}], [[8]])],
    ]);
    for (const response of tab.posted) {
      expect(isStatementResponse(response as unknown[], true)).toBe(true);
    }
    expect(tabs.calls).toEqual([
      {target: SELECT, sql: SELECT, params: [7], rowMode: undefined},
      {target: SELECT, sql: SELECT, params: [8], rowMode: 'array'},
    ]);
    // Each reached the engine as the array it arrived as, its parameters read
    // where they stood: no request was built to carry it, nor kept to wait
    // for its answer, which is all that serving it here saves.
    expect(tabs.inPlace).toEqual([10, 11]);

    // A prepared statement goes by the id of the request that prepared it,
    // and a transaction by an id that names this owner.
    const prepareId = tab.id();
    tab.send({v: PROTOCOL_VERSION, id: prepareId, method: 'prepareSql', params: {sql: UPDATE}});
    expect(await tab.answered(prepareId)).toMatchObject({result: {statementId: prepareId}});
    const {transactionId} = (await tab.request('beginTransaction')) as {
      transactionId: string;
    };
    expect(transactionId).toMatch(/^[0-9a-f-]{36}\/tx-1$/);
    tab.posted.length = 0;
    tabs.calls.length = 0;
    tab.send(execution(20, prepareId, ['one', 1], transactionId));
    tab.send(statement(21, UPDATE, ['two', 2], transactionId));
    tab.send(statement(22, SELECT, [2], transactionId));
    expect(tab.posted).toEqual([
      [PROTOCOL_VERSION, 20, 1, 0, 1, 'posts', 1, 'id', 1],
      [PROTOCOL_VERSION, 21, 1, 0, 1, 'posts', 1, 'id', 2],
      [PROTOCOL_VERSION, 22, 3, 0, 1, expect.any(String)],
    ]);
    // The engine is asked for the statement under its own id for it, and
    // again reads each request in place.
    expect(tabs.calls).toEqual([
      {target: 1, sql: UPDATE, params: ['one', 1], rowMode: undefined},
      {target: UPDATE, sql: UPDATE, params: ['two', 2], rowMode: undefined},
      {target: SELECT, sql: SELECT, params: [2], rowMode: undefined},
    ]);
    expect(tabs.inPlace).toEqual([10, 11, 20, 21, 22]);

    // The commit answers with its revision alone, and the event that follows
    // carries what it changed.
    tab.posted.length = 0;
    expect(await tab.request('commitTransaction', {transactionId})).toEqual({
      revision: 1,
    });
    await settled();
    expect(tab.posted).toEqual([
      {v: PROTOCOL_VERSION, id: expect.any(Number), ok: true, result: {revision: 1}},
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables: ['posts'], keys: {}},
      },
    ]);
  });
});

describe('a Worker whose first message is not the opening of a persistent database', () => {
  const uninitialized = {
    code: 'WORKER_NOT_INITIALIZED',
    message: 'Initialize the TinyJoin worker before sending other requests',
  };

  it('serves a memory database from a host of its own, statement requests included', async () => {
    const tab = tabs.openTab();
    expect(await tab.request('init', {storage: {kind: 'memory'}})).toEqual({
      revision: 0,
    });
    tab.posted.length = 0;

    tab.send(statement(10, SELECT, [7], 0, 1));
    tab.send(statement(11, UPDATE, ['changed', 5]));
    expect(tab.posted).toEqual([
      [PROTOCOL_VERSION, 10, 3, 0, 1, sqlData([{name: 'id', dataTypeID: 20}], [[7]])],
      {
        v: PROTOCOL_VERSION,
        id: 11,
        ok: true,
        result: {
          command: 'UPDATE',
          revision: 1,
          rowCount: 1,
          tables: ['posts'],
          keys: {posts: [{id: 5}]},
          data: sqlData([], []),
        },
      },
    ]);
    const {transactionId} = (await tab.request('beginTransaction')) as {
      transactionId: string;
    };
    // The host's own name for its transaction: no owner stands between.
    expect(transactionId).toBe('tx-1');
    tab.send(statement(12, UPDATE, ['staged', 6], transactionId));
    expect(tab.answer(12)).toEqual([PROTOCOL_VERSION, 12, 1, 1, 1, 'posts', 1, 'id', 6]);
    // The host read each from the array it arrived as.
    expect(tabs.inPlace).toEqual([10, 11, 12]);
    expect(await tab.request('commitTransaction', {transactionId})).toEqual({
      revision: 2,
    });
    // Nothing of the persistent path was entered: no lock is held.
    expect(tabs.locksHeld()).toEqual([]);
  });

  it('answers a statement request that arrives first as a host answers any request before init', async () => {
    const tab = tabs.openTab();
    tab.send(statement(1, SELECT, [1]));
    expect(await tab.answered(1)).toEqual({
      v: PROTOCOL_VERSION,
      id: 1,
      ok: false,
      error: uninitialized,
    });
    // The Worker is a host from then on: it never joins a persistent database.
    expect(tabs.calls).toEqual([]);
    expect(tabs.locksHeld()).toEqual([]);
  });
});

// One session's requests, each asked through `ask` and its answer kept. The
// statements an owner can serve at once are those in `AT_ONCE`; each of the
// others names a transaction or a prepared statement that the owner does not
// hold, and so must take its turn to be refused. The one in `REFUSED_AT_ONCE`
// names another owner's transaction, which a tab's own Worker refuses as it
// arrives, wherever the owner is.
const AT_ONCE = new Set([101, 102, 103, 105, 108, 109, 110, 111, 114, 115, 117]);
const REFUSED_AT_ONCE = 113;
const session = async (
  ask: (message: StatementRequest | WorkerRequest) => Promise<Posted>,
): Promise<Posted[]> => {
  const answers: Posted[] = [];
  const push = async (message: StatementRequest | WorkerRequest): Promise<Posted> => {
    const answer = await ask(message);
    answers.push(answer);
    return answer;
  };
  const result = (answer: Posted): Record<string, unknown> =>
    (answer as {result: Record<string, unknown>}).result;
  await push(statement(101, SELECT, [7]));
  await push(statement(102, SELECT, [8], 0, 1));
  await push(statement(103, UPDATE, ['outside', 5]));
  const prepared = result(
    await push({v: PROTOCOL_VERSION, id: 104, method: 'prepareSql', params: {sql: UPDATE}}),
  ).statementId as number;
  await push(execution(105, prepared, ['prepared', 9]));
  await push(execution(106, 999, []));
  const transaction = result(
    await push({v: PROTOCOL_VERSION, id: 107, method: 'beginTransaction', params: undefined}),
  ).transactionId as string;
  const epoch = transaction.slice(0, transaction.indexOf('/'));
  await push(statement(108, UPDATE, ['staged', 5], transaction));
  await push(execution(109, prepared, ['staged', 6], transaction, 1));
  await push(statement(110, RETURNING, [7], transaction, 1));
  await push(statement(111, SELECT, [5]));
  await push(statement(112, SELECT, [5], `${epoch}/tx-9`));
  await push(statement(113, SELECT, [5], 'another-owner/tx-1'));
  await push(statement(114, FAIL, ['CONSTRAINT_VIOLATION'], transaction));
  await push(statement(115, SELECT, [{nested: [1, 'two']}, 4], transaction));
  await push({
    v: PROTOCOL_VERSION,
    id: 116,
    method: 'commitTransaction',
    params: {transactionId: transaction},
  });
  await push(statement(117, 'DELETE FROM posts WHERE id = $1', [5]));
  await push(statement(118, SELECT, [5], transaction));
  await push({
    v: PROTOCOL_VERSION,
    id: 119,
    method: 'closePrepared',
    params: {statementId: prepared},
  });
  await push(execution(120, prepared, ['closed', 1]));
  // The owner's epoch is the only thing that differs from one run to another.
  return JSON.parse(JSON.stringify(answers).replaceAll(epoch, 'epoch')) as Posted[];
};

describe('statement requests through a database owner', () => {
  it('get the same answers at once, in turn, as requests, and from another tab', async () => {
    const run = async (
      how: 'at once' | 'in turn' | 'as requests' | 'from another tab',
    ) => {
      await tabs.closeAll();
      tabs = origin();
      const owner = tabs.openTab();
      await owner.open();
      const tab = how === 'from another tab' ? tabs.openTab() : owner;
      if (tab !== owner) await tab.open();
      const answers = await session(async (message) => {
        const id = Array.isArray(message) ? message[1] : message.id;
        if (how === 'in turn') {
          // Whatever arrives behind a request that waits must wait too.
          tab.send({v: PROTOCOL_VERSION, id: tab.id() + 1_000, method: 'schema', params: undefined});
        }
        tab.send(
          how === 'as requests' && Array.isArray(message)
            ? statementRequest(message)
            : message,
        );
        // Only the owner's own tab, with nothing waiting, is answered as the
        // message arrives, and only for what the owner can serve at once.
        expect(tab.answer(id) !== undefined).toBe(
          id === REFUSED_AT_ONCE ||
            ((how === 'at once' || how === 'as requests') && AT_ONCE.has(id)),
        );
        return tab.answered(id);
      });
      await settled();
      return {
        answers,
        calls: [...tabs.calls],
        inPlace: [...tabs.inPlace],
        revision: tabs.database.revision,
      };
    };
    const atOnce = await run('at once');
    for (const how of ['in turn', 'as requests', 'from another tab'] as const) {
      const other = await run(how);
      expect(other.answers).toEqual(atOnce.answers);
      expect(other.calls).toEqual(atOnce.calls);
      expect(other.revision).toBe(atOnce.revision);
      // In each of these ways a statement reaches the engine as a request's
      // parameters, never as the array it arrived as.
      expect(other.inPlace).toEqual(atOnce.calls.map(() => undefined));
    }
    // Served at once, every statement that reached the engine did so as the
    // array it arrived as: all of `AT_ONCE` but 111, which the host refused
    // before the engine.
    expect(atOnce.inPlace).toEqual([101, 102, 103, 105, 108, 109, 110, 114, 115, 117]);

    const byId = new Map(atOnce.answers.map((answer) => [idOf(answer), answer]));
    const lost = {
      code: 'TRANSACTION_LOST',
      message: 'The TinyJoin transaction ended when its database owner disconnected',
    };
    const gone = {
      code: 'PREPARED_STATEMENT_CLOSED',
      message: 'The prepared statement is no longer available',
    };
    const failure = (id: number, error: unknown) => ({
      v: PROTOCOL_VERSION,
      id,
      ok: false,
      error,
    });
    expect(byId.get(101)).toEqual([PROTOCOL_VERSION, 101, 3, 0, 1, expect.any(String)]);
    expect(byId.get(102)).toEqual([
      PROTOCOL_VERSION,
      102,
      3,
      0,
      1,
      sqlData([{name: 'id', dataTypeID: 20}], [[8]]),
    ]);
    // A write outside a transaction published, and so is a result.
    expect(byId.get(103)).toEqual({
      v: PROTOCOL_VERSION,
      id: 103,
      ok: true,
      result: {
        command: 'UPDATE',
        revision: 1,
        rowCount: 1,
        tables: ['posts'],
        keys: {posts: [{id: 5}]},
        data: sqlData([], []),
      },
    });
    expect(byId.get(105)).toMatchObject({ok: true, result: {revision: 2}});
    expect(byId.get(106)).toEqual(failure(106, gone));
    expect(byId.get(107)).toMatchObject({result: {transactionId: 'epoch/tx-1'}});
    expect(byId.get(108)).toEqual([PROTOCOL_VERSION, 108, 1, 2, 1, 'posts', 1, 'id', 5]);
    expect(byId.get(109)).toEqual([PROTOCOL_VERSION, 109, 1, 2, 1, 'posts', 1, 'id', 6]);
    expect(byId.get(110)).toMatchObject({
      ok: true,
      result: {command: 'INSERT', data: sqlData([{name: 'id', dataTypeID: 20}], [[7]])},
    });
    expect(byId.get(111)).toEqual(
      failure(111, {
        code: 'TRANSACTION_ACTIVE',
        message: 'Use the active TinyJoin transaction for this operation',
        retryable: false,
      }),
    );
    expect(byId.get(112)).toEqual(failure(112, lost));
    expect(byId.get(113)).toEqual(failure(113, lost));
    expect(byId.get(114)).toEqual(
      failure(114, {
        code: 'CONSTRAINT_VIOLATION',
        message: 'The engine refused CONSTRAINT_VIOLATION',
        retryable: false,
      }),
    );
    expect(byId.get(115)).toEqual([PROTOCOL_VERSION, 115, 3, 2, 1, expect.any(String)]);
    expect(byId.get(116)).toEqual({
      v: PROTOCOL_VERSION,
      id: 116,
      ok: true,
      result: {revision: 3},
    });
    expect(byId.get(117)).toMatchObject({ok: true, result: {command: 'DELETE', revision: 4}});
    expect(byId.get(118)).toEqual(failure(118, lost));
    expect(byId.get(120)).toEqual(failure(120, gone));

    // The engine saw each statement that reached it once, its parameters
    // alone, whichever way it came.
    expect(atOnce.calls.map(({params}) => params)).toEqual([
      [7],
      [8],
      ['outside', 5],
      ['prepared', 9],
      ['staged', 5],
      ['staged', 6],
      [7],
      ['CONSTRAINT_VIOLATION'],
      [{nested: [1, 'two']}, 4],
      [5],
    ]);
  });

  it('get the same answers in each of those ways for sessions made at random', async () => {
    const random = (seed: number) => () => {
      seed = (seed * 1_103_515_245 + 12_345) % 2_147_483_648;
      return seed / 2_147_483_648;
    };
    // A session that follows what the owner answers: it names the transaction
    // and the prepared statements it was given, and now and then one that is
    // finished, never begun, another owner's, or closed.
    const play = async (
      seed: number,
      ask: (message: StatementRequest | WorkerRequest) => Promise<Posted>,
    ): Promise<Posted[]> => {
      const next = random(seed);
      const pick = <Item>(items: readonly Item[]): Item =>
        items[Math.floor(next() * items.length)]!;
      const answers: Posted[] = [];
      const prepared: number[] = [];
      let id = 200;
      let transaction: string | undefined;
      let epoch = 'no transaction was begun';
      const named = (): string | 0 =>
        next() < 0.75
          ? (transaction ?? 0)
          : pick([0, `${epoch}/tx-999`, 'another-owner/tx-1', transaction ?? 0]);
      const params = (): JsonValue[] =>
        Array.from({length: Math.floor(next() * 3)}, () =>
          pick<JsonValue>([null, false, 3, 'text', {nested: [1, 'two']}]),
        );
      const request = (
        method: WorkerRequest['method'],
        requestParams?: unknown,
      ): WorkerRequest =>
        ({v: PROTOCOL_VERSION, id: id++, method, params: requestParams}) as WorkerRequest;
      for (let count = 10 + Math.floor(next() * 14); count > 0; count--) {
        const roll = next();
        let message: StatementRequest | WorkerRequest;
        if (roll < 0.4) {
          message = statement(
            id++,
            pick([SELECT, SELECT, UPDATE, UPDATE, RETURNING, FAIL]),
            // A failing statement names the code it fails with.
            ['CONSTRAINT_VIOLATION', ...params()],
            named(),
            pick([0, 0, 1]),
          );
        } else if (roll < 0.6) {
          message = execution(
            id++,
            pick([...prepared, ...prepared, 999]),
            params(),
            named(),
            pick([0, 1]),
          );
        } else if (roll < 0.7) {
          message = request('prepareSql', {sql: pick([SELECT, UPDATE])});
        } else if (roll < 0.74 && prepared.length > 0) {
          message = request('closePrepared', {statementId: pick(prepared)});
        } else if (roll < 0.84) {
          message = request('beginTransaction');
        } else if (roll < 0.94) {
          message = request(pick(['commitTransaction', 'rollbackTransaction']), {
            transactionId:
              next() < 0.85
                ? (transaction ?? `${epoch}/tx-999`)
                : `${epoch}/tx-999`,
          });
        } else {
          message = request('schema');
        }
        const answer = await ask(message);
        answers.push(answer);
        if (Array.isArray(message) || Array.isArray(answer) || !('ok' in answer) || !answer.ok) {
          continue;
        }
        const result = answer.result as Record<string, unknown> | undefined;
        if (message.method === 'prepareSql') {
          prepared.push(result!.statementId as number);
        } else if (message.method === 'beginTransaction') {
          transaction = result!.transactionId as string;
          epoch = transaction.slice(0, transaction.indexOf('/'));
        } else if (
          message.method === 'commitTransaction' ||
          message.method === 'rollbackTransaction'
        ) {
          transaction = undefined;
        }
      }
      // The owner's epoch is the only thing that differs from one run to another.
      return JSON.parse(JSON.stringify(answers).replaceAll(epoch, 'epoch')) as Posted[];
    };
    const run = async (
      seed: number,
      how: 'at once' | 'in turn' | 'as requests' | 'from another tab',
    ) => {
      await tabs.closeAll();
      tabs = origin();
      const owner = tabs.openTab();
      await owner.open();
      const tab = how === 'from another tab' ? tabs.openTab() : owner;
      if (tab !== owner) await tab.open();
      const answers = await play(seed, (message) => {
        if (how === 'in turn') {
          tab.send({v: PROTOCOL_VERSION, id: tab.id() + 1_000, method: 'schema', params: undefined});
        }
        tab.send(
          how === 'as requests' && Array.isArray(message)
            ? statementRequest(message)
            : message,
        );
        return tab.answered(Array.isArray(message) ? message[1] : message.id);
      });
      await settled();
      return {
        answers,
        calls: [...tabs.calls],
        inPlace: [...tabs.inPlace],
        revision: tabs.database.revision,
      };
    };

    let flat = 0;
    let failures = 0;
    let served = 0;
    for (const seed of [11, 23, 37, 41, 59, 67]) {
      const atOnce = await run(seed, 'at once');
      for (const how of ['in turn', 'as requests', 'from another tab'] as const) {
        const other = await run(seed, how);
        expect(other.answers).toEqual(atOnce.answers);
        expect(other.calls).toEqual(atOnce.calls);
        expect(other.revision).toBe(atOnce.revision);
        expect(other.inPlace).toEqual(atOnce.calls.map(() => undefined));
      }
      // Each statement is sent once the one before it is answered, so the
      // owner serves at once every one that it can serve at all, and each of
      // those reaches the engine as the array it arrived as.
      expect(atOnce.inPlace).not.toContain(undefined);
      served += atOnce.inPlace.length;
      for (const answer of atOnce.answers) {
        if (Array.isArray(answer)) {
          expect(isStatementResponse(answer, true)).toBe(true);
          flat += 1;
        } else if ('ok' in answer && !answer.ok) {
          failures += 1;
        }
      }
    }
    // The sessions met flat answers and failures alike, and many statements
    // that the engine ran.
    expect(flat).toBeGreaterThan(15);
    expect(failures).toBeGreaterThan(15);
    expect(served).toBeGreaterThan(30);
  });

  it('take their turn behind the opening of the database when they arrive before it is open', async () => {
    const tab = tabs.openTab();
    const init = tab.id();
    tab.send({
      v: PROTOCOL_VERSION,
      id: init,
      method: 'init',
      params: {storage: {kind: 'opfs', name: 'shared'}},
    });
    tab.send(statement(100, SELECT, [1]));
    tab.send(statement(101, UPDATE, ['early', 2]));
    // No owner has been elected yet, so each is held as the request it stands
    // for, and sent to the owner once there is one.
    expect(tab.posted).toEqual([]);
    await tab.answered(101);
    expect(tab.answers().map(idOf)).toEqual([init, 100, 101]);
    expect(tab.answer(100)).toEqual([PROTOCOL_VERSION, 100, 3, 0, 1, expect.any(String)]);
    expect(tab.answer(101)).toMatchObject({ok: true, result: {command: 'UPDATE'}});
    expect(tabs.calls.map(({params}) => params)).toEqual([[1], ['early', 2]]);
  });

  it('wait behind whatever arrived before them, and are answered in order', async () => {
    const tab = tabs.openTab();
    await tab.open();
    tab.posted.length = 0;

    // A request that the owner cannot serve at once holds back the statements
    // that arrive behind it in the same turn.
    tab.send({v: PROTOCOL_VERSION, id: 1_001, method: 'schema', params: undefined});
    tab.send(statement(1_002, SELECT, [2]));
    tab.send(statementRequest(statement(1_003, SELECT, [3])));
    tab.send(statement(1_004, UPDATE, ['four', 4]));
    tab.send(statement(1_005, SELECT, [5]));
    expect(tab.posted).toEqual([]);
    expect(tabs.calls).toEqual([]);

    await tab.answered(1_005);
    expect(tab.answers().map(idOf)).toEqual([1_001, 1_002, 1_003, 1_004, 1_005]);
    expect(tabs.calls.map(({params}) => params)).toEqual([[2], [3], ['four', 4], [5]]);
    // Each waited as the request it stands for, and reached the engine as one.
    expect(tabs.inPlace).toEqual([undefined, undefined, undefined, undefined]);

    // With nothing waiting, the next is served as it arrives again, from the
    // array it arrived as.
    tab.send(statement(1_006, SELECT, [6]));
    expect(idOf(tab.posted.at(-1)!)).toBe(1_006);
    expect(tabs.inPlace.at(-1)).toBe(1_006);
  });

  it('are refused when malformed, with the id when there is one', async () => {
    const tab = tabs.openTab();
    await tab.open();
    const sparse: unknown[] = statement(8, SELECT, [1, 2, 3]);
    delete sparse[7];
    const malformed: [message: unknown[], id: number][] = [
      [[99, 1, STATEMENT_SQL, SELECT, 0, 0], 1],
      [[PROTOCOL_VERSION, 2, STATEMENT_PREPARED, 'not an id', 0, 0], 2],
      [[PROTOCOL_VERSION, 3, 2, SELECT, 0, 0], 3],
      [[PROTOCOL_VERSION, 4, STATEMENT_SQL, SELECT, '', 0], 4],
      [[PROTOCOL_VERSION, 5, STATEMENT_SQL, SELECT, 0, 2], 5],
      [[PROTOCOL_VERSION, 6, STATEMENT_SQL, SELECT, 0, 0, Number.NaN], 6],
      [[PROTOCOL_VERSION, 7, STATEMENT_SQL, SELECT, 0], 7],
      [sparse, 8],
      // This Worker has always echoed any number as the id.
      [[PROTOCOL_VERSION, 1.5, STATEMENT_SQL, SELECT, 0, 0], 1.5],
      [[PROTOCOL_VERSION, '9', STATEMENT_SQL, SELECT, 0, 0], 0],
      [[PROTOCOL_VERSION], 0],
      [[], 0],
    ];
    for (const [message, id] of malformed) {
      tab.posted.length = 0;
      tab.send(message);
      expect(tab.posted).toEqual([
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
    expect(tabs.calls).toEqual([]);
    tab.send(statement(10, SELECT, [1]));
    expect(idOf(tab.posted.at(-1)!)).toBe(10);
  });

  it('are bounded as their requests are, even with nothing waiting', async () => {
    const tab = tabs.openTab();
    await tab.open();
    tab.posted.length = 0;
    const full = {
      code: 'RESOURCE_LIMIT',
      message: 'The TinyJoin client request queue is full',
    };

    // The largest statement the queue takes, to the byte, is served, and one
    // a character longer is refused before it reaches the engine.
    const fill = (MAX_QUEUED_BYTES - 356 - SELECT.length * 2 - 8 - 16) / 2;
    tab.send(statement(1, SELECT, ['x'.repeat(fill)]));
    tab.send(statement(2, SELECT, ['x'.repeat(fill + 1)]));
    tab.send(statementRequest(statement(3, SELECT, ['x'.repeat(fill)])));
    tab.send(statementRequest(statement(4, SELECT, ['x'.repeat(fill + 1)])));
    expect(tab.posted.map((message) => (Array.isArray(message) ? idOf(message) : message))).toEqual([
      1,
      {v: PROTOCOL_VERSION, id: 2, ok: false, error: full},
      3,
      {v: PROTOCOL_VERSION, id: 4, ok: false, error: full},
    ]);
    expect(tabs.calls).toHaveLength(2);

    // Behind a request that waits, the queue takes so many, and so much.
    tab.posted.length = 0;
    tabs.calls.length = 0;
    tab.send({v: PROTOCOL_VERSION, id: 1_000, method: 'schema', params: undefined});
    for (let index = 1; index < MAX_PENDING_REQUESTS; index++) {
      tab.send(statement(1_000 + index, SELECT, [index]));
    }
    tab.send(statement(2_000, SELECT, [0]));
    expect(tab.posted).toEqual([
      {v: PROTOCOL_VERSION, id: 2_000, ok: false, error: full},
    ]);
    await tab.answered(1_000 + MAX_PENDING_REQUESTS - 1);
    expect(tab.answers().map(idOf)).toEqual([
      2_000,
      ...Array.from({length: MAX_PENDING_REQUESTS}, (_, index) => 1_000 + index),
    ]);
    expect(tabs.calls).toHaveLength(MAX_PENDING_REQUESTS - 1);

    tab.posted.length = 0;
    const half = 'x'.repeat(MAX_QUEUED_BYTES / 4);
    tab.send({v: PROTOCOL_VERSION, id: 3_000, method: 'schema', params: undefined});
    tab.send(statement(3_001, SELECT, [half]));
    tab.send(statement(3_002, SELECT, [half]));
    expect(tab.posted).toEqual([
      {v: PROTOCOL_VERSION, id: 3_002, ok: false, error: full},
    ]);
    await tab.answered(3_001);
  });

  it('answer a result that is not flat in a response, and note only what was published', async () => {
    const tab = tabs.openTab();
    await tab.open();
    tab.posted.length = 0;
    // The owner tells its page the revision it has noted when asked to
    // refresh it.
    const revisionNoted = async (): Promise<number> => {
      tab.posted.length = 0;
      tab.send({tinyjoin: 'resync'});
      await settled();
      const [event] = tab.events();
      tab.posted.length = 0;
      expect(event).toMatchObject({event: 'resync'});
      return event!.payload.revision;
    };

    // A flat result holds no news, whatever revision it claims.
    tab.send(statement(1, 'SELECT FUTURE', [1]));
    expect(tab.posted).toEqual([[PROTOCOL_VERSION, 1, 3, 99, 1, expect.any(String)]]);
    expect(await revisionNoted()).toBe(0);

    // A result that published is noted, and announced to every tab.
    tab.send(statement(2, UPDATE, ['changed', 5]));
    expect(tab.posted).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 2,
        ok: true,
        result: {
          command: 'UPDATE',
          revision: 1,
          rowCount: 1,
          tables: ['posts'],
          keys: {posts: [{id: 5}]},
          data: sqlData([], []),
        },
      },
    ]);
    await settled();
    expect(tab.events()).toEqual([
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables: ['posts'], keys: {posts: [{id: 5}]}},
      },
    ]);
    expect(await revisionNoted()).toBe(1);
  });

  it('answer a failure with its error, and retire an owner whose engine is beyond use', async () => {
    const tab = tabs.openTab();
    await tab.open();
    tab.posted.length = 0;

    tab.send(statement(1, FAIL, ['CONSTRAINT_VIOLATION']));
    tab.send(statement(2, SELECT, [2]));
    expect(tab.posted).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 1,
        ok: false,
        error: {
          code: 'CONSTRAINT_VIOLATION',
          message: 'The engine refused CONSTRAINT_VIOLATION',
          retryable: false,
        },
      },
      [PROTOCOL_VERSION, 2, 3, 0, 1, expect.any(String)],
    ]);

    // An error that leaves the engine beyond use is answered, and then fails
    // everything after it without the engine being asked again.
    tab.posted.length = 0;
    tabs.calls.length = 0;
    const poisoned = {
      code: 'STORAGE_ENGINE_POISONED',
      message: 'The engine refused STORAGE_ENGINE_POISONED',
      retryable: false,
    };
    tab.send(statement(3, FAIL, ['STORAGE_ENGINE_POISONED']));
    tab.send(statement(4, SELECT, [4]));
    tab.send(statementRequest(statement(5, SELECT, [5])));
    expect(tab.posted).toEqual([
      {v: PROTOCOL_VERSION, id: 3, ok: false, error: poisoned},
      {v: PROTOCOL_VERSION, id: 4, ok: false, error: poisoned},
      {v: PROTOCOL_VERSION, id: 5, ok: false, error: poisoned},
    ]);
    expect(tabs.calls).toHaveLength(1);
    // The retired owner gives up the database for another tab to open.
    await vi.waitFor(
      () => expect(tabs.locksHeld()).not.toContain('tinyjoin:database:shared'),
      {interval: 1},
    );
  });
});

describe("another tab's statement requests", () => {
  // The owner's tab and another, both open, and the owner's epoch, which its
  // transaction ids begin with.
  const twoTabs = async () => {
    const owner = tabs.openTab();
    await owner.open();
    const other = tabs.openTab();
    await other.open();
    const {transactionId} = (await owner.request('beginTransaction')) as {
      transactionId: string;
    };
    await owner.request('rollbackTransaction', {transactionId});
    const epoch = transactionId.slice(0, transactionId.indexOf('/'));
    owner.posted.length = 0;
    other.posted.length = 0;
    tabs.calls.length = 0;
    tabs.inPlace.length = 0;
    return {owner, other, epoch};
  };
  // Everything a channel carries, as a third party on it hears it.
  const overhear = (name: string): unknown[] => {
    const heard: unknown[] = [];
    tabs.channel(name).onmessage = (event) => void heard.push(event.data);
    return heard;
  };

  it('cross to the owner as the requests they stand for, and come back flat', async () => {
    const {other, epoch} = await twoTabs();
    const routed = overhear(ownerChannel(epoch)) as {
      client: string;
      request: WorkerRequest;
      statementSql?: string;
    }[];

    const select = statement(50, SELECT, [5], 0, 1);
    other.send(select);
    // Nothing is answered until the owner has had the message.
    expect(other.posted).toEqual([]);
    expect(await other.answered(50)).toEqual([
      PROTOCOL_VERSION,
      50,
      3,
      0,
      1,
      sqlData([{name: 'id', dataTypeID: 20}], [[5]]),
    ]);
    expect(routed).toHaveLength(1);
    expect(routed[0]).toMatchObject({kind: 'request', epoch});
    expect(routed[0]!.request).toEqual(statementRequest(select));
    expect(Object.keys(routed[0]!.request.params!)).toEqual(['sql', 'params', 'rowMode']);

    // The owner's answer crosses as the message the other tab's page reads.
    const replies = overhear(clientChannel(routed[0]!.client));
    const prepared = (
      (await other.request('prepareSql', {sql: UPDATE})) as {statementId: number}
    ).statementId;
    const {transactionId} = (await other.request('beginTransaction')) as {
      transactionId: string;
    };
    replies.length = 0;
    routed.length = 0;
    const update = execution(51, prepared, ['routed', 6], transactionId);
    other.send(update);
    const flat = [PROTOCOL_VERSION, 51, 1, 0, 1, 'posts', 1, 'id', 6];
    expect(await other.answered(51)).toEqual(flat);
    expect(replies).toEqual([{epoch, response: flat}]);
    // The request carries the statement's text, for an owner that has yet to
    // prepare it, and the owner's own name for the transaction.
    expect(routed[0]).toMatchObject({
      request: statementRequest(update),
      statementSql: UPDATE,
    });
    expect(tabs.calls.at(-1)).toEqual({
      target: 1,
      sql: UPDATE,
      params: ['routed', 6],
      rowMode: undefined,
    });

    // A result that is not flat crosses in a response, as it always has.
    other.send(statement(52, RETURNING, [7], transactionId));
    expect(await other.answered(52)).toMatchObject({
      id: 52,
      ok: true,
      result: {command: 'INSERT', keys: {posts: [{id: 7}]}},
    });
    // None of the three reached the engine as an array: the owner serves
    // another tab's statement as the request that crossed to it.
    expect(tabs.inPlace).toEqual([undefined, undefined, undefined]);
    expect(await other.request('commitTransaction', {transactionId})).toEqual({
      revision: 1,
    });
    // Both tabs hear what the commit changed.
    await settled();
    expect(other.events()).toEqual([
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables: ['posts'], keys: {}},
      },
    ]);
  });

  it('are refused by their own Worker when malformed or too large', async () => {
    const {other} = await twoTabs();
    other.send([PROTOCOL_VERSION, 60, STATEMENT_SQL, SELECT, 0, 2]);
    other.send(statement(61, SELECT, ['x'.repeat(MAX_QUEUED_BYTES / 2)]));
    expect(other.posted).toEqual([
      {
        v: PROTOCOL_VERSION,
        id: 60,
        ok: false,
        error: {
          code: 'PROTOCOL_MISMATCH',
          message: 'The worker received an invalid TinyJoin protocol request',
        },
      },
      {
        v: PROTOCOL_VERSION,
        id: 61,
        ok: false,
        error: {
          code: 'RESOURCE_LIMIT',
          message: 'The TinyJoin client request queue is full',
        },
      },
    ]);
    await settled();
    expect(tabs.calls).toEqual([]);
  });

  it('take a flat response only from their owner, in this protocol, for a request that waits', async () => {
    const {owner, other, epoch} = await twoTabs();
    const routed = overhear(ownerChannel(epoch)) as {client: string}[];

    // The owner's own transaction holds the other tab's statement in its
    // queue, so that the statement stays unanswered.
    const {transactionId} = (await owner.request('beginTransaction')) as {
      transactionId: string;
    };
    other.send(statement(70, SELECT, [7]));
    await settled();
    expect(other.posted).toEqual([]);
    const intruder = tabs.channel(clientChannel(routed[0]!.client));
    const forged = [PROTOCOL_VERSION, 70, 3, 0, 1, sqlData([], [])];

    // Not from another owner's epoch, nor of another protocol, nor for a
    // request that is not waiting, nor with anything but an id where a
    // response holds its request's.
    intruder.postMessage({epoch: 'another-epoch', response: forged});
    intruder.postMessage({epoch, response: [PROTOCOL_VERSION - 1, 70, 3, 0, 1, '']});
    intruder.postMessage({epoch, response: [PROTOCOL_VERSION, 71, 3, 0, 1, '']});
    intruder.postMessage({epoch, response: [PROTOCOL_VERSION, '70', 3, 0, 1, '']});
    intruder.postMessage({epoch, response: [PROTOCOL_VERSION]});
    intruder.postMessage({epoch, response: []});
    await settled();
    expect(other.posted).toEqual([]);

    // One of this protocol that names the waiting request is its answer, as
    // far as this Worker can tell, and the request waits no more: the owner's
    // own answer finds nothing.
    intruder.postMessage({epoch, response: forged});
    await settled();
    expect(other.posted).toEqual([forged]);
    await owner.request('rollbackTransaction', {transactionId});
    await settled();
    expect(other.posted).toEqual([forged]);
    expect(tabs.calls.map(({params}) => params)).toEqual([[7]]);
  });

  it('are answered with what the owner sent even when their page cannot read it, in either form, and wait for it no more', async () => {
    const {owner, other, epoch} = await twoTabs();
    const routed = overhear(ownerChannel(epoch)) as {client: string}[];
    const {transactionId} = (await owner.request('beginTransaction')) as {
      transactionId: string;
    };
    for (let id = 70; id <= 76; id++) {
      other.send(statement(id, SELECT, [id]));
    }
    await settled();
    expect(other.posted).toEqual([]);
    const intruder = tabs.channel(clientChannel(routed[0]!.client));

    // A response is taken by its envelope: what its result holds is the
    // page's to check, and a page that cannot read it ends its connection.
    // This Worker has always passed such a response on, here one whose row
    // count is no count.
    const response = {
      v: PROTOCOL_VERSION,
      id: 70,
      ok: true,
      result: {
        command: 'SELECT',
        revision: 0,
        rowCount: -1,
        tables: [],
        keys: {},
        data: sqlData([], []),
      },
    };
    // The same defect in a flat array, and each other way one can be wrong
    // after its id: a read without its rows, a command that is none, a
    // revision that is no count, key columns that are not there, and nothing
    // at all.
    const arrays = [
      [PROTOCOL_VERSION, 71, 3, 0, -1, sqlData([], [])],
      [PROTOCOL_VERSION, 72, 3, 0, 1],
      [PROTOCOL_VERSION, 73, 9, 0, 1],
      [PROTOCOL_VERSION, 74, 1, 1.5, 1, 'posts'],
      [PROTOCOL_VERSION, 75, 1, 0, 1, 'posts', 2, 'id'],
      [PROTOCOL_VERSION, 76],
    ];
    for (const array of arrays) {
      expect(isStatementResponse(array, false)).toBe(false);
    }
    for (const answer of [response, ...arrays]) {
      intruder.postMessage({epoch, response: answer});
    }
    await settled();
    // Each is passed on as it came, for the page to refuse where that can be
    // seen. Held back here, its statement would wait for ever.
    expect(other.posted).toEqual([response, ...arrays]);

    // None of the requests waits any more: the owner's own answers, once its
    // transaction has ended and it has run each statement, find nothing.
    await owner.request('rollbackTransaction', {transactionId});
    await settled();
    expect(tabs.calls.map(({params}) => params)).toEqual([
      [70],
      [71],
      [72],
      [73],
      [74],
      [75],
      [76],
    ]);
    expect(other.posted).toEqual([response, ...arrays]);
  });

  it('are all that a flat response answers: one that names another request is passed on, and confirms nothing of it', async () => {
    const {owner, other, epoch} = await twoTabs();
    const routed = overhear(ownerChannel(epoch)) as {
      client: string;
      request: WorkerRequest;
      statementSql?: string;
    }[];
    const prepared = (
      (await other.request('prepareSql', {sql: UPDATE})) as {statementId: number}
    ).statementId;
    const {transactionId} = (await owner.request('beginTransaction')) as {
      transactionId: string;
    };
    other.posted.length = 0;
    routed.length = 0;

    // Behind the owner's own transaction, the other tab's requests wait in
    // the owner's queue: to begin a transaction, and to close its statement.
    const begin = other.id();
    const close = other.id();
    other.send({v: PROTOCOL_VERSION, id: begin, method: 'beginTransaction', params: undefined});
    other.send({
      v: PROTOCOL_VERSION,
      id: close,
      method: 'closePrepared',
      params: {statementId: prepared},
    });
    await settled();
    expect(other.posted).toEqual([]);
    expect(routed.map(({request}) => request.id)).toEqual([begin, close]);

    // Only a statement is answered with a flat array, so one that names any
    // other request is not an answer its page can read. It is passed on as
    // it came all the same, for the page to refuse, rather than held back to
    // leave the request waiting.
    const intruder = tabs.channel(clientChannel(routed[0]!.client));
    const answers = [
      [PROTOCOL_VERSION, begin, 1, 0, 1, 'posts'],
      [PROTOCOL_VERSION, close, 3, 0, 0, sqlData([], [])],
    ];
    for (const answer of answers) {
      intruder.postMessage({epoch, response: answer});
    }
    await settled();
    expect(other.posted).toEqual(answers);

    // It confirmed nothing of the request it named. The statement was not
    // closed as far as this Worker knows, which still sends its text with an
    // execution, for an owner that has yet to prepare it.
    routed.length = 0;
    other.send(execution(90, prepared, ['kept', 1]));
    await settled();
    expect(routed).toHaveLength(1);
    expect(routed[0]).toMatchObject({request: {id: 90}, statementSql: UPDATE});

    // And neither request waits any more: when the owner's transaction has
    // ended and it answers them itself, its answers find nothing.
    await owner.request('rollbackTransaction', {transactionId});
    await other.answered(90);
    await settled();
    expect(other.answers().map(idOf)).toEqual([begin, close, 90]);
    expect(other.posted.slice(0, 2)).toEqual(answers);
  });

  it('fail with the owner, and are prepared again by the tab that takes its place', async () => {
    const {owner, other, epoch} = await twoTabs();
    const prepared = (
      (await other.request('prepareSql', {sql: UPDATE})) as {statementId: number}
    ).statementId;
    const {transactionId} = (await other.request('beginTransaction')) as {
      transactionId: string;
    };
    other.send(execution(80, prepared, ['before', 1], transactionId));
    expect(await other.answered(80)).toEqual([
      PROTOCOL_VERSION,
      80,
      1,
      0,
      1,
      'posts',
      1,
      'id',
      1,
    ]);

    // The owner goes while a statement is on its way to it: whether it ran is
    // not known, which is what the statement's failure says.
    other.posted.length = 0;
    other.send(execution(81, prepared, ['during', 2], transactionId));
    await owner.close();
    expect(await other.answered(81)).toEqual({
      v: PROTOCOL_VERSION,
      id: 81,
      ok: false,
      error: {
        code: 'LEADER_CHANGED',
        message:
          'The database owner changed. The pending operation may have committed; reconcile its outcome before retrying.',
      },
    });

    // The other tab now opens the database itself, and the old owner's
    // transaction went with the old owner.
    await vi.waitFor(() => expect(tabs.database.opened).toBe(2), {interval: 1});
    other.send(execution(82, prepared, ['lost', 3], transactionId));
    expect(await other.answered(82)).toEqual({
      v: PROTOCOL_VERSION,
      id: 82,
      ok: false,
      error: {
        code: 'TRANSACTION_LOST',
        message:
          'The TinyJoin transaction ended when its database owner disconnected',
      },
    });

    // Its engine has yet to see the prepared statement: the first execution
    // takes its turn, to be prepared again from the text its Worker kept, and
    // the next is served at once.
    other.posted.length = 0;
    tabs.calls.length = 0;
    other.send(execution(83, prepared, ['again', 4]));
    expect(other.posted).toEqual([]);
    expect(await other.answered(83)).toMatchObject({
      ok: true,
      result: {command: 'UPDATE', keys: {posts: [{id: 4}]}},
    });
    other.send(execution(84, prepared, ['at once', 5]));
    expect(other.answer(84)).toMatchObject({
      ok: true,
      result: {command: 'UPDATE', keys: {posts: [{id: 5}]}},
    });
    // The new owner's transactions go by its own epoch.
    const {transactionId: next} = (await other.request('beginTransaction')) as {
      transactionId: string;
    };
    expect(next.startsWith(`${epoch}/`)).toBe(false);
    other.send(execution(85, prepared, ['inside', 6], next));
    expect(other.answer(85)).toEqual([
      PROTOCOL_VERSION,
      85,
      1,
      2,
      1,
      'posts',
      1,
      'id',
      6,
    ]);
    tabs.calls.pop();
    expect(tabs.calls).toEqual([
      {target: 1, sql: UPDATE, params: ['again', 4], rowMode: undefined},
      {target: 1, sql: UPDATE, params: ['at once', 5], rowMode: undefined},
    ]);
  });
});
