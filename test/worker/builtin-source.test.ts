import {describe, expect, it, vi} from 'vitest';

import {
  bindOpfsStorageName,
  builtinSourceConfigurationKey,
  mergeSourceSchemas,
  prepareBuiltinSource,
  sourceStorageIdentity,
} from '../../src/worker/builtin-source.ts';

function source(overrides: Record<string, unknown> = {}) {
  return {
    kind: 'supabase' as const,
    url: 'https://example.supabase.co/project/?ignored=yes#hash',
    publishableKey: '  sb_publishable_example  ',
    tables: [
      {
        table: 'posts',
        primaryKey: ['id'],
        columns: ['id', 'title'],
      },
    ],
    ...overrides,
  };
}

describe('built-in source preparation', () => {
  it('normalizes anonymous Supabase config and defaults schema to public', () => {
    expect(prepareBuiltinSource(source())).toEqual({
      options: {
        kind: 'supabase',
        url: 'https://example.supabase.co/project',
        publishableKey: 'sb_publishable_example',
        tables: [
          {
            schema: 'public',
            table: 'posts',
            primaryKey: ['id'],
            localName: 'posts',
            columns: ['id', 'title'],
          },
        ],
        id: 'supabase:https://example.supabase.co/project',
        pageSize: 500,
        maxSnapshotPasses: 3,
      },
      schemas: [{name: 'posts', primaryKey: ['id']}],
    });
  });

  it.each([
    source({publishableKey: 'sb_secret_example'}),
    source({url: 'https://user:password@example.supabase.co'}),
    source({tables: []}),
    source({tables: [{table: 'bad-name', primaryKey: ['id']}]}),
    source({
      tables: [{table: 'posts', primaryKey: ['id'], columns: ['title']}],
    }),
    source({pageSize: 0}),
  ])('rejects unsafe or invalid source configuration', (options) => {
    expect(() => prepareBuiltinSource(options)).toThrowError(
      expect.objectContaining({code: 'SUPABASE_INVALID_CONFIG'}),
    );
  });

  it('merges identical source schemas and rejects explicit conflicts', () => {
    const prepared = prepareBuiltinSource(source());
    expect(
      mergeSourceSchemas(
        [{name: 'posts', primaryKey: ['id']}],
        prepared.schemas,
      ),
    ).toEqual([{name: 'posts', primaryKey: ['id']}]);
    expect(() =>
      mergeSourceSchemas(
        [{name: 'posts', primaryKey: ['slug']}],
        prepared.schemas,
      ),
    ).toThrowError(expect.objectContaining({code: 'SOURCE_SCHEMA_CONFLICT'}));
  });
});

describe('built-in source OPFS identity', () => {
  it('canonicalizes table order and omits source id and tuning', () => {
    const first = prepareBuiltinSource(
      source({
        id: 'first',
        pageSize: 10,
        maxSnapshotPasses: 2,
        tables: [
          {
            schema: 'private',
            table: 'notes',
            localName: 'private.notes',
            primaryKey: ['id'],
          },
          {
            table: 'posts',
            primaryKey: ['id'],
            columns: ['id', 'title'],
          },
        ],
      }),
    );
    const second = prepareBuiltinSource(
      source({
        id: 'second',
        pageSize: 1_000,
        maxSnapshotPasses: 9,
        tables: [...first.options.tables].reverse(),
      }),
    );

    expect(sourceStorageIdentity('cache', first.options)).toBe(
      sourceStorageIdentity('cache', second.options),
    );
    expect(builtinSourceConfigurationKey(first)).not.toBe(
      builtinSourceConfigurationKey(second),
    );
  });

  it('hashes the complete canonical identity into a valid physical name', async () => {
    const prepared = prepareBuiltinSource(source());
    const identity = sourceStorageIdentity(
      'application-cache',
      prepared.options,
    );
    const hasher = vi.fn(() => 'a'.repeat(64));

    await expect(
      bindOpfsStorageName('application-cache', prepared, hasher),
    ).resolves.toBe('a'.repeat(64));
    expect(hasher).toHaveBeenCalledWith(identity);
    await expect(
      bindOpfsStorageName('application-cache', prepared),
    ).resolves.toMatch(/^[a-f0-9]{64}$/);

    const sameIdentity = prepareBuiltinSource(
      source({id: 'display-only', pageSize: 50, maxSnapshotPasses: 7}),
    );
    const changedProject = prepareBuiltinSource(
      source({url: 'https://different.supabase.co'}),
    );
    const physicalName = await bindOpfsStorageName(
      'application-cache',
      prepared,
    );
    await expect(
      bindOpfsStorageName('application-cache', sameIdentity),
    ).resolves.toBe(physicalName);
    await expect(
      bindOpfsStorageName('application-cache', changedProject),
    ).resolves.not.toBe(physicalName);
  });

  it('validates the user-visible name before hashing it', async () => {
    const hasher = vi.fn(() => 'a'.repeat(64));
    await expect(
      bindOpfsStorageName('../shared', prepareBuiltinSource(source()), hasher),
    ).rejects.toMatchObject({code: 'INVALID_STORAGE_NAME'});
    expect(hasher).not.toHaveBeenCalled();
  });

  it('rejects incomplete or non-hex source fingerprints', async () => {
    await expect(
      bindOpfsStorageName(
        'application-cache',
        prepareBuiltinSource(source()),
        () => 'weak-hash',
      ),
    ).rejects.toMatchObject({code: 'SOURCE_FINGERPRINT_INVALID'});
  });
});
