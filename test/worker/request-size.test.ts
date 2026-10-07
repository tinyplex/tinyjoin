import {describe, expect, it} from 'vitest';
import {
  PROTOCOL_VERSION,
  STATEMENT_PARAMS,
  STATEMENT_PREPARED,
  STATEMENT_SQL,
  isStatementRequest,
  isWorkerRequest,
  type JsonValue,
  type StatementRequest,
} from '../../src/protocol.js';
import {MAX_QUEUED_BYTES} from '../../src/worker/coordination-protocol.js';
import {statementRequest} from '../../src/worker/host.js';
import {
  checkedRequestBytes,
  requestBytes,
  statementBytes,
} from '../../src/worker/request-size.js';

/**
 * How a statement's request was sized while statements were sent as request
 * objects: by its shape, when its parameters were scalars, and by the walk
 * otherwise. Statements are now sent as arrays, which statementBytes() sizes,
 * and this routine is kept only as the oracle for it: what a statement was
 * charged then is what it must be charged now.
 */
const requestFormBytes = (
  request: {method: string; params: unknown},
  limit: number,
): number => {
  const params = request.params as Record<string, unknown> | undefined;
  const values = params?.params;
  if (
    (request.method === 'executePrepared' || request.method === 'executeSql') &&
    params !== undefined &&
    Array.isArray(values) &&
    values.length <= 4096
  ) {
    // The request object with its keys `v`, `id`, `method` and `params`, the
    // numbers and the method's name; then the parameters object with its keys,
    // each scalar value, and the array of parameters.
    let bytes = 222 + request.method.length * 2;
    for (const key of Object.keys(params)) {
      const value = params[key];
      bytes += 24 + key.length * 2;
      if (key === 'params') {
        bytes += 32;
        continue;
      }
      if (typeof value === 'string') bytes += 16 + value.length * 2;
      else if (
        value === undefined ||
        value === null ||
        typeof value === 'boolean' ||
        typeof value === 'number'
      )
        bytes += 8;
      else return checkedRequestBytes(request, limit);
    }
    bytes += values.length * 8;
    for (const value of values) {
      if (typeof value === 'string') bytes += 16 + value.length * 2;
      else if (
        value === null ||
        typeof value === 'boolean' ||
        typeof value === 'number'
      )
        bytes += 8;
      else return checkedRequestBytes(request, limit);
    }
    return bytes > limit ? limit + 1 : bytes;
  }
  return checkedRequestBytes(request, limit);
};

describe('retained request accounting', () => {
  it('bounds actual graph size without expanding repeated aliases', () => {
    let graph: unknown = {value: 'shared'};
    for (let depth = 0; depth < 50; depth++) graph = [graph, graph];
    expect(requestBytes(graph, 8_192)).toBeLessThan(8_192);
    expect(requestBytes(graph, 100)).toBe(101);

    const child = {value: 'x'.repeat(100)};
    const shared = Array.from({length: 100}, () => child);
    const copied = Array.from({length: 100}, () => ({value: 'x'.repeat(100)}));
    expect(requestBytes(shared, 2_048)).toBeLessThan(2_048);
    expect(requestBytes(copied, 2_048)).toBe(2_049);
  });

  it('charges slots even when every value points to the same container', () => {
    const child = {};
    const manySlots = Array.from({length: 10_000}, () => child);
    expect(requestBytes(manySlots, 1_024)).toBe(1_025);
    expect(requestBytes(new Array(100_000_000), 1_024)).toBe(1_025);
  });

  it('terminates on cycles and traverses deep graphs without recursion', () => {
    const cyclic: {self?: unknown} = {};
    cyclic.self = cyclic;
    expect(requestBytes(cyclic, 1_024)).toBeLessThan(1_024);

    let deep: unknown = null;
    for (let depth = 0; depth < 20_000; depth++) deep = {child: deep};
    expect(requestBytes(deep, 2_000_000)).toBeLessThan(2_000_000);
    expect(requestBytes(deep, 1_024)).toBe(1_025);
  });

  it('charges UTF-16 keys, values, and named array properties', () => {
    expect(requestBytes('🦀', 20)).toBe(20);
    expect(requestBytes('🦀', 19)).toBe(20);
    expect(requestBytes({['x'.repeat(1_000)]: null}, 1_024)).toBe(1_025);

    const values: unknown[] & {extra?: string} = [];
    values.extra = 'x'.repeat(1_000);
    expect(requestBytes(values, 1_024)).toBe(1_025);
  });

  it('does not invoke getters and saturates at the exact boundary', () => {
    let invoked = false;
    const accessor = Object.defineProperty({}, 'value', {
      enumerable: true,
      get: () => {
        invoked = true;
        return 'untrusted';
      },
    });
    expect(requestBytes(accessor, 1_024)).toBe(1_025);
    expect(invoked).toBe(false);
    expect(requestBytes(null, 8)).toBe(8);
    expect(requestBytes(null, 7)).toBe(8);
    expect(requestBytes(undefined, 0)).toBe(1);
  });
});

