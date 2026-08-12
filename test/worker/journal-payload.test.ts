import {describe, expect, it} from 'vitest';

import {
  decodeJournalTransaction,
  encodeJournalTransaction,
  JOURNAL_TRANSACTION_VERSION,
  MAX_JOURNAL_SQL_STATEMENTS,
  type JournalTransaction,
} from '../../src/worker/journal-payload.ts';

const decoder = new TextDecoder();
const encoder = new TextEncoder();

function transaction(
  mutations: JournalTransaction['mutations'],
): JournalTransaction {
  return {
    version: JOURNAL_TRANSACTION_VERSION,
    revisionBefore: 4,
    revisionAfter: 5,
    mutations,
  };
}

describe('journal transaction payload codec', () => {
  it('round-trips every deterministic mutation shape', () => {
    const value = transaction([
      {
        type: 'defineTables',
        schemas: [{name: 'posts', primaryKey: ['tenant', 'id']}],
      },
      {
        type: 'replaceTableSnapshot',
        schema: {name: 'posts', primaryKey: ['tenant', 'id']},
        rows: [{id: 1, tenant: 'acme', nested: {b: 2, a: [true, null]}}],
      },
      {
        type: 'applyBatch',
        batch: {
          sourceId: 'source',
          cursor: {kind: 'lsn', value: '0/16B6C50'},
          transactionId: 'tx-1',
          committedAt: '2026-08-13T00:00:00Z',
          changes: [
            {type: 'upsert', table: 'posts', row: {tenant: 'acme', id: 2}},
            {type: 'delete', table: 'posts', key: {tenant: 'acme', id: 1}},
          ],
        },
      },
      {
        type: 'executeSql',
        statements: [
          {sql: 'INSERT INTO posts VALUES ($1, $2)', params: ['acme', 3]},
          {sql: 'UPDATE posts SET title = $1', params: ['hello']},
        ],
      },
    ]);

    expect(decodeJournalTransaction(encodeJournalTransaction(value))).toEqual(
      value,
    );
  });

  it('canonicalizes arbitrary JSON object key order', () => {
    const left = transaction([
      {
        type: 'replaceTableSnapshot',
        schema: {name: 'posts', primaryKey: ['id']},
        rows: [{z: {b: 2, a: 1}, id: 1}],
      },
    ]);
    const right = transaction([
      {
        type: 'replaceTableSnapshot',
        schema: {name: 'posts', primaryKey: ['id']},
        rows: [{id: 1, z: {a: 1, b: 2}}],
      },
    ]);

    expect([...encodeJournalTransaction(left)]).toEqual([
      ...encodeJournalTransaction(right),
    ]);
  });

  it('returns a detached JSON-only value', () => {
    const row = {id: 1, labels: ['old']};
    const encoded = encodeJournalTransaction(
      transaction([
        {
          type: 'replaceTableSnapshot',
          schema: {name: 'posts', primaryKey: ['id']},
          rows: [row],
        },
      ]),
    );
    row.labels[0] = 'new';

    const first = decodeJournalTransaction(encoded);
    const second = decodeJournalTransaction(encoded);
    expect(first.mutations[0]).toMatchObject({rows: [{labels: ['old']}]});
    expect(first).not.toBe(second);
    expect(first.mutations[0]).not.toBe(second.mutations[0]);
  });

  it('round-trips a batch with only its required changes field', () => {
    const value = transaction([
      {type: 'applyBatch', batch: {changes: []}},
    ]);

    expect(decodeJournalTransaction(encodeJournalTransaction(value))).toEqual(
      value,
    );
  });

  it('rejects unknown versions and fields at every fixed-shape layer', () => {
    const valid = transaction([
      {
        type: 'executeSql',
        statements: [{sql: 'DELETE FROM posts', params: []}],
      },
    ]);
    expect(() => encodeUnknown({...valid, version: 2})).toThrowError(
      expect.objectContaining({code: 'STORAGE_VERSION_UNSUPPORTED'}),
    );
    expect(() => encodeUnknown({...valid, surprise: true})).toThrowError(
      expect.objectContaining({code: 'STORAGE_JOURNAL_PAYLOAD_INVALID'}),
    );
    expect(() =>
      encodeUnknown({
        ...valid,
        mutations: [
          {
            type: 'executeSql',
            statements: [{sql: 'DELETE FROM posts', params: [], extra: true}],
          },
        ],
      }),
    ).toThrowError(/extra is not supported/);
    expect(() =>
      encodeUnknown({
        ...valid,
        mutations: [
          {
            type: 'applyBatch',
            batch: {changes: [], extra: true},
          },
        ],
      }),
    ).toThrowError(/extra is not supported/);
  });

  it('rejects invalid revision bounds and empty transactions', () => {
    const valid = transaction([
      {type: 'defineTables', schemas: [{name: 'posts', primaryKey: ['id']}]},
    ]);
    expect(() =>
      encodeUnknown({...valid, revisionBefore: -1}),
    ).toThrowError(/revisionBefore/);
    expect(() =>
      encodeUnknown({...valid, revisionAfter: 3}),
    ).toThrowError(/revisionAfter cannot/);
    expect(() => encodeUnknown({...valid, mutations: []})).toThrowError(
      /between 1 and/,
    );
  });

  it('rejects sparse arrays, cycles, accessors, and non-finite numbers', () => {
    const sparse = Array<unknown>(1);
    expect(() =>
      encodeUnknown({
        ...transaction([]),
        mutations: [
          {
            type: 'executeSql',
            statements: [{sql: 'SELECT $1', params: sparse}],
          },
        ],
      }),
    ).toThrowError(/cannot be sparse/);

    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    expect(() =>
      encodeUnknown({
        ...transaction([]),
        mutations: [
          {
            type: 'replaceTableSnapshot',
            schema: {name: 'posts', primaryKey: ['id']},
            rows: [cyclic],
          },
        ],
      }),
    ).toThrowError(/cyclic/);

    const accessor = Object.defineProperty({}, 'id', {
      enumerable: true,
      get: () => 1,
    });
    expect(() =>
      encodeUnknown(
        transaction([
          {
            type: 'replaceTableSnapshot',
            schema: {name: 'posts', primaryKey: ['id']},
            rows: [accessor],
          },
        ]),
      ),
    ).toThrowError(/enumerable data property/);

    expect(() =>
      encodeUnknown(
        transaction([
          {
            type: 'executeSql',
            statements: [{sql: 'SELECT $1', params: [Number.POSITIVE_INFINITY]}],
          },
        ]),
      ),
    ).toThrowError(/finite JSON numbers/);
  });

  it('enforces statement and nesting limits before serialization', () => {
    const statement = {sql: 'SELECT 1', params: []};
    expect(() =>
      encodeUnknown(
        transaction([
          {
            type: 'executeSql',
            statements: Array.from(
              {length: MAX_JOURNAL_SQL_STATEMENTS + 1},
              () => statement,
            ),
          },
        ]),
      ),
    ).toThrowError(/between 1 and/);

    let nested: unknown = null;
    for (let depth = 0; depth < 66; depth += 1) {
      nested = [nested];
    }
    expect(() =>
      encodeUnknown({
        ...transaction([]),
        mutations: [
          {
            type: 'executeSql',
            statements: [{sql: 'SELECT $1', params: [nested]}],
          },
        ],
      }),
    ).toThrowError(/nesting depth/);
  });

  it('rejects malformed UTF-8 and JSON when decoding', () => {
    expect(() => decodeJournalTransaction(new Uint8Array([0xff]))).toThrowError(
      /UTF-8 JSON/,
    );
    expect(() =>
      decodeJournalTransaction(encoder.encode('{"version":1')),
    ).toThrowError(/UTF-8 JSON/);
    expect(() => decodeJournalTransaction(new Uint8Array())).toThrowError(
      /cannot be empty/,
    );
  });

  it('accepts schema-only transactions without a revision change', () => {
    const value: JournalTransaction = {
      version: JOURNAL_TRANSACTION_VERSION,
      revisionBefore: 8,
      revisionAfter: 8,
      mutations: [
        {type: 'defineTables', schemas: [{name: 'posts', primaryKey: ['id']}]},
      ],
    };

    expect(
      JSON.parse(decoder.decode(encodeJournalTransaction(value))),
    ).toMatchObject({revisionBefore: 8, revisionAfter: 8});
  });
});

function encodeUnknown(value: unknown): Uint8Array {
  return encodeJournalTransaction(value as JournalTransaction);
}
