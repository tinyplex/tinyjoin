import {describe, expect, it, vi} from 'vitest';

import {WorkerRpc} from '../../src/client/rpc.ts';
import {
  PROTOCOL_VERSION,
  isRpcResult,
  isRpcResultHeader,
  isWorkerRequest,
  type WorkerRequest,
} from '../../src/protocol.ts';
import {FakeWorker} from '../helpers/fake-worker.ts';

describe('WorkerRpc', () => {
  it('matches out-of-order responses to their requests', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const first = rpc.request('executeSql', {sql: 'first', params: []});
    const second = rpc.request('executeSql', {sql: 'second', params: []});
    const [firstRequest, secondRequest] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: secondRequest!.id,
      ok: true,
      result: sqlResult(2, [{id: 2}]),
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: firstRequest!.id,
      ok: true,
      result: sqlResult(1, [{id: 1}]),
    });

    await expect(first).resolves.toEqual(sqlResult(1, [{id: 1}]));
    await expect(second).resolves.toEqual(sqlResult(2, [{id: 2}]));
  });

  it('turns structured worker failures into TinyGres errors', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('executeSql', {sql: 'bad', params: []});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: false,
      error: {code: 'UNSUPPORTED_SQL', message: 'No joins yet'},
    });

    await expect(request).rejects.toMatchObject({
      code: 'UNSUPPORTED_SQL',
      message: 'No joins yet',
    });
  });

  it('delivers events without consuming pending responses', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const listener = vi.fn();
    rpc.onEvent(listener);
    const request = rpc.request('executeSql', {sql: 'select', params: []});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 3, tables: ['posts']},
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: true,
      result: sqlResult(3),
    });

    expect(listener).toHaveBeenCalledOnce();
    await expect(request).resolves.toEqual(sqlResult(3));
  });

  it('rejects a malformed SQL query success and terminates the worker', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('executeSql', {
      sql: 'SELECT id FROM posts',
      params: [],
    });
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: true,
      result: {...sqlResult(0), extra: true},
    });

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('rejects malformed exec result metadata and terminates the worker', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('execSql', {sql: 'SELECT id FROM posts'});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: true,
      result: [
        {
          ...sqlResult(0, [{id: 1}]),
          fields: [{name: 'id', dataTypeID: -1}],
        },
      ],
    });

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('checks only fixed metadata for a trusted bundled Worker result', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker, 'header');
    const request = rpc.request('executeSql', {sql: 'SELECT id FROM posts', params: []});
    const [message] = worker.posted as WorkerRequest[];
    const row = {};
    Object.defineProperty(row, 'id', {
      enumerable: true,
      get() {
        throw new Error('The client revisited a trusted result cell');
      },
    });
    const result = sqlResult(1, [row as {id: number}]);

    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: true,
      result,
    });

    await expect(request).resolves.toBe(result);
    expect(worker.terminated).toBe(false);
  });

  it('validates every method-specific success shape', () => {
    const outcome = {revision: 1, tables: ['posts']};

    expect(isRpcResult('init', {revision: 0})).toBe(true);
    expect(isRpcResult('executeSql', sqlResult(1, [{id: 1}]))).toBe(true);
    expect(isRpcResult('prepareSql', {statementId: 1})).toBe(true);
    expect(isRpcResult('executePrepared', sqlResult(1, [{id: 1}]))).toBe(
      true,
    );
    expect(isRpcResult('closePrepared', undefined)).toBe(true);
    expect(isRpcResult('execSql', [sqlResult(1)])).toBe(true);
    expect(isRpcResult('beginTransaction', {transactionId: 'tx-1'})).toBe(
      true,
    );
    expect(isRpcResult('commitTransaction', outcome)).toBe(true);
    expect(isRpcResult('rollbackTransaction', undefined)).toBe(true);
    expect(isRpcResult('close', undefined)).toBe(true);

    expect(isRpcResult('init', {revision: -1})).toBe(false);
    expect(
      isRpcResult('executeSql', {
        ...sqlResult(1),
        rows: [{created: new Date()}],
      }),
    ).toBe(false);
    expect(
      isRpcResult('executeSql', {
        ...sqlResult(1),
        rowCount: Number.MAX_SAFE_INTEGER + 1,
      }),
    ).toBe(false);
    expect(isRpcResult('prepareSql', {statementId: 0})).toBe(false);
    expect(isRpcResult('prepareSql', {statementId: 1, extra: true})).toBe(
      false,
    );
    expect(isRpcResult('closePrepared', null)).toBe(false);
    expect(isRpcResult('execSql', [sqlResult(-1)])).toBe(false);
    expect(
      isRpcResult('beginTransaction', {
        transactionId: 'tx-1',
        extra: true,
      }),
    ).toBe(false);
    expect(isRpcResult('rollbackTransaction', {})).toBe(false);
    expect(isRpcResult('close', null)).toBe(false);

    expect(
      isRpcResultHeader('executeSql', {
        ...sqlResult(1),
        rows: [{created: new Date()}],
      }),
    ).toBe(true);
    expect(
      isRpcResultHeader('executeSql', {
        ...sqlResult(1),
        rows: 'not-an-array',
      }),
    ).toBe(false);
  });

  it('rejects extra request envelope and parameter keys', () => {
    const requests = [
      {
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'init',
        params: {storage: {kind: 'memory'}},
      },
      {
        v: PROTOCOL_VERSION,
        id: 2,
        method: 'executeSql',
        params: {sql: 'SELECT id FROM posts', params: []},
      },
    ] as const;

    for (const request of requests) {
      expect(isWorkerRequest(request)).toBe(true);
      expect(isWorkerRequest({...request, extra: true})).toBe(false);
      expect(
        isWorkerRequest({
          ...request,
          params: {...request.params, extra: true},
        }),
      ).toBe(false);
    }

    expect(
      isWorkerRequest({
        ...requests[0],
        params: {storage: {kind: 'memory', extra: true}},
      }),
    ).toBe(false);
    for (const request of [
      {
        v: PROTOCOL_VERSION,
        id: 7,
        method: 'prepareSql',
        params: {sql: 'SELECT id FROM posts WHERE id = $1'},
      },
      {
        v: PROTOCOL_VERSION,
        id: 8,
        method: 'executePrepared',
        params: {statementId: 1, params: [1], transactionId: 'tx-1'},
      },
      {
        v: PROTOCOL_VERSION,
        id: 9,
        method: 'closePrepared',
        params: {statementId: 1},
      },
    ] as const) {
      expect(isWorkerRequest(request)).toBe(true);
      expect(
        isWorkerRequest({...request, params: {...request.params, extra: true}}),
      ).toBe(false);
    }
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 10,
        method: 'executePrepared',
        params: {statementId: 0, params: []},
      }),
    ).toBe(false);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 11,
        method: 'executePrepared',
        params: {statementId: 1, params: [1n]},
      }),
    ).toBe(false);
  });

  it('rejects every pending request after a protocol mismatch', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('executeSql', {sql: 'select', params: []});

    worker.emitInvalidMessage({v: 999, id: 1, ok: true, result: []});

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('rejects pending work when the worker crashes', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('executeSql', {sql: 'select', params: []});

    worker.emitError('boom');

    await expect(request).rejects.toMatchObject({
      code: 'WORKER_ERROR',
      message: 'boom',
    });
  });
});

function sqlResult(revision: number, rows: Array<{id: number}> = []) {
  return {
    command: 'SELECT',
    fields: rows.length > 0 ? [{name: 'id', dataTypeID: 20}] : [],
    revision,
    rowCount: rows.length,
    rows,
    tables: [],
  };
}
