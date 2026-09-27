import {describe, expect, it} from 'vitest';

import type {JsonValue} from '../../src/protocol.ts';
import {WASM_OPERATION} from '../../src/worker/wasm-bridge.ts';
import {
  encodeClosePrepared,
  encodeExecSql,
  encodeExecutePrepared,
  encodeExecuteSql,
  encodePrepareSql,
} from '../../src/worker/wasm-preflight.ts';
import {decodeRequest} from '../helpers/wasm-request.ts';

describe('WASM request writing', () => {
  it('writes every SQL-first request as WASM reads it', () => {
    const params = [
      -0,
      1.5,
      9_007_199_254_740_993,
      {nested: [true, null, 'é😀'], ['__proto__']: 'data'},
    ] satisfies JsonValue[];
    expect(
      decodeRequest(
        WASM_OPERATION.executeSql,
        encodeExecuteSql('SELECT $1, $2', params, false),
      ),
    ).toEqual({sql: 'SELECT $1, $2', params});
    expect(
      decodeRequest(WASM_OPERATION.prepareSql, encodePrepareSql('SELECT $1')),
    ).toBe('SELECT $1');
    expect(
      decodeRequest(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(7, [1, 'two'], true),
      ),
    ).toEqual({statementId: 7, params: [1, 'two'], arrayRows: true});
    expect(
      decodeRequest(WASM_OPERATION.closePrepared, encodeClosePrepared(7)),
    ).toBe(7);
    expect(
      decodeRequest(
        WASM_OPERATION.execSql,
        encodeExecSql('CREATE TABLE items; SELECT * FROM items', false),
      ),
    ).toEqual({sql: 'CREATE TABLE items; SELECT * FROM items'});
  });

  it('writes a lone surrogate as TextEncoder does, as U+FFFD', () => {
    expect(
      decodeRequest(
        WASM_OPERATION.executeSql,
        encodeExecuteSql('SELECT $1', ['a\ud800b'], false),
      ),
    ).toEqual({sql: 'SELECT $1', params: ['a�b']});
  });

  it('writes each request afresh into the reused buffer, growing it as needed', () => {
    const large = 'x'.repeat(100_000);
    const first = decodeRequest(
      WASM_OPERATION.executeSql,
      encodeExecuteSql('SELECT $1', [large], false),
    );
    expect(first).toEqual({sql: 'SELECT $1', params: [large]});
    expect(
      decodeRequest(
        WASM_OPERATION.executeSql,
        encodeExecuteSql('SELECT 1', [], true),
      ),
    ).toEqual({sql: 'SELECT 1', params: [], arrayRows: true});
  });

  it('keeps every value when the buffer grows in the middle of one', () => {
    // Enough values to cross several capacities, at every alignment of tags,
    // numbers, and strings, and then to be let go as unusually large.
    const params: JsonValue[] = Array.from({length: 200_000}, (_, index) =>
      index % 3 === 0 ? index : index % 3 === 1 ? index + 0.5 : `v${index}`,
    );
    expect(
      decodeRequest(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(3, params, false),
      ),
    ).toEqual({statementId: 3, params});
    expect(
      decodeRequest(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(4, [{a: [1, 'b']}], false),
      ),
    ).toEqual({statementId: 4, params: [{a: [1, 'b']}]});
  });

  it('rejects sparse arrays, accessors, revoked proxies, and oversized work', () => {
    const sparse = new Array<JsonValue>(1);
    expect(() => encodeExecuteSql('SELECT $1', sparse, false)).toThrow(
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
      encodeExecuteSql('SELECT $1', [accessor as JsonValue], false),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
    expect(getterCalls).toBe(0);

    const revoked = Proxy.revocable([], {});
    revoked.revoke();
    expect(() => encodeExecuteSql('SELECT 1', revoked.proxy, false)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );

    expect(() =>
      encodeExecuteSql('SELECT 1', new Array(1_000_001), false),
    ).toThrow(expect.objectContaining({code: 'RESOURCE_LIMIT'}));
    expect(() =>
      encodeExecuteSql('x'.repeat(16 * 1024 * 1024), [], false),
    ).toThrow(expect.objectContaining({code: 'RESOURCE_LIMIT'}));

    for (const invalidId of [0, -1, 1.5, 0x1_0000_0000]) {
      expect(() => encodeExecutePrepared(invalidId, [], false)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
      expect(() => encodeClosePrepared(invalidId)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
  });

  it('checks the retained Rust-model estimate', () => {
    // wasm32 Vec<Value> is 12 bytes and each serde_json::Value slot is 24.
    expect(() =>
      encodeExecuteSql('SELECT', new Array<JsonValue>(699_049).fill(null), false),
    ).not.toThrow();
    expect(() =>
      encodeExecuteSql('SELECT', new Array<JsonValue>(699_050).fill(null), false),
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

    expect(() =>
      encodeExecuteSql('SELECT $1', [nested(64)], false),
    ).not.toThrow();
    expect(() => encodeExecuteSql('SELECT $1', [nested(65)], false)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
  });

  it('rejects non-finite JSON numbers', () => {
    for (const value of [Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(() => encodeExecuteSql('SELECT $1', [value], false)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
  });
});