describe('checked request accounting', () => {
  // Plain JSON-like graphs as structured clone delivers them: shared children
  // stay shared, and arrays may carry named properties.
  const random = (seed: number) => () => {
    seed = (seed * 1_103_515_245 + 12_345) % 2_147_483_648;
    return seed / 2_147_483_648;
  };
  const graph = (next: () => number): unknown => {
    const pool: object[] = [];
    const value = (depth: number): unknown => {
      const pick = next();
      if (depth > 4 || pick < 0.3) {
        return [null, true, 1.5, -7, 'text', '🦀'.repeat(3), ''][
          Math.floor(next() * 7)
        ];
      }
      if (pool.length && pick < 0.4) {
        return pool[Math.floor(next() * pool.length)];
      }
      const container: Record<string, unknown> | unknown[] =
        pick < 0.7
          ? Array.from({length: Math.floor(next() * 4)}, () => value(depth + 1))
          : Object.fromEntries(
              Array.from({length: Math.floor(next() * 4)}, (_, index) => [
                `key${index}`,
                value(depth + 1),
              ]),
            );
      if (Array.isArray(container) && next() < 0.2) {
        (container as unknown[] & {extra?: unknown}).extra = value(depth + 1);
      }
      pool.push(container);
      return container;
    };
    return structuredClone({
      v: 9,
      id: 1,
      method: 'executeSql',
      params: {sql: 'SELECT 1', params: [value(0), value(0)]},
    });
  };

  it('charges exactly what the general walk charges', () => {
    const next = random(42);
    for (let run = 0; run < 500; run++) {
      const value = graph(next);
      for (const limit of [64, 512, 4_096, 1_000_000]) {
        expect(checkedRequestBytes(value, limit)).toBe(
          requestBytes(value, limit),
        );
      }
    }
  });

  // The walk now sizes every request object, a statement's included, so what
  // the shortcut for statements charged must be what the walk charges.
  it('charges a statement with scalar parameters what its shortcut charged', () => {
    const requests = [
      {v: 9, id: 1, method: 'executePrepared', params: {statementId: 3, params: [1, 'updated']}},
      {v: 9, id: 2, method: 'executePrepared', params: {statementId: 3, params: [], transactionId: 'tx-1'}},
      {v: 9, id: 3, method: 'executePrepared', params: {statementId: 3, params: [null, true, 2.5, 'é'.repeat(40)], transactionId: 'tx-1', rowMode: 'array'}},
      {v: 9, id: 4, method: 'executeSql', params: {sql: 'SELECT * FROM t WHERE id = $1', params: [7]}},
      {v: 9, id: 5, method: 'executeSql', params: {sql: 'INSERT INTO t VALUES ($1)', params: [{nested: [1]}]}},
      {v: 9, id: 6, method: 'executePrepared', params: {statementId: 3, params: new Array(40).fill(1)}},
      {v: 9, id: 7, method: 'prepareSql', params: {sql: 'SELECT 1'}},
      {v: 9, id: 8, method: 'close', params: undefined},
      {v: 9, id: 9, method: 'executeSql', params: undefined},
    ];
    for (const request of requests.map((request) => structuredClone(request))) {
      for (const limit of [64, 512, 4_096, 1_000_000]) {
        expect(checkedRequestBytes(request, limit)).toBe(
          requestFormBytes(request, limit),
        );
      }
    }
  });

  it('falls back to the general walk for sparse arrays and other values', () => {
    const sparse = structuredClone({params: [1, , 3]});
    expect(checkedRequestBytes(sparse, 1_024)).toBe(
      requestBytes(sparse, 1_024),
    );
    expect(checkedRequestBytes(new Array(100_000_000), 1_024)).toBe(1_025);
    expect(checkedRequestBytes(1n, 1_024)).toBe(requestBytes(1n, 1_024));
    expect(checkedRequestBytes(null, 7)).toBe(8);
    expect(() => checkedRequestBytes(null, -1)).toThrow(RangeError);
  });
});

