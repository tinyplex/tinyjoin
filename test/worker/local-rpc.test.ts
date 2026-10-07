import {describe, expect, it, vi} from 'vitest';

import {
  PROTOCOL_VERSION,
  STATEMENT_PARAMS,
  STATEMENT_SQL,
  type JsonValue,
  type StatementRequest,
  type WorkerEvent,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {createLocalRpc} from '../../src/worker/local-rpc.ts';

// The connection's host opens its database through the private OPFS runtime,
// which these tests replace with an engine of their own.
const runtime = vi.hoisted(() => ({
  open: (): unknown => {
    throw new Error('No engine fixture is installed');
  },
}));

vi.mock('../../src/worker/opfs-loader.ts', () => ({
  createOpfsWasmEngine: async () => runtime.open(),
}));

const SELECT = 'SELECT id FROM posts WHERE id = $1';
const UPDATE = 'UPDATE posts SET title = $1 WHERE id = $2';

const statement = (
  sql: string,
  params: JsonValue[],
  transaction: string | 0 = 0,
): StatementRequest => [
  PROTOCOL_VERSION,
  9,
  STATEMENT_SQL,
  sql,
  transaction,
  0,
  ...params,
];

/**
 * An engine that answers a read with the flat array of its result, as the
 * bridge does, and a write with a result that published a revision.
 */
function engine() {
  let revision = 0;
  const executeSql = vi.fn<WorkerEngine['executeSql']>(
    (sql, params, _rowMode, from = 0) => {
      if (sql === 'FAIL') {
        throw Object.assign(new Error('The engine refused'), {
          code: 'CONSTRAINT_VIOLATION',
        });
      }
      return sql.startsWith('SELECT')
        ? [0, 0, 3, revision, 1, JSON.stringify(params.slice(from))]
        : {
            command: 'UPDATE',
            revision: ++revision,
            rowCount: 1,
            tables: ['posts'],
            keys: {},
            data: '{"fields":[],"rows":[]}',
          };
    },
  );
  return {
    executeSql,
    prepareSql: () => 1,
    executePrepared: () => {
      throw new Error('Nothing is prepared');
    },
    closePrepared: () => undefined,
    execSql: () => [],
    beginTransaction: () => undefined,
    commitTransaction: () => ({revision, tables: [], keys: {}}),
    rollbackTransaction: () => undefined,
    inTransaction: () => false,
    revision: () => revision,
    check: () => undefined,
    schema: () => ({version: 0, tables: []}),
    setSchema: () => ({revision, tables: [], keys: {}}),
    close: vi.fn(),
  } satisfies WorkerEngine;
}

const open = async () => {
  const opened = engine();
  runtime.open = () => opened;
  const rpc = createLocalRpc();
  await rpc.request('init', {storage: {kind: 'opfs', name: 'local'}});
  return {rpc, engine: opened};
};

const nextTask = (): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, 0));

describe('a connection to a host in the same Worker', () => {
  it('serves statement requests at once, as calls, and passes on what the host announces', async () => {
    const {rpc, engine} = await open();
    const events: WorkerEvent[] = [];
    rpc.onEvent((event) => void events.push(event));

    // The result comes back as the engine returned it, read from the request
    // where its parameters stand.
    const select = statement(SELECT, [7]);
    expect(rpc.statementNow(select, SELECT, undefined)).toEqual([
      0,
      0,
      3,
      0,
      1,
      '[7]',
    ]);
    expect(engine.executeSql).toHaveBeenLastCalledWith(
      SELECT,
      select,
      undefined,
      STATEMENT_PARAMS,
    );
    // The same statement as a request is served at once too.
    expect(rpc.requestNow('executeSql', {sql: SELECT, params: [8]})).toEqual({
      ok: true,
      value: [0, 0, 3, 0, 1, '[8]'],
    });

    // A failure is thrown as the error a response would carry.
    expect(() => rpc.statementNow(statement('FAIL', []), 'FAIL', undefined)).toThrow(
      expect.objectContaining({
        name: 'ClientError',
        code: 'CONSTRAINT_VIOLATION',
        message: 'The engine refused',
        retryable: false,
      }),
    );
    expect(() =>
      rpc.statementNow(statement(SELECT, [1], 'tx-1'), SELECT, 'tx-1'),
    ).toThrow(expect.objectContaining({code: 'TRANSACTION_NOT_ACTIVE'}));

    // A result that published is announced to the connection's listeners.
    expect(
      rpc.statementNow(statement(UPDATE, ['changed', 1]), UPDATE, undefined),
    ).toMatchObject({command: 'UPDATE', revision: 1});
    expect(events).toEqual([]);
    await nextTask();
    expect(events).toEqual([
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables: ['posts'], keys: {}},
      },
    ]);
  });

  it('serves nothing once it is disposed', async () => {
    const {rpc, engine} = await open();
    const events: WorkerEvent[] = [];
    rpc.onEvent((event) => void events.push(event));
    expect(
      rpc.statementNow(statement(UPDATE, ['changed', 1]), UPDATE, undefined),
    ).toMatchObject({revision: 1});
    engine.executeSql.mockClear();

    rpc.dispose();
    // Neither lane serves, so whoever asks falls back to a request, which is
    // refused: the engine is not reached again.
    expect(rpc.statementNow(statement(SELECT, [7]), SELECT, undefined)).toBeUndefined();
    expect(rpc.requestNow('executeSql', {sql: SELECT, params: [7]})).toBeUndefined();
    await expect(
      rpc.request('executeSql', {sql: SELECT, params: [7]}),
    ).rejects.toMatchObject({code: 'WORKER_TERMINATED'});
    expect(engine.executeSql).not.toHaveBeenCalled();
    // Nor does it pass on what was announced before.
    await nextTask();
    expect(events).toEqual([]);
  });
});
