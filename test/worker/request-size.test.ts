import {describe, expect, it} from 'vitest';
import {
  checkedRequestBytes,
  requestBytes,
  statementRequestBytes,
} from '../../src/worker/request-size.js';

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

  it('estimates a statement with scalar parameters exactly as the walk does', () => {
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
        expect(statementRequestBytes(request, limit)).toBe(
          checkedRequestBytes(request, limit),
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
