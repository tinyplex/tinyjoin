import {describe, expect, it, vi} from 'vitest';

import {
  createWorkerRpc,
  type PageRpc,
  type StatementSettled,
} from '../../src/client/rpc.ts';
import {isCount, isCountWithin} from '../../src/common.ts';
import {
  PROTOCOL_VERSION,
  STATEMENT_PREPARED,
  STATEMENT_SQL,
  isRpcResult,
  isRpcResultHeader,
  isSerializedError,
  isStatementRequest,
  isStatementResponse,
  isWorkerRequest,
  type JsonValue,
  type SqlResult,
  type StatementRequest,
  type StatementResponse,
  type WorkerRequest,
} from '../../src/protocol.ts';
import {reactions} from './reactions.ts';
import {StatementWorker, flatResponse} from './statement-forms.ts';
import {unhandledRejections} from './unhandled.ts';

const INVALID_MESSAGE = 'The TinyJoin worker sent an invalid protocol message';
const INVALID_RESULT =
  'The TinyJoin worker returned an invalid result for the requested operation';

// Sends a statement whose response or result is passed through unread.
const execute = (
  rpc: PageRpc,
  operation: StatementRequest[2],
  target: string | number,
  {
    transaction = 0,
    arrayRows = false,
    params = [],
    settled,
    context,
  }: {
    transaction?: StatementRequest[4];
    arrayRows?: boolean;
    params?: JsonValue[];
    settled?: StatementSettled<unknown>;
    context?: unknown;
  } = {},
): Promise<StatementResponse | SqlResult> =>
  rpc.execute<StatementResponse | SqlResult, unknown>(
    operation,
    target,
    transaction,
    arrayRows,
    params,
    (response) => response,
    (result) => result,
    settled,
    context,
  );

