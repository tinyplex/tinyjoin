import type {ReplicaSource} from '../adapters/types.js';
import type {TableSchema} from '../protocol.js';
import {
  normalizeSupabaseSourceOptions,
  type NormalizedSupabaseSourceOptions,
  type NormalizedSupabaseTableOptions,
  type SourceOptions,
} from '../source-options.js';
import {assertDatabaseName} from './storage-error.js';

const SOURCE_STORAGE_FORMAT_VERSION = 1;

export interface PreparedBuiltinSource {
  readonly options: NormalizedSupabaseSourceOptions;
  readonly schemas: TableSchema[];
}

export type BuiltinSourceFactory = (
  options: NormalizedSupabaseSourceOptions,
) => ReplicaSource | PromiseLike<ReplicaSource>;

export type SourceIdentityHasher = (
  identity: string,
) => string | PromiseLike<string>;

export function prepareBuiltinSource(
  source: SourceOptions,
): PreparedBuiltinSource {
  switch (source.kind) {
    case 'supabase': {
      const options = normalizeSupabaseSourceOptions(source);
      return {
        options,
        schemas: options.tables.map((table) => ({
          name: table.localName,
          primaryKey: [...table.primaryKey],
        })),
      };
    }
  }
}

export function mergeSourceSchemas(
  schemas: readonly TableSchema[],
  sourceSchemas: readonly TableSchema[],
): TableSchema[] {
  const merged = schemas.map(copySchema);
  const existing = new Map<string, TableSchema>();
  for (const schema of schemas) {
    existing.set(schema.name, schema);
  }
  for (const schema of sourceSchemas) {
    const previous = existing.get(schema.name);
    if (!previous) {
      const copy = copySchema(schema);
      existing.set(copy.name, copy);
      merged.push(copy);
      continue;
    }
    if (!sameStrings(previous.primaryKey, schema.primaryKey)) {
      throw sourceError(
        'SOURCE_SCHEMA_CONFLICT',
        `Built-in source table \`${schema.name}\` conflicts with an explicitly configured schema`,
      );
    }
  }
  return merged;
}

export async function bindOpfsStorageName(
  databaseName: string,
  source: PreparedBuiltinSource,
  hasher: SourceIdentityHasher = sha256,
): Promise<string> {
  assertDatabaseName(databaseName);
  const physicalName = await hasher(
    sourceStorageIdentity(databaseName, source.options),
  );
  if (!/^[a-f0-9]{64}$/.test(physicalName)) {
    throw sourceError(
      'SOURCE_FINGERPRINT_INVALID',
      'TinyGres source identity hashing did not return a complete SHA-256 fingerprint',
    );
  }
  return physicalName;
}

export function sourceStorageIdentity(
  databaseName: string,
  source: NormalizedSupabaseSourceOptions,
): string {
  const tables = [...source.tables].sort(compareTables).map((table) => ({
    schema: table.schema,
    table: table.table,
    localName: table.localName,
    primaryKey: [...table.primaryKey],
    columns: table.columns ? [...table.columns] : null,
  }));
  return JSON.stringify({
    adapter: 'supabase',
    formatVersion: SOURCE_STORAGE_FORMAT_VERSION,
    databaseName,
    url: source.url,
    publishableKey: source.publishableKey,
    tables,
  });
}

export function builtinSourceConfigurationKey(
  source: PreparedBuiltinSource | undefined,
): string | undefined {
  if (!source) {
    return undefined;
  }
  return JSON.stringify({
    identity: sourceStorageIdentity('', source.options),
    id: source.options.id,
    pageSize: source.options.pageSize,
    maxSnapshotPasses: source.options.maxSnapshotPasses,
  });
}

export async function loadBuiltinSource(
  options: NormalizedSupabaseSourceOptions,
): Promise<ReplicaSource> {
  const module = await import('../adapters/supabase/builtin.js');
  return await module.createBuiltinSupabaseSource(options);
}

function compareTables(
  left: NormalizedSupabaseTableOptions,
  right: NormalizedSupabaseTableOptions,
): number {
  const leftKey = `${left.schema}\u0000${left.table}\u0000${left.localName}`;
  const rightKey = `${right.schema}\u0000${right.table}\u0000${right.localName}`;
  return leftKey < rightKey ? -1 : leftKey > rightKey ? 1 : 0;
}

function copySchema(schema: TableSchema): TableSchema {
  return {name: schema.name, primaryKey: [...schema.primaryKey]};
}

function sameStrings(
  left: readonly string[],
  right: readonly string[],
): boolean {
  return (
    left.length === right.length &&
    left.every((value, index) => value === right[index])
  );
}

async function sha256(value: string): Promise<string> {
  if (!globalThis.crypto?.subtle) {
    throw sourceError(
      'SOURCE_FINGERPRINT_UNAVAILABLE',
      'TinyGres cannot bind OPFS storage without Web Crypto SHA-256 support',
    );
  }
  const digest = new Uint8Array(
    await globalThis.crypto.subtle.digest(
      'SHA-256',
      new TextEncoder().encode(value),
    ),
  );
  return [...digest].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function sourceError(code: string, message: string): Error {
  return Object.assign(new Error(message), {code});
}