describe('statement request accounting', () => {
  const random = (seed: number) => () => {
    seed = (seed * 1_103_515_245 + 12_345) % 2_147_483_648;
    return seed / 2_147_483_648;
  };
  const SCALARS: JsonValue[] = [
    null,
    true,
    false,
    0,
    -7,
    1.5,
    '',
    'text',
    '🦀'.repeat(3),
    'é'.repeat(40),
  ];
  // A statement request as a structured clone delivers it, so that a
  // container several parameters share arrives shared.
  const generate = (next: () => number): StatementRequest => {
    const pick = <Item>(items: readonly Item[]): Item =>
      items[Math.floor(next() * items.length)]!;
    const pool: JsonValue[] = [];
    const value = (depth: number): JsonValue => {
      const roll = next();
      if (depth > 3 || roll < 0.55) return pick(SCALARS);
      if (pool.length > 0 && roll < 0.65) return pick(pool);
      const container: JsonValue =
        roll < 0.85
          ? Array.from({length: Math.floor(next() * 4)}, () => value(depth + 1))
          : Object.fromEntries(
              Array.from({length: Math.floor(next() * 4)}, (_, index) => [
                `key${index}`,
                value(depth + 1),
              ]),
            );
      pool.push(container);
      return container;
    };
    const scalarsOnly = next() < 0.6;
    const count = pick([0, 1, 2, 5, 40]);
    const params = Array.from({length: count}, () =>
      scalarsOnly ? pick(SCALARS) : value(0),
    );
    return structuredClone<StatementRequest>([
      PROTOCOL_VERSION,
      1 + Math.floor(next() * 1_000_000),
      ...(next() < 0.5
        ? ([STATEMENT_PREPARED, 1 + Math.floor(next() * 500)] as const)
        : ([STATEMENT_SQL, pick(['', 'SELECT 1', 'UPDATE t SET c = $1 WHERE id = $2', 'ü'.repeat(70)])] as const)),
      pick([0, 0, 'tx-1', '0b0e6e0e-7d5c-4a43-9d0c-3f7c1b2a9e11/tx-12']),
      pick([0, 0, 1]),
      ...params,
    ]);
  };
  // What the request it stands for was charged while statements were sent as
  // requests, which is also what the walk charges that request now.
  const charged = (request: StatementRequest, limit: number): number => {
    const stoodFor = statementRequest(request);
    const bytes = requestFormBytes(stoodFor, limit);
    expect(checkedRequestBytes(stoodFor, limit)).toBe(bytes);
    return bytes;
  };

  it('charges a statement request what the request it stands for is charged', () => {
    const next = random(7);
    let walked = 0;
    for (let run = 0; run < 2_000; run++) {
      const request = generate(next);
      expect(isStatementRequest(request)).toBe(true);
      expect(isWorkerRequest(statementRequest(request))).toBe(true);
      const exact = charged(request, MAX_QUEUED_BYTES);
      expect(statementBytes(request, MAX_QUEUED_BYTES)).toBe(exact);
      // At, just under and just over what it costs, and at bounds it exceeds
      // before, in, and after its fixed part.
      for (const limit of [exact, exact - 1, exact + 1, 0, 64, 350, 512, 4_096]) {
        if (limit >= 0) {
          expect(statementBytes(request, limit)).toBe(charged(request, limit));
        }
      }
      expect(statementBytes(request, exact - 1)).toBe(exact);
      if (request.slice(STATEMENT_PARAMS).some((value) => typeof value === 'object' && value !== null)) {
        walked += 1;
      }
    }
    // Both the requests of scalars and those that need the walk were met.
    expect(walked).toBeGreaterThan(200);
    expect(walked).toBeLessThan(1_800);
  });

  it('charges the forms of a statement exactly', () => {
    const cases: [StatementRequest, number][] = [
      // The request object 222 and its method's name, the key and value of
      // the statement, the `params` key 36 and its array 32.
      [[PROTOCOL_VERSION, 1, STATEMENT_PREPARED, 3, 0, 0], 374],
      [[PROTOCOL_VERSION, 1, STATEMENT_SQL, '', 0, 0], 356],
      [[PROTOCOL_VERSION, 1, STATEMENT_SQL, 'SELECT 1', 0, 0], 372],
      // A slot of 8 for each parameter, and then its value.
      [[PROTOCOL_VERSION, 1, STATEMENT_PREPARED, 3, 0, 0, 1, 'updated'], 428],
      // `transactionId` and its text, and `rowMode` and `array`.
      [[PROTOCOL_VERSION, 1, STATEMENT_PREPARED, 3, 'tx-1', 0], 448],
      [[PROTOCOL_VERSION, 1, STATEMENT_PREPARED, 3, 0, 1], 438],
      [[PROTOCOL_VERSION, 1, STATEMENT_PREPARED, 3, 'tx-1', 1, null, true], 544],
    ];
    for (const [request, bytes] of cases) {
      expect(statementBytes(request, 1_000_000)).toBe(bytes);
      expect(charged(request, 1_000_000)).toBe(bytes);
      expect(checkedRequestBytes(statementRequest(request), 1_000_000)).toBe(bytes);
    }
  });

  it('charges shared and many parameters as the walk does', () => {
    // A container that several parameters share is charged once.
    const child = {value: 'x'.repeat(100)};
    const shared = structuredClone<StatementRequest>([
      PROTOCOL_VERSION,
      1,
      STATEMENT_PREPARED,
      3,
      0,
      0,
      ...Array.from({length: 100}, () => child),
    ]);
    const copied = structuredClone<StatementRequest>([
      PROTOCOL_VERSION,
      1,
      STATEMENT_PREPARED,
      3,
      0,
      0,
      ...Array.from({length: 100}, () => ({value: 'x'.repeat(100)})),
    ]);
    for (const request of [shared, copied]) {
      for (const limit of [2_048, 1_000_000]) {
        expect(statementBytes(request, limit)).toBe(charged(request, limit));
      }
    }
    expect(statementBytes(shared, 2_048)).toBeLessThan(2_048);
    expect(statementBytes(copied, 2_048)).toBe(2_049);

    // More parameters than the request form's shortcut takes, and more
    // containers than its walk tells apart in a short list.
    const many = [
      PROTOCOL_VERSION,
      1,
      STATEMENT_SQL,
      'SELECT 1',
      'tx-1',
      1,
      ...Array.from({length: 5_000}, (_, index) => (index % 3 ? index : `p${index}`)),
    ] as StatementRequest;
    const nested = [
      PROTOCOL_VERSION,
      1,
      STATEMENT_SQL,
      'SELECT 1',
      0,
      0,
      ...Array.from({length: 100}, (_, index) => [index, {index}]),
    ] as StatementRequest;
    for (const request of [many, nested]) {
      const exact = charged(request, MAX_QUEUED_BYTES);
      expect(statementBytes(request, MAX_QUEUED_BYTES)).toBe(exact);
      expect(statementBytes(request, exact)).toBe(exact);
      expect(statementBytes(request, exact - 1)).toBe(exact);
      expect(statementBytes(request, 300)).toBe(charged(request, 300));
    }
  });

  it('refuses nothing below the bound, and saturates just past it', () => {
    // The largest statement the queue takes, to the byte, and the smallest it
    // refuses: a character more.
    const fill = (MAX_QUEUED_BYTES - 374 - 8 - 16) / 2;
    const largest: StatementRequest = [
      PROTOCOL_VERSION,
      1,
      STATEMENT_PREPARED,
      3,
      0,
      0,
      'x'.repeat(fill),
    ];
    const over: StatementRequest = [...largest];
    over[STATEMENT_PARAMS] = 'x'.repeat(fill + 1);
    expect(statementBytes(largest, MAX_QUEUED_BYTES)).toBe(MAX_QUEUED_BYTES);
    expect(charged(largest, MAX_QUEUED_BYTES)).toBe(MAX_QUEUED_BYTES);
    expect(statementBytes(over, MAX_QUEUED_BYTES)).toBe(MAX_QUEUED_BYTES + 1);
    expect(charged(over, MAX_QUEUED_BYTES)).toBe(MAX_QUEUED_BYTES + 1);
    // The fixed part alone can exceed a bound before any parameter is read.
    const container: StatementRequest = [
      PROTOCOL_VERSION,
      1,
      STATEMENT_SQL,
      'x'.repeat(1_000),
      0,
      0,
      [1],
    ];
    expect(statementBytes(container, 1_024)).toBe(1_025);
    expect(charged(container, 1_024)).toBe(1_025);
  });
});
