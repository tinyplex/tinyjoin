import {describe, expect, it} from 'vitest';

import {
  MAX_QUERY_POSITION,
  PROTOCOL_VERSION,
  isWorkerEvent,
  isWorkerRequest,
} from '../src/protocol.ts';

const source = {
  kind: 'supabase',
  url: 'https://example.supabase.co',
  publishableKey: 'sb_publishable_example',
  tables: [
    {
      table: 'posts',
      primaryKey: ['id'],
      columns: ['id', 'title'],
    },
  ],
} as const;

describe('built-in source protocol', () => {
  it('accepts an exact serializable Supabase init descriptor', () => {
    const request = {
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {
        schemas: [],
        storage: {kind: 'opfs', name: 'posts'},
        source,
      },
    };

    expect(isWorkerRequest(request)).toBe(true);
    expect(structuredClone(request)).toEqual(request);
  });

  it.each([
    {
      ...source,
      transport: () => undefined,
    },
    {
      ...source,
      tables: [{...source.tables[0], unknown: true}],
    },
    {
      ...source,
      pageSize: 2.5,
    },
    {
      ...source,
      kind: 'custom',
    },
  ])('rejects a non-contract source shape', (invalidSource) => {
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'init',
        params: {
          schemas: [],
          storage: {kind: 'memory'},
          source: invalidSource,
        },
      }),
    ).toBe(false);
  });

  it('rejects unknown init fields and the previous protocol version', () => {
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'init',
        params: {
          schemas: [],
          storage: {kind: 'memory'},
          source,
          legacySource: source,
        },
      }),
    ).toBe(false);
    expect(
      isWorkerRequest({
        v: 3,
        id: 1,
        method: 'init',
        params: {schemas: [], storage: {kind: 'memory'}, source},
      }),
    ).toBe(false);
  });

  it('accepts the writable SQL and transaction protocol surface', () => {
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 2,
        method: 'executeSql',
        params: {
          sql: 'INSERT INTO posts (id, title) VALUES ($1, $2) RETURNING *',
          params: [1, 'hello'],
        },
      }),
    ).toBe(true);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 3,
        method: 'beginTransaction',
        params: undefined,
      }),
    ).toBe(true);
    for (const method of ['querySql', 'executeSql'] as const) {
      expect(
        isWorkerRequest({
          v: PROTOCOL_VERSION,
          id: 4,
          method,
          params: {
            sql: 'SELECT * FROM posts',
            params: [],
            transactionId: 'tx-1',
          },
        }),
      ).toBe(true);
    }
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 5,
        method: 'query',
        params: {
          plan: {table: 'posts', filters: []},
          transactionId: 'tx-1',
        },
      }),
    ).toBe(true);
    for (const method of [
      'commitTransaction',
      'rollbackTransaction',
    ] as const) {
      expect(
        isWorkerRequest({
          v: PROTOCOL_VERSION,
          id: 6,
          method,
          params: {transactionId: 'tx-1'},
        }),
      ).toBe(true);
    }
  });

  it('validates structured comparisons, ordering, and offsets', () => {
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 8,
        method: 'query',
        params: {
          plan: {
            table: 'tasks',
            filters: [{column: 'priority', operator: 'gte', value: 2}],
            orderBy: [
              {column: 'priority', direction: 'desc', nulls: 'last'},
            ],
            offset: 10,
            limit: 5,
          },
        },
      }),
    ).toBe(true);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 8,
        method: 'query',
        params: {
          plan: {
            table: 'tasks',
            filters: [],
            offset: MAX_QUERY_POSITION,
            limit: 0,
          },
        },
      }),
    ).toBe(true);
    for (const plan of [
      {table: 'tasks', filters: [], offset: -1},
      {table: 'tasks', filters: [], limit: MAX_QUERY_POSITION + 1},
      {table: 'tasks', filters: [], offset: MAX_QUERY_POSITION + 1},
      {
        table: 'tasks',
        filters: [],
        offset: MAX_QUERY_POSITION,
        limit: 1,
      },
      {
        table: 'tasks',
        filters: [{column: 'id', operator: 'contains', value: 1}],
      },
      {
        table: 'tasks',
        filters: [],
        orderBy: [{column: 'id', direction: 'sideways', nulls: 'default'}],
      },
    ]) {
      expect(
        isWorkerRequest({
          v: PROTOCOL_VERSION,
          id: 9,
          method: 'query',
          params: {plan},
        }),
      ).toBe(false);
    }
  });

  it.each([
    {
      method: 'executeSql',
      params: {
        sql: 'INSERT INTO posts (id) VALUES (1)',
        params: [],
        extra: true,
      },
    },
    {
      method: 'executeSql',
      params: {sql: 'INSERT INTO posts (id) VALUES ($1)', params: [1n]},
    },
    {method: 'beginTransaction', params: {}},
    {method: 'commitTransaction', params: {transactionId: ''}},
    {
      method: 'rollbackTransaction',
      params: {transactionId: 'x'.repeat(129)},
    },
    {
      method: 'querySql',
      params: {sql: 'SELECT * FROM posts', params: [], transactionId: 1},
    },
  ])('rejects an invalid writable protocol request', ({method, params}) => {
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 7,
        method,
        params,
      }),
    ).toBe(false);
  });

  it('rejects sparse arrays and explicit undefined optional fields', () => {
    const sparseTables = new Array(1);
    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'init',
        params: {
          schemas: [],
          storage: {kind: 'memory'},
          source: {...source, tables: sparseTables},
        },
      }),
    ).toBe(false);

    const sparse = <T>(value: T): T[] => {
      const values = new Array<T>(2);
      values[1] = value;
      return values;
    };
    const schema = {name: 'posts', primaryKey: ['id']};
    const requests = [
      {
        method: 'init',
        params: {
          schemas: sparse(schema),
          storage: {kind: 'memory'},
        },
      },
      {
        method: 'defineTable',
        params: {schema: {...schema, primaryKey: sparse('id')}},
      },
      {
        method: 'replaceTable',
        params: {schema, rows: sparse({id: 1})},
      },
      {
        method: 'replaceTable',
        params: {schema, rows: [{id: sparse(1)}]},
      },
      {
        method: 'applyBatch',
        params: {
          batch: {
            changes: sparse({type: 'upsert', table: 'posts', row: {id: 1}}),
          },
        },
      },
      {
        method: 'query',
        params: {
          plan: {
            table: 'posts',
            columns: sparse('id'),
            filters: [],
          },
        },
      },
      {
        method: 'query',
        params: {
          plan: {
            table: 'posts',
            filters: sparse({column: 'id', operator: 'eq', value: 1}),
          },
        },
      },
      {
        method: 'query',
        params: {
          plan: {
            table: 'posts',
            filters: [],
            orderBy: sparse({
              column: 'id',
              direction: 'asc',
              nulls: 'default',
            }),
          },
        },
      },
      {
        method: 'querySql',
        params: {sql: 'SELECT $1', params: sparse(1)},
      },
    ];
    for (const [index, request] of requests.entries()) {
      expect(
        isWorkerRequest({
          v: PROTOCOL_VERSION,
          id: index + 10,
          ...request,
        }),
      ).toBe(false);
    }

    expect(
      isWorkerRequest({
        v: PROTOCOL_VERSION,
        id: 1,
        method: 'init',
        params: {
          schemas: [],
          storage: {kind: 'memory'},
          source: undefined,
        },
      }),
    ).toBe(false);
  });

  it('rejects sparse arrays in worker events', () => {
    const tables = new Array<string>(2);
    tables[1] = 'posts';
    expect(
      isWorkerEvent({
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 1, tables},
      }),
    ).toBe(false);
  });
});
