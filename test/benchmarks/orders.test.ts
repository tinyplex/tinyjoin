import {describe, expect, it} from 'vitest';

import {roundOrders} from '../../benchmarks/compare/orders.mjs';

// How often each engine followed each other engine within a round.
const successors = (orders: string[][]): Map<string, number> => {
  const counts = new Map<string, number>();
  for (const order of orders) {
    for (let index = 1; index < order.length; index++) {
      const pair = `${order[index - 1]} > ${order[index]}`;
      counts.set(pair, (counts.get(pair) ?? 0) + 1);
    }
  }
  return counts;
};

describe('the round orders', () => {
  it('use every arrangement of three engines once in six rounds', () => {
    const orders = roundOrders(['tinyjoin', 'sqlite', 'pglite'], 6);
    expect(new Set(orders.map((order) => order.join(','))).size).toBe(6);
    for (const order of orders) {
      expect([...order].sort()).toEqual(['pglite', 'sqlite', 'tinyjoin']);
    }
    expect([...successors(orders).values()]).toEqual([2, 2, 2, 2, 2, 2]);
    expect(orders.map((order) => order[0])).toEqual([
      'tinyjoin',
      'sqlite',
      'pglite',
      'tinyjoin',
      'pglite',
      'sqlite',
    ]);
  });

  it('continue with rotations of the given order beyond the arrangements', () => {
    const orders = roundOrders(['tinyjoin', 'sqlite', 'pglite'], 9);
    expect(orders.slice(6)).toEqual([
      ['tinyjoin', 'sqlite', 'pglite'],
      ['sqlite', 'pglite', 'tinyjoin'],
      ['pglite', 'tinyjoin', 'sqlite'],
    ]);
    const counts = successors(orders);
    expect(counts.get('pglite > tinyjoin')).toBe(4);
    expect(counts.get('sqlite > tinyjoin')).toBe(2);
    expect(counts.get('tinyjoin > pglite')).toBe(2);
  });

  it('alternate two engines, and repeat one', () => {
    expect(roundOrders(['a', 'b'], 5)).toEqual([
      ['a', 'b'],
      ['b', 'a'],
      ['a', 'b'],
      ['b', 'a'],
      ['a', 'b'],
    ]);
    expect(roundOrders(['a'], 2)).toEqual([['a'], ['a']]);
    expect(roundOrders([], 2)).toEqual([[], []]);
  });

  it('cover all 24 arrangements of four engines', () => {
    const orders = roundOrders(['a', 'b', 'c', 'd'], 26);
    expect(new Set(orders.slice(0, 24).map((order) => order.join(','))).size).toBe(24);
    expect([...successors(orders.slice(0, 24)).values()]).toEqual(
      Array.from({length: 12}, () => 6),
    );
    expect(orders.slice(24)).toEqual([
      ['a', 'b', 'c', 'd'],
      ['b', 'c', 'd', 'a'],
    ]);
  });
});
