import {describe, expect, it} from 'vitest';

import type {
  JsonValue,
  QueryPlan,
  Row,
  TableSchema,
} from '../../src/protocol.ts';
import {
  preflightApplyBatch,
  preflightClosePrepared,
  preflightDefineTables,
  preflightExecSql,
  preflightExecutePrepared,
  preflightExecuteSql,
  preflightPrepareSql,
  preflightQuery,
  preflightReplaceSnapshot,
  type BridgeTableSchema,
} from '../../src/worker/wasm-preflight.ts';

const schema: TableSchema = {name: 'items', primaryKey: ['id']};
const query: QueryPlan = {
  table: 'items',
  columns: ['id', 'title'],
  filters: [{column: 'id', operator: 'gte', value: 1}],
  orderBy: [{column: 'id', direction: 'desc', nulls: 'last'}],
  limit: 10,
  offset: 2,
};

describe('WASM request preflight', () => {
  it('accepts every structured request shape', () => {
    const typed: BridgeTableSchema = {
      name: 'typed',
      primaryKey: ['id'],
      columns: [
        {name: 'id', dataType: 'integer', nullable: false},
        {
          name: 'payload',
          dataType: 'json',
          default: {enabled: true, threshold: 1.5},
        },
        {name: 'optional', dataType: 'text', default: null},
      ],
    };

    expect(() => preflightDefineTables([typed])).not.toThrow();
    expect(() =>
      preflightReplaceSnapshot(schema, [
        {id: 1, nested: {zero: -0, list: [true, null]}},
      ]),
    ).not.toThrow();
    expect(() =>
      preflightApplyBatch({
        changes: [
          {type: 'upsert', table: 'items', row: {id: 2}},
          {type: 'delete', table: 'items', key: {id: 1}},
        ],
      }),
    ).not.toThrow();
    expect(() => preflightQuery(query)).not.toThrow();
    expect(() =>
      preflightExecuteSql('SELECT $1, $2', [-0, '\ud800']),
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
    expect(() => preflightReplaceSnapshot(schema, [accessor as Row])).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
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

  it('preserves missing/null defaults and rejects invalid optionals', () => {
    const withNullDefault = {
      name: 'typed',
      primaryKey: ['id'],
      columns: [{name: 'id', dataType: 'integer', default: null}],
    } satisfies BridgeTableSchema;

    expect(() => preflightDefineTables([withNullDefault])).not.toThrow();
    expect(() =>
      preflightQuery({...query, columns: null} as unknown as QueryPlan),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
    expect(() =>
      preflightQuery({...query, unknown: true} as unknown as QueryPlan),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
  });

  it('rejects invalid numeric and enum boundaries', () => {
    for (const value of [Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(() => preflightExecuteSql('SELECT $1', [value])).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
    expect(() => preflightQuery({...query, limit: 0x1_0000_0000})).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
    expect(() =>
      preflightDefineTables([
        {
          name: 'items',
          primaryKey: ['id'],
          columns: [{name: 'id', dataType: 'uuid'}],
        } as unknown as BridgeTableSchema,
      ]),
    ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
  });
});
