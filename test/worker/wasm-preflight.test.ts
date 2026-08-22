import {describe, expect, it} from 'vitest';

import type {JsonValue} from '../../src/protocol.ts';
import {
  preflightClosePrepared,
  preflightExecSql,
  preflightExecutePrepared,
  preflightExecuteSql,
  preflightPrepareSql,
} from '../../src/worker/wasm-preflight.ts';

describe('WASM request preflight', () => {
  it('accepts every SQL-first structured request shape', () => {
    expect(() =>
      preflightExecuteSql('SELECT $1, $2', [
        -0,
        {nested: [true, null, 1.5, '\ud800']},
      ]),
    ).not.toThrow();
    expect(() => preflightPrepareSql('SELECT $1')).not.toThrow();
    expect(() => preflightExecutePrepared(7, [1, 'two'])).not.toThrow();
    expect(() => preflightClosePrepared(7)).not.toThrow();
    expect(() =>
      preflightExecSql('CREATE TABLE items; SELECT * FROM items'),
    ).not.toThrow();
  });

  it('rejects sparse arrays, accessors, revoked proxies, and oversized work', () => {
    const sparse = new Array<JsonValue>(1);
    expect(() => preflightExecuteSql('SELECT $1', sparse)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );

    let getterCalls = 0;
    const accessor = Object.defineProperty({}, 'id', {
      enumerable: true,
      get() {
        getterCalls += 1;
        return 1;
      },
    });
    expect(() =>
      preflightExecuteSql('SELECT $1', [accessor as JsonValue]),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
    expect(getterCalls).toBe(0);

    const revoked = Proxy.revocable([], {});
    revoked.revoke();
    expect(() => preflightExecuteSql('SELECT 1', revoked.proxy)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );

    expect(() => preflightExecuteSql('SELECT 1', new Array(1_000_001))).toThrow(
      expect.objectContaining({code: 'RESOURCE_LIMIT'}),
    );
    expect(() => preflightExecuteSql('x'.repeat(16 * 1024 * 1024), [])).toThrow(
      expect.objectContaining({code: 'RESOURCE_LIMIT'}),
    );

    for (const invalidId of [0, -1, 1.5, 0x1_0000_0000]) {
      expect(() => preflightExecutePrepared(invalidId, [])).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
      expect(() => preflightClosePrepared(invalidId)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
  });

  it('checks the retained Rust-model estimate without allocating a copy', () => {
    // wasm32 Vec<Value> is 12 bytes and each serde_json::Value slot is 24.
    expect(() =>
      preflightExecuteSql('SELECT', new Array<JsonValue>(699_049).fill(null)),
    ).not.toThrow();
    expect(() =>
      preflightExecuteSql('SELECT', new Array<JsonValue>(699_050).fill(null)),
    ).toThrow(expect.objectContaining({code: 'RESOURCE_LIMIT'}));
  }, 15_000);

  it('accepts JSON depth 64 and rejects the first deeper node', () => {
    const nested = (depth: number): JsonValue => {
      let value: JsonValue = null;
      for (let index = 0; index < depth; index += 1) {
        value = [value];
      }
      return value;
    };

    expect(() => preflightExecuteSql('SELECT $1', [nested(64)])).not.toThrow();
    expect(() => preflightExecuteSql('SELECT $1', [nested(65)])).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
  });

  it('rejects non-finite JSON numbers', () => {
    for (const value of [Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(() => preflightExecuteSql('SELECT $1', [value])).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
  });
});
