import {describe, expect, it} from 'vitest';

import {PROTOCOL_VERSION, isWorkerRequest} from '../src/protocol.ts';

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
        v: 2,
        id: 1,
        method: 'init',
        params: {schemas: [], storage: {kind: 'memory'}, source},
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
});
