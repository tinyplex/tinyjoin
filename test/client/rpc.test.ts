import {describe, expect, it, vi} from 'vitest';

import {createWorkerRpc} from '../../src/client/rpc.ts';
import {
  PROTOCOL_VERSION,
  isRpcResult,
  isRpcResultHeader,
  isSerializedError,
  isWorkerRequest,
  type JsonValue,
  type WorkerRequest,
} from '../../src/protocol.ts';
import {FakeWorker} from '../helpers/fake-worker.ts';

describe('WorkerRpc', () => {
  it('matches out-of-order responses to their requests', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker);
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

  it('turns structured worker failures into TinyJoin errors', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker);
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
    const rpc = createWorkerRpc(worker);
    const listener = vi.fn();
    rpc.onEvent(listener);
    const request = rpc.request('executeSql', {sql: 'select', params: []});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 3, tables: ['posts'], keys: {}},
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

  it('reads a statement result sent as text, and fails alone one it cannot read', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker);
    const asText = ({data, ...header}: ReturnType<typeof sqlResult>) => [
      JSON.stringify(header),
      data,
    ];
    const read = rpc.request('executePrepared', {statementId: 1, params: []});
    const unreadable = rpc.request('executeSql', {sql: 'SELECT 1', params: []});
    const invalid = rpc.request('executeSql', {sql: 'SELECT 2', params: []});
    const after = rpc.request('executeSql', {sql: 'SELECT 3', params: []});
    const [first, second, third, fourth] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: first!.id,
      ok: true,
      result: asText(sqlResult(4, [{id: 7}])),
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: second!.id,
      ok: true,
      result: ['{"command":', sqlData([], [])],
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: third!.id,
      ok: true,
      result: asText({...sqlResult(4), rowCount: -1}),
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: fourth!.id,
      ok: true,
      result: sqlResult(5),
    });

    await expect(read).resolves.toEqual(sqlResult(4, [{id: 7}]));
    for (const failed of [unreadable, invalid]) {
      await expect(failed).rejects.toMatchObject({
        code: 'BRIDGE_SERIALIZATION_ERROR',
      });
    }
    await expect(after).resolves.toEqual(sqlResult(5));
    expect(worker.terminated).toBe(false);
  });

  it('rejects a result sent as text for anything but a statement', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker);
    const request = rpc.request('execSql', {sql: 'SELECT 1'});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: true,
      result: ['{}', sqlData([], [])],
    });

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('rejects a malformed SQL query success and terminates the worker', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker);
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
    const rpc = createWorkerRpc(worker);
    const request = rpc.request('execSql', {sql: 'SELECT id FROM posts'});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: true,
      result: [
        {
          ...sqlResult(0, [{id: 1}]),
          data: sqlData([{name: 'id', dataTypeID: -1}], [{id: 1}]),
        },
      ],
    });

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('checks only fixed metadata for a trusted bundled Worker result', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker, 'header');
    const request = rpc.request('executeSql', {sql: 'SELECT id FROM posts', params: []});
    const [message] = worker.posted as WorkerRequest[];
    // The rows' text is left for the client to parse once they are used.
    const result = {...sqlResult(1), data: 'not JSON'};

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
    const outcome = {revision: 1, tables: ['posts'], keys: {}};

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
    for (const data of [
      'not JSON',
      sqlData([], [1]),
      sqlData([{name: 'id'}], []),
      JSON.stringify({fields: [], rows: [], extra: true}),
    ]) {
      expect(isRpcResult('executeSql', {...sqlResult(1), data})).toBe(false);
    }
    expect(
      isRpcResult('executeSql', {
        ...sqlResult(1),
        data: sqlData([{name: 'id', dataTypeID: 20}], [[1], {id: 2}]),
      }),
    ).toBe(true);
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
      isRpcResultHeader('executeSql', {...sqlResult(1), data: 'not JSON'}),
    ).toBe(true);
    expect(isRpcResultHeader('executeSql', {...sqlResult(1), data: 5})).toBe(
      false,
    );
    expect(
      isRpcResultHeader('executeSql', {...sqlResult(1), rows: []}),
    ).toBe(false);
  });

  it('validates a schema result strictly', () => {
    const column = {name: 'id', type: 'integer', nullable: false};
    const table = {
      name: 'notes',
      columns: [column, {name: 'body', type: 'json', nullable: true, default: {a: [1]}}],
      primaryKey: ['id'],
      indexes: [{name: 'notes_body', columns: ['body'], unique: true}],
    };

    expect(isRpcResult('schema', {tables: []})).toBe(true);
    expect(isRpcResult('schema', {tables: [table]})).toBe(true);
    expect(
      isRpcResult('schema', {
        tables: [{...table, columns: [{...column, default: null}]}],
      }),
    ).toBe(true);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'schema',
        params: undefined,
      }),
    ).toBe(true);
    expect(
      isWorkerRequest({v: PROTOCOL_VERSION, id: 1, method: 'schema', params: {}}),
    ).toBe(false);

    for (const invalid of [
      undefined,
      {},
      {tables: [], extra: true},
      {tables: [{...table, extra: true}]},
      {tables: [{...table, primaryKey: 'id'}]},
      {tables: [{...table, columns: [{...column, type: 'bigint'}]}]},
      {tables: [{...table, columns: [{...column, nullable: 0}]}]},
      {tables: [{...table, columns: [{...column, default: undefined}]}]},
      {tables: [{...table, columns: [{...column, extra: true}]}]},
      {tables: [{...table, indexes: [{name: 'i', columns: ['id']}]}]},
      {tables: [{...table, columns: new Array(1)}]},
    ]) {
      expect(isRpcResult('schema', invalid)).toBe(false);
    }
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

  it('validates shared JSON graphs without expanding every reference path', () => {
    let shared: JsonValue = 1;
    for (let depth = 0; depth < 60; depth += 1) {
      shared = depth % 2 === 0 ? [shared, shared] : {left: shared, right: shared};
    }
    // Structured clone retains these sixty containers, rather than creating
    // the exponentially larger JSON tree they represent.
    const value = structuredClone(shared);
    expect(isWorkerRequest(queryRequest([value]))).toBe(true);
    expect(
      isWorkerRequest({
        ...queryRequest([]),
        method: 'executePrepared',
        params: {statementId: 1, params: [value]},
      }),
    ).toBe(true);
    expect(
      isSerializedError({code: 'CUSTOM_ERROR', message: 'details', details: value}),
    ).toBe(true);
  });

  it('keeps depth and cycle checks when cached JSON takes a longer path', () => {
    const shared: JsonValue = {leaf: 1};
    const nested = (depth: number): JsonValue => {
      let value: JsonValue = shared;
      for (let index = 0; index < depth; index += 1) {
        value = [value];
      }
      return value;
    };

    for (const [depth, valid] of [[63, true], [64, false]] as const) {
      const values = [shared, nested(depth)];
      expect(isWorkerRequest(queryRequest(values))).toBe(valid);
      expect(isRpcResult('execSql', values.map(jsonResult))).toBe(valid);
    }

    const cyclic: JsonValue[] = [];
    cyclic.push(cyclic);
    expect(isWorkerRequest(queryRequest([shared, cyclic]))).toBe(false);
    expect(
      isSerializedError({code: 'CUSTOM_ERROR', message: 'cycle', details: cyclic}),
    ).toBe(false);
    const sparse = new Array<JsonValue>(1);
    expect(isWorkerRequest(queryRequest([sparse]))).toBe(false);
  });

  it('checks scalar parameters as strictly as nested ones', () => {
    const cases: [unknown[], boolean][] = [
      [[null, true, false, 0, -0, 1.5, -2, 'text', ''], true],
      [[1, NaN], false],
      [[Infinity], false],
      [[1, undefined], false],
      [new Array(2), false],
      [[1, 'x', {nested: [1, 'y']}], true],
      [[{nested: 1}, NaN], false],
      [[1, {nested: NaN}], false],
      [[1, () => 1], false],
      [[1, Symbol('s')], false],
    ];
    for (const [params, valid] of cases) {
      expect(isWorkerRequest(queryRequest(params as JsonValue[]))).toBe(valid);
      expect(
        isWorkerRequest({
          ...queryRequest([]),
          method: 'executePrepared',
          params: {statementId: 1, params},
        }),
      ).toBe(valid);
    }
  });

  it('shares one JSON work budget across parameters, rows, and script results', () => {
    const first = new Array<JsonValue>(600_000).fill(null);
    const second = new Array<JsonValue>(600_000).fill(null);
    expect(isWorkerRequest(queryRequest([first]))).toBe(true);
    expect(isRpcResult('executeSql', jsonResult(first))).toBe(true);
    expect(isWorkerRequest(queryRequest([first, second]))).toBe(false);
    expect(
      isRpcResult('executeSql', {
        ...sqlResult(0),
        data: sqlData([], [{payload: first}, {payload: second}]),
      }),
    ).toBe(false);
    expect(
      isRpcResult('execSql', [jsonResult(first), jsonResult(second)]),
    ).toBe(false);
    expect(
      isSerializedError({
        code: 'CUSTOM_ERROR',
        message: 'large',
        details: [first, second],
      }),
    ).toBe(false);
    // Independent messages receive independent budgets.
    expect(isWorkerRequest(queryRequest([second]))).toBe(true);
  });

  it('also bounds repeated non-JSON result metadata within a script response', () => {
    const result = {
      ...sqlResult(0),
      data: sqlData(new Array(600_000).fill({name: 'id', dataTypeID: 20}), []),
    };
    expect(isRpcResult('executeSql', result)).toBe(true);
    expect(isRpcResult('execSql', [result, result])).toBe(false);
  });

  it('rejects every pending request after a protocol mismatch', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker);
    const request = rpc.request('executeSql', {sql: 'select', params: []});

    worker.emitInvalidMessage({v: 999, id: 1, ok: true, result: []});

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('rejects pending work when the worker crashes', async () => {
    const worker = new FakeWorker();
    const rpc = createWorkerRpc(worker);
    const request = rpc.request('executeSql', {sql: 'select', params: []});

    worker.emitError('boom');

    await expect(request).rejects.toMatchObject({
      code: 'WORKER_ERROR',
      message: 'boom',
    });
  });
});

function sqlData(fields: unknown[], rows: unknown[]): string {
  return JSON.stringify({fields, rows});
}

function sqlResult(revision: number, rows: Array<{id: number}> = []) {
  return {
    command: 'SELECT',
    revision,
    rowCount: rows.length,
    tables: [],
    keys: {},
    data: sqlData(rows.length > 0 ? [{name: 'id', dataTypeID: 20}] : [], rows),
  };
}

function queryRequest(params: JsonValue[]): WorkerRequest {
  return {
    v: PROTOCOL_VERSION,
    id: 1,
    method: 'executeSql',
    params: {sql: 'SELECT id FROM posts WHERE id = $1', params},
  };
}

function jsonResult(value: JsonValue) {
  return {
    ...sqlResult(0),
    rowCount: 1,
    data: sqlData([{name: 'payload', dataTypeID: 114}], [{payload: value}]),
  };
}
