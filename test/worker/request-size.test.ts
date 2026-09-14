import {describe, expect, it} from 'vitest';
import {requestBytes} from '../../src/worker/request-size.js';

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