describe('WorkerRpc', () => {
  it('matches out-of-order responses to their requests', async () => {
    const worker = new StatementWorker();
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
    const worker = new StatementWorker();
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
    const worker = new StatementWorker();
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

  it('sends a statement as one flat array', () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const nested = {tags: ['a', {b: null}]};

    void execute(rpc, STATEMENT_SQL, 'SELECT 1');
    void execute(rpc, STATEMENT_SQL, 'SELECT $1, $2', {
      transaction: 'tx-1',
      params: [7, 'seven'],
    });
    void execute(rpc, STATEMENT_PREPARED, 3, {arrayRows: true});
    void execute(rpc, STATEMENT_PREPARED, 4, {
      transaction: 'epoch/tx-2',
      arrayRows: true,
      params: [null, true, 1.5, '', nested],
    });

    // The version, the request's id, the operation, the text or the prepared
    // statement's id, the transaction or 0, array rows or not, and then each
    // parameter, as it was given.
    expect(worker.posted).toEqual([
      [PROTOCOL_VERSION, 1, STATEMENT_SQL, 'SELECT 1', 0, 0],
      [PROTOCOL_VERSION, 2, STATEMENT_SQL, 'SELECT $1, $2', 'tx-1', 0, 7, 'seven'],
      [PROTOCOL_VERSION, 3, STATEMENT_PREPARED, 3, 0, 1],
      [
        PROTOCOL_VERSION,
        4,
        STATEMENT_PREPARED,
        4,
        'epoch/tx-2',
        1,
        null,
        true,
        1.5,
        '',
        nested,
      ],
    ]);
    expect((worker.posted[3] as unknown[])[10]).toBe(nested);
    // A request sent the other way takes the next id, as one sequence.
    void rpc.request('execSql', {sql: 'SELECT 1'});
    expect(worker.posted[4]).toMatchObject({id: 5, method: 'execSql'});
  });

  it('lays out as many parameters as a statement can take, in order, and a missing one as undefined', () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const head = (id: number): unknown[] => [
      PROTOCOL_VERSION,
      id,
      STATEMENT_PREPARED,
      1,
      0,
      0,
    ];
    // None, a few, and as many as a statement can refer to, `$1` to `$1024`.
    const counts = [0, 1, 5, 1023, 1024];

    counts.forEach((count, index) => {
      const params = Array.from({length: count}, (_, at) => (at % 3 ? `p${at}` : at));
      void execute(rpc, STATEMENT_PREPARED, 1, {params});
      const posted = worker.posted[index] as unknown[];
      expect(posted).toEqual([...head(index + 1), ...params]);
      expect(isStatementRequest(posted)).toBe(true);
      // No slot of the array is missing: a structured clone writes an array
      // with holes as the sparse object it then is, slot by slot.
      expect(Object.keys(posted)).toHaveLength(6 + count);
    });

    // A hole in the caller's array is a parameter that is not JSON, which the
    // Worker refuses as it refuses `undefined`.
    for (const length of [3, 1024]) {
      const sparse: JsonValue[] = new Array(length);
      sparse[0] = 'first';
      sparse[length - 1] = 'last';
      void execute(rpc, STATEMENT_PREPARED, 1, {params: sparse});
      const posted = worker.posted.at(-1) as unknown[];
      expect(posted).toHaveLength(6 + length);
      expect(posted[6]).toBe('first');
      expect(Object.hasOwn(posted, 7)).toBe(true);
      expect(posted[7]).toBeUndefined();
      expect(posted.at(-1)).toBe('last');
      expect(isStatementRequest(posted)).toBe(false);
    }
  });

  it('lays out an array by its indexes, whatever else the array is or holds', () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    // A flat request holds the parameters themselves, read by index, and not
    // the array they came in. So what only a clone of the array itself would
    // meet is not met: a Proxy around it, as a reactive array has, which no
    // clone can copy, and a property beside its indexes, which a clone takes
    // with it. In a request object, as every statement once went, either one
    // made the post fail.
    const reactive = new Proxy<JsonValue[]>([1, 'two'], {});
    const annotated = Object.assign<JsonValue[], object>([3], {
      note: () => 'not data',
    });
    expect(() => structuredClone({params: reactive})).toThrow();
    expect(() => structuredClone({params: annotated})).toThrow();

    void execute(rpc, STATEMENT_SQL, 'SELECT $1, $2', {params: reactive});
    void execute(rpc, STATEMENT_SQL, 'SELECT $1', {params: annotated});

    expect(worker.posted).toStrictEqual([
      [PROTOCOL_VERSION, 1, STATEMENT_SQL, 'SELECT $1, $2', 0, 0, 1, 'two'],
      [PROTOCOL_VERSION, 2, STATEMENT_SQL, 'SELECT $1', 0, 0, 3],
    ]);
    for (const posted of worker.posted) {
      expect(isStatementRequest(structuredClone(posted))).toBe(true);
    }
  });

  it('sends parameters that no statement can take in its request object, as they were given', () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    // Laid out flat, a string would become a parameter for each character, and
    // an object that only looks like an array would pass for one. In a request
    // object the Worker sees each for what it is, and refuses the request.
    const notArrays = ['ab', {length: 1, 0: 'a'}, null, 5, new Uint8Array(2)];
    // One parameter more than any statement can refer to. The Worker takes the
    // request, and its engine then refuses the statement for their number.
    const tooMany = Array.from({length: 1025}, (_, index) => index);

    for (const params of [...notArrays, tooMany]) {
      void execute(rpc, STATEMENT_SQL, 'SELECT $1', {
        params: params as never,
      }).catch(() => undefined);
    }
    void execute(rpc, STATEMENT_PREPARED, 9, {
      transaction: 'tx-1',
      arrayRows: true,
      params: 'ab' as never,
    }).catch(() => undefined);
    void execute(rpc, STATEMENT_PREPARED, 9, {transaction: 'tx-1', params: tooMany});
    void execute(rpc, STATEMENT_SQL, 'SELECT $1', {arrayRows: true, params: tooMany});

    // Each is the request its statement stands for, with exactly the keys such
    // a request has: the transaction only inside one, and the row mode only
    // for array rows. The Worker then answers each as it always has.
    expect(worker.posted).toStrictEqual([
      ...[...notArrays, tooMany].map((params, index) => ({
        v: PROTOCOL_VERSION,
        id: index + 1,
        method: 'executeSql',
        params: {sql: 'SELECT $1', params},
      })),
      {
        v: PROTOCOL_VERSION,
        id: 7,
        method: 'executePrepared',
        params: {
          statementId: 9,
          params: 'ab',
          transactionId: 'tx-1',
          rowMode: 'array',
        },
      },
      {
        v: PROTOCOL_VERSION,
        id: 8,
        method: 'executePrepared',
        params: {statementId: 9, params: tooMany, transactionId: 'tx-1'},
      },
      {
        v: PROTOCOL_VERSION,
        id: 9,
        method: 'executeSql',
        params: {sql: 'SELECT $1', params: tooMany, rowMode: 'array'},
      },
    ]);
    const requests = worker.posted as {params: {params: unknown}}[];
    expect(Object.keys(requests[6]!.params)).toEqual([
      'statementId',
      'params',
      'transactionId',
      'rowMode',
    ]);
    const given = [...notArrays, tooMany, 'ab', tooMany, tooMany];
    requests.forEach((posted, index) => {
      // The parameters are the caller's own, for a structured clone to copy.
      expect(posted.params.params).toBe(given[index]);
      expect(isWorkerRequest(posted)).toBe(given[index] === tooMany);
    });
  });

  it('reads no more of an array too long for any statement than its length', () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    // A caller's mistake, such as params[id] = value, makes an array that is
    // nearly all holes. A structured clone writes only what such an array
    // holds, and the Worker refuses it for its length. Laid out flat, it would
    // be copied slot by slot on the page's thread first: every one of four
    // million here, and of four thousand million at the longest.
    const sparse: JsonValue[] = [];
    sparse[4_000_000] = 'x';
    let slotsRead = 0;
    const watched = new Proxy(sparse, {
      get: (target, key, receiver) => {
        slotsRead += Number(key !== 'length');
        return Reflect.get(target, key, receiver) as unknown;
      },
    });

    void execute(rpc, STATEMENT_PREPARED, 1, {transaction: 'tx-1', params: watched});
    void execute(rpc, STATEMENT_SQL, 'SELECT $1', {params: sparse});

    expect(slotsRead).toBe(0);
    const [first, second] = worker.posted as {params: {params: unknown}}[];
    expect(first!.params.params).toBe(watched);
    expect(second!.params.params).toBe(sparse);
    expect(isWorkerRequest(second)).toBe(false);
  });

  it('reads a flat response with one reader and a result object with the other', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const send = (): Promise<string> =>
      rpc.execute(
        STATEMENT_PREPARED,
        1,
        0,
        false,
        [],
        (response) => `flat ${response[3]}`,
        (result) => `object ${result.revision}`,
      );
    const flat = send();
    const object = send();

    worker.respond([PROTOCOL_VERSION, 1, 1, 8, 1, 'posts']);
    worker.respond({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: sqlResult(9, [{id: 1}]),
    });

    await expect(flat).resolves.toBe('flat 8');
    await expect(object).resolves.toBe('object 9');
  });

  it('gives a statement sent as a request object the flat response it may get', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const raw = rpc.request('executeSql', {sql: 'SELECT 1', params: []});
    const read = rpc.request(
      'executePrepared',
      {statementId: 1, params: []},
      (response) => (Array.isArray(response) ? response[4] : -1),
    );
    const response = flatResponse(1, sqlResult(4, [{id: 7}]))!;

    worker.respond(response);
    worker.respond([PROTOCOL_VERSION, 2, 2, 4, 3, 'posts']);

    await expect(raw).resolves.toBe(response);
    await expect(read).resolves.toBe(3);
  });

  it('fails alone a read whose rows it cannot read', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const read = execute(rpc, STATEMENT_PREPARED, 1);
    const unreadable = execute(rpc, STATEMENT_SQL, 'SELECT 1');
    const invalid = execute(rpc, STATEMENT_SQL, 'SELECT 2');
    const after = execute(rpc, STATEMENT_SQL, 'SELECT 3');
    const readable = flatResponse(1, sqlResult(4, [{id: 7}]))!;

    worker.respond(readable);
    worker.respond([PROTOCOL_VERSION, 2, 3, 4, 0, '{"fields":']);
    worker.respond([PROTOCOL_VERSION, 3, 3, 4, 1, sqlData([], [1])]);
    worker.respond({
      v: PROTOCOL_VERSION,
      id: 4,
      ok: true,
      result: sqlResult(5),
    });

    await expect(read).resolves.toBe(readable);
    // The rows are text the engine wrote, which the Worker passed on unread.
    // The statement fails as the Worker would have failed it, and the Worker
    // and its database are unharmed.
    for (const failed of [unreadable, invalid]) {
      await expect(failed).rejects.toMatchObject({
        code: 'BRIDGE_SERIALIZATION_ERROR',
        message: 'WASM returned an invalid structured SQL result',
      });
    }
    await expect(after).resolves.toEqual(sqlResult(5));
    expect(worker.terminated).toBe(false);
  });

  it('leaves the rows of a trusted Worker\'s flat response unread', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker, 'header');
    const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');
    const response: StatementResponse = [PROTOCOL_VERSION, 1, 3, 4, 0, 'not JSON'];

    worker.respond(response);

    // Whoever uses the rows parses them, and finds out then.
    await expect(request).resolves.toBe(response);
    expect(worker.terminated).toBe(false);
  });

  it.each(['full', 'header'] as const)(
    'rejects a flat response to anything but a statement, checked in %s',
    async (validation) => {
      const worker = new StatementWorker();
      const rpc = createWorkerRpc(worker, validation);
      const request = rpc.request('execSql', {sql: 'SELECT 1'});

      // The Worker TinyJoin ships is held to this as any other is: the array
      // is sound in itself, and answers a request that no array answers.
      worker.respond([PROTOCOL_VERSION, 1, 3, 0, 0, sqlData([], [])]);

      await expect(request).rejects.toMatchObject({
        code: 'PROTOCOL_MISMATCH',
        message: INVALID_RESULT,
      });
      expect(worker.terminated).toBe(true);
    },
  );

  it('rejects a statement result sent as the text of an earlier protocol', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const {data, ...header} = sqlResult(4, [{id: 7}]);
    const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');

    worker.respond({
      v: PROTOCOL_VERSION,
      id: 1,
      ok: true,
      result: [JSON.stringify(header), data],
    });

    await expect(request).rejects.toMatchObject({
      code: 'PROTOCOL_MISMATCH',
      message: INVALID_RESULT,
    });
    expect(worker.terminated).toBe(true);
  });

  it('passes over a flat response to no request in flight', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');

    // A response object to a request no longer in flight is passed over too.
    worker.respond([PROTOCOL_VERSION, 99, 1, 0, 1, 'posts']);
    worker.respond({v: PROTOCOL_VERSION, id: 98, ok: true, result: 'anything'});
    expect(worker.terminated).toBe(false);

    // As is one whose result nothing could read: its request is looked for
    // before its result is judged, as a response object's is.
    worker.emitInvalidMessage([PROTOCOL_VERSION, 2, 1, -1, 0]);
    worker.emitInvalidMessage([PROTOCOL_VERSION, 97]);
    expect(worker.terminated).toBe(false);

    worker.respond([PROTOCOL_VERSION, 1, 1, 2, 1, 'posts']);
    await expect(request).resolves.toEqual([PROTOCOL_VERSION, 1, 1, 2, 1, 'posts']);
  });

  it('holds the array of a Worker an application supplied to its slots and nothing else', async () => {
    // A hole and a named property together leave the count of keys right.
    const named = Object.assign([PROTOCOL_VERSION, 1, 1, 0, 1], {extra: 1});
    const holed = Object.assign(
      withHole([PROTOCOL_VERSION, 1, 1, 0, 1, 'posts'], 5),
      {extra: 1},
    );
    for (const array of [named, holed]) {
      const worker = new StatementWorker();
      const rpc = createWorkerRpc(worker, 'full');
      const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');
      worker.emitInvalidMessage(array);
      await expect(request).rejects.toMatchObject({
        code: 'PROTOCOL_MISMATCH',
        message: INVALID_MESSAGE,
      });
      expect(worker.terminated).toBe(true);
    }
    // The Worker TinyJoin ships is the other half of this package, and its
    // array is not walked for what it should not hold.
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker, 'header');
    const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');
    worker.emitInvalidMessage(named);
    await expect(request).resolves.toBe(named);
    expect(worker.terminated).toBe(false);
  });

  it('reads a message once', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');
    let reads = 0;
    const response = [PROTOCOL_VERSION, 1, 1, 2, 1, 'posts'];
    worker.emitEvent({
      get data() {
        reads++;
        return response;
      },
    });
    await expect(request).resolves.toBe(response);
    expect(reads).toBe(1);
  });

  it.each(['full', 'header'] as const)(
    'disposes the connection for an array that is no statement response, checked in %s',
    async (validation) => {
      // Each array is sent while request 1, a statement, is in flight. One
      // that names it has an invalid result. One that names nothing in flight
      // is an invalid message.
      const cases: [unknown[], string][] = [
        [[PROTOCOL_VERSION, 1], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 4, 0, 0], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, '1', 0, 0], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 1, -1, 0], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 1, 0, 1.5], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 1, 0, 1, 5], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 0], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 2, 'a', 'b', 1], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 3, 0, 1], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 3, 0, 1, 5], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 3, 0, 1, '{}', 'extra'], INVALID_RESULT],
        [[], INVALID_MESSAGE],
        [[PROTOCOL_VERSION - 1, 1, 1, 0, 1], INVALID_MESSAGE],
        [[PROTOCOL_VERSION, 0, 1, 0, 1], INVALID_MESSAGE],
        [[PROTOCOL_VERSION, '1', 1, 0, 1], INVALID_MESSAGE],
        [[PROTOCOL_VERSION, 1.5, 1, 0, 1], INVALID_MESSAGE],
      ];
      // What only the full check of an application's own Worker reads. An
      // array with a hole is no message at all, whatever it would have held.
      const deepCases: [unknown[], string][] = [
        [[PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 1, 5, 1], INVALID_RESULT],
        [[PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 1, 'id', NaN], INVALID_RESULT],
        [
          [PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 1, 'id', {nested: 1}],
          INVALID_RESULT,
        ],
        [
          [PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 1, 'id', undefined],
          INVALID_RESULT,
        ],
        [
          withHole([PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 1, 'id'], 9),
          INVALID_MESSAGE,
        ],
        [
          [
            PROTOCOL_VERSION,
            1,
            2,
            0,
            1001,
            'posts',
            1,
            'id',
            ...Array.from({length: 1001}, (_, index) => index),
          ],
          INVALID_RESULT,
        ],
      ];
      for (const [array, message] of cases) {
        const worker = new StatementWorker();
        const rpc = createWorkerRpc(worker, validation);
        const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');
        worker.emitInvalidMessage(array);
        await expect(request, JSON.stringify(array)).rejects.toMatchObject({
          code: 'PROTOCOL_MISMATCH',
          message,
        });
        expect(worker.terminated).toBe(true);
      }
      for (const [array, message] of deepCases) {
        const worker = new StatementWorker();
        const rpc = createWorkerRpc(worker, validation);
        const request = execute(rpc, STATEMENT_SQL, 'SELECT 1');
        worker.emitInvalidMessage(array);
        if (validation === 'full') {
          await expect(request).rejects.toMatchObject({
            code: 'PROTOCOL_MISMATCH',
            message,
          });
        } else {
          await expect(request).resolves.toBe(array);
        }
        expect(worker.terminated).toBe(validation === 'full');
      }
    },
  );

  it('rejects a malformed SQL query success and terminates the worker', async () => {
    const worker = new StatementWorker();
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
    const worker = new StatementWorker();
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
    const worker = new StatementWorker();
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
    // A commit answers with its revision alone: the change event that follows
    // carries the tables and keys.
    expect(isRpcResult('commitTransaction', {revision: 1})).toBe(true);
    expect(isRpcResult('commitTransaction', outcome)).toBe(false);
    expect(isRpcResult('commitTransaction', {revision: -1})).toBe(false);
    expect(isRpcResult('commitTransaction', undefined)).toBe(false);
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
      foreignKeys: [],
    };

    const schema = (tables: unknown[]) => ({version: 1, tables});
    expect(isRpcResult('schema', schema([]))).toBe(true);
    expect(isRpcResult('schema', schema([table]))).toBe(true);
    expect(
      isRpcResult(
        'schema',
        schema([{...table, columns: [{...column, default: null}]}]),
      ),
    ).toBe(true);
    const text = {name: 'code', type: 'text', nullable: true, maxLength: 3};
    expect(isRpcResult('schema', schema([{...table, columns: [text]}]))).toBe(
      true,
    );
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
    // setSchema takes any JSON as its schema, whose shape the engine checks.
    for (const [params, valid] of [
      [{schema: schema([table]), drop: false}, true],
      [{schema: 'anything', drop: true}, true],
      [{schema: schema([]), drop: 'yes'}, false],
      [{schema: schema([])}, false],
    ] as const) {
      expect(
        isWorkerRequest({
          v: PROTOCOL_VERSION,
          id: 1,
          method: 'setSchema',
          params,
        }),
      ).toBe(valid);
    }
    expect(isRpcResult('setSchema', true)).toBe(true);
    expect(isRpcResult('setSchema', 1)).toBe(false);

    for (const invalid of [
      undefined,
      {},
      {tables: []},
      {version: -1, tables: []},
      {version: 1, tables: [], extra: true},
      {version: 1, tables: [{...table, extra: true}]},
      {version: 1, tables: [{...table, primaryKey: 'id'}]},
      {version: 1, tables: [{...table, columns: [{...column, type: 'bigint'}]}]},
      {version: 1, tables: [{...table, columns: [{...column, nullable: 0}]}]},
      {version: 1, tables: [{...table, columns: [{...column, default: undefined}]}]},
      {version: 1, tables: [{...table, columns: [{...column, extra: true}]}]},
      {version: 1, tables: [{...table, columns: [{...column, maxLength: 3}]}]},
      {version: 1, tables: [{...table, columns: [{...text, maxLength: 0}]}]},
      {version: 1, tables: [{...table, columns: [{...text, maxLength: 1.5}]}]},
      {version: 1, tables: [{...table, indexes: [{name: 'i', columns: ['id']}]}]},
      {version: 1, tables: [{...table, columns: new Array(1)}]},
      {
        version: 1,
        tables: [
          {
            ...table,
            foreignKeys: [
              {
                name: 'k',
                columns: ['id'],
                references: 'other',
                referencedColumns: ['id'],
                onDelete: 'delete',
                onUpdate: 'no action',
              },
            ],
          },
        ],
      },
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

  it('checks a statement response as its helpers would', () => {
    // The check spells out its integer tests, which run for every statement.
    // What it replaced, written with the shared helpers, is kept here to
    // compare with on arrays that mix valid slots with every kind of invalid
    // one.
    const oracle = (value: readonly unknown[], deep: boolean): boolean => {
      const length = value.length;
      const command: unknown = value[2];
      if (
        length < 5 ||
        value[0] !== PROTOCOL_VERSION ||
        !(isCount(value[1]) && value[1] >= 1) ||
        !isCount(value[3]) ||
        !isCount(value[4])
      ) {
        return false;
      }
      if (command === 3) {
        return length === 6 && typeof value[5] === 'string';
      }
      if (command !== 0 && command !== 1 && command !== 2) {
        return false;
      }
      if (length === 5) {
        return true;
      }
      if (typeof value[5] !== 'string') {
        return false;
      }
      if (length === 6) {
        return true;
      }
      const width: unknown = value[6];
      if (!isCountWithin(width, 1, 255)) {
        return false;
      }
      const values = length - 7 - width;
      if (values < 0 || values % width !== 0) {
        return false;
      }
      if (!deep) {
        return true;
      }
      if (values / width > 1_000) {
        return false;
      }
      for (let index = 7; index < length; index++) {
        const item: unknown = value[index];
        if (!Object.hasOwn(value, index)) {
          return false;
        }
        if (index < 7 + width) {
          if (typeof item !== 'string') {
            return false;
          }
        } else if (typeof item === 'number') {
          if (!Number.isFinite(item)) {
            return false;
          }
        } else if (
          item !== null &&
          typeof item !== 'boolean' &&
          typeof item !== 'string'
        ) {
          return false;
        }
      }
      return true;
    };

    // What a slot may hold: the values a response has there first, and then
    // those it must not have.
    const safe = Number.MAX_SAFE_INTEGER;
    const integers = [
      ...[0, 1, 2, 7],
      ...[-0, -1, 0.5, 1.5, safe, safe + 1, -safe, 2 ** 31, 2 ** 32, 1e300],
      ...[NaN, Infinity, -Infinity, '1', null, undefined, true, [1], {}, 1n],
    ];
    const versions = [
      ...[PROTOCOL_VERSION, PROTOCOL_VERSION, PROTOCOL_VERSION],
      ...[PROTOCOL_VERSION - 1, String(PROTOCOL_VERSION), undefined],
    ];
    const commands = [0, 1, 2, 3, 0, 1, 2, 3, 4, -1, 1.5, '1', null, undefined];
    const widths = [1, 1, 1, 2, 3, 0, 255, 256, 1.5, -1, '1', null, NaN];
    const tails = [
      ...['id', 'posts', '', 0, 1, -1.5, true, false, null],
      ...[NaN, Infinity, undefined, {}, [], 1n],
    ];
    let seed = 0x2f6e2b1;
    const random = (): number => {
      seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
      return seed / 0x1_0000_0000;
    };
    // Most slots are valid most of the time, so that the later checks run.
    const pick = <Item>(items: readonly Item[], valid: number): Item =>
      items[Math.floor(random() * (random() < 0.8 ? valid : items.length))]!;

    let accepted = 0;
    for (let round = 0; round < 60_000; round++) {
      const value: unknown[] = [
        pick(versions, 3),
        pick(integers, 4),
        pick(commands, 8),
        pick(integers, 4),
        pick(integers, 4),
      ];
      const shape = random();
      if (shape < 0.1) {
        value.length = Math.floor(random() * 5);
      } else if (shape < 0.3) {
        value.push(pick(tails, 2));
      } else if (shape < 0.9) {
        // A table, a key width, that many columns, and a few keys' values:
        // then sometimes a value too many, or a slot that was never set.
        const width = pick(widths, 5);
        value.push(pick(tails, 2), width);
        const columns =
          typeof width === 'number' && width >= 0 && width < 300
            ? Math.floor(width)
            : 1;
        const keys = Math.floor(random() * 4);
        for (let index = 0; index < columns; index++) {
          value.push(pick(tails, 3));
        }
        for (let index = 0; index < columns * keys; index++) {
          value.push(pick(tails, 9));
        }
        if (random() < 0.15) {
          value.push(pick(tails, 9));
        }
        if (random() < 0.05) {
          value.length += 1;
        }
      }
      for (const deep of [false, true]) {
        const expected = oracle(value, deep);
        if (isStatementResponse(value, deep) !== expected) {
          expect.fail(
            `The ${deep ? 'full' : 'fixed'} check of [${String(value)}] should be ${expected}`,
          );
        }
        accepted += Number(expected);
      }
    }
    // The comparison means something only if both answers were common.
    expect(accepted).toBeGreaterThan(20_000);
    expect(accepted).toBeLessThan(100_000);

    // The key bound of the full check, which too few random arrays reach.
    const keys = (count: number): unknown[] => [
      PROTOCOL_VERSION,
      1,
      2,
      0,
      count,
      'posts',
      1,
      'id',
      ...Array.from({length: count}, (_, index) => index),
    ];
    for (const [count, deep] of [
      [1000, true],
      [1001, false],
    ] as const) {
      expect(isStatementResponse(keys(count), false)).toBe(true);
      expect(isStatementResponse(keys(count), true)).toBe(deep);
      expect(oracle(keys(count), true)).toBe(deep);
    }
  });

  it('tells a tracked statement\'s caller that it settled, before its promise does', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const told: unknown[][] = [];
    const order: string[] = [];
    const settled: StatementSettled<unknown> = (failed, error, context) => {
      told.push([failed, (error as {code?: string} | undefined)?.code, context]);
      // Whatever the caller makes ready here runs before the promise's readers.
      queueMicrotask(() => order.push(`${String(context)} told`));
    };
    const track = (context: string): Promise<unknown> =>
      execute(rpc, STATEMENT_PREPARED, 1, {settled, context}).then(
        () => order.push(`${context} resolved`),
        () => order.push(`${context} rejected`),
      );
    const flat = track('flat');
    const object = track('object');
    const refused = track('refused');
    const unread = rpc
      .execute(
        STATEMENT_PREPARED,
        1,
        0,
        false,
        [],
        () => {
          throw Object.assign(new Error('unreadable'), {code: 'UNREAD'});
        },
        (result) => result,
        settled,
        'unread',
      )
      .catch(() => order.push('unread rejected'));
    const unreadable = track('unreadable');

    worker.respond([PROTOCOL_VERSION, 1, 1, 0, 1, 'posts']);
    worker.respond({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: sqlResult(3, [{id: 1}]),
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: 3,
      ok: false,
      error: {code: 'BIND_ERROR', message: 'bad parameters'},
    });
    worker.respond([PROTOCOL_VERSION, 4, 1, 0, 1]);
    // A read whose rows the full check cannot read fails alone, and its
    // caller is told of that as of any other failure.
    worker.respond([PROTOCOL_VERSION, 5, 3, 0, 1, '{"fields":']);

    // Each was told as its response arrived, and no promise had settled yet.
    expect(told).toEqual([
      [false, undefined, 'flat'],
      [false, undefined, 'object'],
      [true, 'BIND_ERROR', 'refused'],
      [true, 'UNREAD', 'unread'],
      [true, 'BRIDGE_SERIALIZATION_ERROR', 'unreadable'],
    ]);
    expect(order).toEqual([]);
    await Promise.all([flat, object, refused, unread, unreadable]);
    expect(order).toEqual([
      'flat told',
      'flat resolved',
      'object told',
      'object resolved',
      'refused told',
      'refused rejected',
      'unread told',
      'unread rejected',
      'unreadable told',
      'unreadable rejected',
    ]);
    expect(told).toHaveLength(5);
    expect(worker.terminated).toBe(false);
  });

  it('tells a tracked statement\'s caller once, whatever stops the statement', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const told: unknown[][] = [];
    const settled: StatementSettled<unknown> = (failed, error, context) => {
      told.push([failed, (error as {code: string}).code, context]);
    };

    // A statement that cannot be posted, with parameters laid out flat or not.
    const notData = (): number => 1;
    worker.onPost = (message) => {
      if (JSON.stringify(message).includes('never posted')) {
        throw new DOMException('could not be cloned', 'DataCloneError');
      }
    };
    await expect(
      execute(rpc, STATEMENT_SQL, 'never posted', {settled, context: 'flat post'}),
    ).rejects.toMatchObject({
      code: 'WORKER_POST_FAILED',
      message: 'could not be cloned',
    });
    await expect(
      execute(rpc, STATEMENT_SQL, 'never posted', {
        params: notData as never,
        settled,
        context: 'object post',
      }),
    ).rejects.toMatchObject({code: 'WORKER_POST_FAILED'});
    expect(told).toEqual([
      [true, 'WORKER_POST_FAILED', 'flat post'],
      [true, 'WORKER_POST_FAILED', 'object post'],
    ]);

    // Statements in flight when the connection is disposed.
    const first = execute(rpc, STATEMENT_SQL, 'SELECT 1', {settled, context: 'first'});
    const second = execute(rpc, STATEMENT_PREPARED, 1, {settled, context: 'second'});
    const untracked = execute(rpc, STATEMENT_SQL, 'SELECT 2');
    worker.emitError('boom');
    for (const request of [first, second, untracked]) {
      await expect(request).rejects.toMatchObject({code: 'WORKER_ERROR'});
    }
    // And a statement sent after it.
    await expect(
      execute(rpc, STATEMENT_SQL, 'SELECT 3', {settled, context: 'late'}),
    ).rejects.toMatchObject({code: 'WORKER_TERMINATED'});
    await expect(execute(rpc, STATEMENT_SQL, 'SELECT 4')).rejects.toMatchObject({
      code: 'WORKER_TERMINATED',
    });

    expect(told.slice(2)).toEqual([
      [true, 'WORKER_ERROR', 'first'],
      [true, 'WORKER_ERROR', 'second'],
      [true, 'WORKER_TERMINATED', 'late'],
    ]);
    // Nothing was posted once the connection had gone.
    expect(worker.posted).toHaveLength(5);
  });

  it('marks as handled the rejection of a statement its caller tracks', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const refuse = (id: number): void =>
      worker.respond({
        v: PROTOCOL_VERSION,
        id,
        ok: false,
        error: {code: 'BIND_ERROR', message: 'bad parameters'},
      });

    // A tracked statement that nobody awaits: its caller was told, and answers
    // for it. The same goes for one whose rows cannot be read, and for one
    // whose connection is disposed, or already was.
    expect(
      await unhandledRejections(() => {
        void execute(rpc, STATEMENT_SQL, 'SELECT 1', {settled: () => undefined});
        refuse(1);
        void execute(rpc, STATEMENT_SQL, 'SELECT 2', {settled: () => undefined});
        worker.respond([PROTOCOL_VERSION, 2, 3, 0, 1, '{"fields":']);
        void execute(rpc, STATEMENT_SQL, 'SELECT 3', {settled: () => undefined});
        rpc.dispose();
        void execute(rpc, STATEMENT_SQL, 'SELECT 4', {settled: () => undefined});
      }),
    ).toEqual([]);

    // A statement nobody tracks is its caller's own to handle, as any promise
    // is.
    const other = createWorkerRpc(worker);
    expect(
      await unhandledRejections(() => {
        void execute(other, STATEMENT_SQL, 'SELECT 5');
        refuse(1);
      }),
    ).toMatchObject([{code: 'BIND_ERROR'}]);
  });

  it('forgets a statement once it has settled', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const told: unknown[] = [];
    const settled: StatementSettled<unknown> = (_failed, _error, context) => {
      told.push(context);
    };
    const flat = execute(rpc, STATEMENT_SQL, 'UPDATE 1', {settled, context: 'flat'});
    const object = execute(rpc, STATEMENT_SQL, 'UPDATE 2', {settled, context: 'object'});
    const refused = execute(rpc, STATEMENT_SQL, 'UPDATE 3', {settled, context: 'refused'});
    const answer = (): void => {
      worker.respond([PROTOCOL_VERSION, 1, 1, 0, 1, 'posts']);
      worker.respond({v: PROTOCOL_VERSION, id: 2, ok: true, result: sqlResult(3)});
      worker.respond({
        v: PROTOCOL_VERSION,
        id: 3,
        ok: false,
        error: {code: 'BIND_ERROR', message: 'bad parameters'},
      });
    };

    answer();
    await Promise.allSettled([flat, object, refused]);
    expect(told).toEqual(['flat', 'object', 'refused']);
    // Each answer again, and then the connection's end: none of them reaches a
    // caller that has been told already, whose count of what is in flight
    // would otherwise go below what is.
    answer();
    rpc.dispose();
    expect(told).toEqual(['flat', 'object', 'refused']);
  });

  it('tells a tracked statement\'s caller once when its post answers and then throws', async () => {
    // Only a Worker's stand-in on the page can answer a statement before its
    // postMessage has returned, and only a broken one then throws. The
    // statement has settled with its answer by then, and its caller has been
    // told, so the failed post has nothing left to fail.
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const told: unknown[][] = [];
    const settled: StatementSettled<unknown> = (failed, error, context) => {
      told.push([failed, (error as {code?: string} | undefined)?.code, context]);
    };
    const thrown = new Error('thrown after it answered');

    worker.onPost = (message) => {
      worker.respond([PROTOCOL_VERSION, message.id, 1, 0, 1, 'posts']);
      throw thrown;
    };
    await expect(
      execute(rpc, STATEMENT_SQL, 'UPDATE 1', {settled, context: 'answered'}),
    ).resolves.toEqual([PROTOCOL_VERSION, 1, 1, 0, 1, 'posts']);

    worker.onPost = (message) => {
      worker.respond({
        v: PROTOCOL_VERSION,
        id: message.id,
        ok: false,
        error: {code: 'BIND_ERROR', message: 'bad parameters'},
      });
      throw thrown;
    };
    await expect(
      execute(rpc, STATEMENT_SQL, 'UPDATE 2', {settled, context: 'refused'}),
    ).rejects.toMatchObject({code: 'BIND_ERROR'});

    // The same goes for a post that lost the connection before it threw.
    worker.onPost = () => {
      worker.emitError('crashed as it was posted');
      throw thrown;
    };
    await expect(
      execute(rpc, STATEMENT_SQL, 'UPDATE 3', {settled, context: 'lost'}),
    ).rejects.toMatchObject({code: 'WORKER_ERROR'});

    expect(told).toEqual([
      [false, undefined, 'answered'],
      [true, 'BIND_ERROR', 'refused'],
      [true, 'WORKER_ERROR', 'lost'],
    ]);
  });

  it('fails a statement with what describing its failed post threw', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const told: unknown[][] = [];
    // What a stand-in throws need not be an error, nor even a value that a
    // message can be made of: an object with no prototype has no way to
    // become a string. The statement still fails as a promise does, with its
    // caller told, rather than throwing from the call that sent it.
    worker.onPost = () => {
      throw Object.create(null);
    };

    const statement = execute(rpc, STATEMENT_SQL, 'SELECT 1', {
      settled: (failed, error, context) => told.push([failed, error, context]),
      context: 'indescribable',
    });
    const failure: unknown = await statement.catch((error: unknown) => error);
    expect(failure).toBeInstanceOf(TypeError);
    expect(told).toHaveLength(1);
    expect(told[0]![0]).toBe(true);
    expect(told[0]![1]).toBe(failure);
    expect(told[0]![2]).toBe('indescribable');
    // Any other request has always failed this way, its executor having
    // thrown.
    await expect(rpc.request('execSql', {sql: 'SELECT 1'})).rejects.toBeInstanceOf(
      TypeError,
    );

    // What can be described is.
    worker.onPost = () => {
      throw 'a plain string';
    };
    await expect(execute(rpc, STATEMENT_SQL, 'SELECT 2')).rejects.toMatchObject({
      code: 'WORKER_POST_FAILED',
      message: 'a plain string',
    });
    expect(worker.terminated).toBe(false);
  });

  it('attaches a reaction to a tracked statement only when it fails', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker, 'header');
    const told: boolean[] = [];
    const sent: Promise<unknown>[] = [];
    const send = (): void => {
      sent.push(
        execute(rpc, STATEMENT_PREPARED, 1, {
          transaction: 'tx-1',
          params: [1],
          settled: (failed) => told.push(failed),
        }),
      );
    };

    // Sent, answered and settled, in either form: the caller was told through
    // its hook, and nothing was attached to the statement's promise. This is
    // what lets a transaction count its statements, where it once followed
    // each one's promise.
    expect(
      reactions(() => {
        send();
        worker.respond([PROTOCOL_VERSION, 1, 1, 0, 1, 'posts', 1, 'id', 1]);
        send();
        worker.respond({
          v: PROTOCOL_VERSION,
          id: 2,
          ok: true,
          result: sqlResult(3, [{id: 1}]),
        });
      }),
    ).toBe(0);
    // A statement that fails is marked as handled, which is one reaction.
    expect(
      reactions(() => {
        send();
        worker.respond({
          v: PROTOCOL_VERSION,
          id: 3,
          ok: false,
          error: {code: 'BIND_ERROR', message: 'bad parameters'},
        });
      }),
    ).toBe(1);
    expect(told).toEqual([false, false, true]);
    await Promise.allSettled(sent);
  });

  it('rejects every pending request after a protocol mismatch', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const request = rpc.request('executeSql', {sql: 'select', params: []});

    worker.emitInvalidMessage({v: 999, id: 1, ok: true, result: []});

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('rejects pending work when the worker crashes', async () => {
    const worker = new StatementWorker();
    const rpc = createWorkerRpc(worker);
    const request = rpc.request('executeSql', {sql: 'select', params: []});

    worker.emitError('boom');

    await expect(request).rejects.toMatchObject({
      code: 'WORKER_ERROR',
      message: 'boom',
    });
  });
});

// An array of the given length whose last slots were never set.
function withHole(values: unknown[], length: number): unknown[] {
  const array = [...values];
  array.length = length;
  return array;
}

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
