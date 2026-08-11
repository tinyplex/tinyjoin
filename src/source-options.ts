export interface SupabaseTableOptions {
  /** Postgres table name exposed through the Supabase Data API. */
  table: string;
  /** Postgres schema. Defaults to `public`. */
  schema?: string;
  /** Columns that uniquely identify a row. */
  primaryKey: string[];
  /** Optional local TinyGres table name. */
  localName?: string;
  /** Optional flat column projection. */
  columns?: string[];
}

export interface SupabaseSourceOptions {
  kind: 'supabase';
  url: string;
  publishableKey: string;
  tables: SupabaseTableOptions[];
  id?: string;
  pageSize?: number;
  maxSnapshotPasses?: number;
}

export interface NormalizedSupabaseTableOptions {
  readonly schema: string;
  readonly table: string;
  readonly primaryKey: string[];
  readonly localName: string;
  readonly columns?: string[];
}

export interface NormalizedSupabaseSourceOptions {
  readonly kind: 'supabase';
  readonly url: string;
  readonly publishableKey: string;
  readonly tables: NormalizedSupabaseTableOptions[];
  readonly id: string;
  readonly pageSize: number;
  readonly maxSnapshotPasses: number;
}

/** Serializable built-in source configurations accepted by the default worker. */
export type SourceOptions = SupabaseSourceOptions;

export function isSourceOptions(value: unknown): value is SourceOptions {
  return isSupabaseSourceOptions(value);
}

export function normalizeSupabaseSourceOptions(
  source: SupabaseSourceOptions,
): NormalizedSupabaseSourceOptions {
  const url = normalizeSupabaseProjectUrl(source.url);
  const publishableKey = normalizeBrowserPublishableKey(
    source.publishableKey,
  );

  const pageSize = source.pageSize ?? 500;
  if (!Number.isSafeInteger(pageSize) || pageSize < 1 || pageSize > 1_000) {
    throw invalidConfig('Snapshot pageSize must be an integer from 1 to 1000');
  }
  const maxSnapshotPasses = source.maxSnapshotPasses ?? 3;
  if (
    !Number.isSafeInteger(maxSnapshotPasses) ||
    maxSnapshotPasses < 1 ||
    maxSnapshotPasses > 10
  ) {
    throw invalidConfig('maxSnapshotPasses must be an integer from 1 to 10');
  }
  if (source.tables.length === 0) {
    throw invalidConfig('At least one Supabase table must be configured');
  }

  const relations = new Set<string>();
  const localNames = new Set<string>();
  const tables = source.tables.map((table) => {
    const schema = table.schema ?? 'public';
    validateIdentifier(schema, 'schema');
    validateIdentifier(table.table, 'table');
    const relation = `${schema}.${table.table}`;
    if (!relations.add(relation)) {
      throw invalidConfig(
        `Supabase relation \`${relation}\` is configured twice`,
      );
    }
    if (table.primaryKey.length === 0) {
      throw invalidConfig(
        `Supabase relation \`${relation}\` must declare a primary key`,
      );
    }
    const primaryKey = uniqueIdentifiers(
      table.primaryKey,
      `primary key for ${relation}`,
    );
    const localName =
      table.localName ?? (schema === 'public' ? table.table : relation);
    if (!LOCAL_NAME.test(localName)) {
      throw invalidConfig(
        `Local table name \`${localName}\` must be an unquoted or schema-qualified SQL identifier`,
      );
    }
    if (!localNames.add(localName)) {
      throw invalidConfig(
        `Local table name \`${localName}\` is configured twice`,
      );
    }

    let columns: string[] | undefined;
    if (table.columns) {
      if (table.columns.length === 0) {
        throw invalidConfig(
          `Column projection for \`${relation}\` cannot be empty`,
        );
      }
      columns = uniqueIdentifiers(table.columns, `columns for ${relation}`);
      for (const column of primaryKey) {
        if (!columns.includes(column)) {
          throw invalidConfig(
            `Column projection for \`${relation}\` must include primary-key column \`${column}\``,
          );
        }
      }
    }
    return {
      schema,
      table: table.table,
      primaryKey,
      localName,
      ...(columns ? {columns} : {}),
    };
  });

  return {
    kind: 'supabase',
    url,
    publishableKey,
    tables,
    id: source.id?.trim() || `supabase:${url}`,
    pageSize,
    maxSnapshotPasses,
  };
}

export function normalizeSupabaseProjectUrl(value: string): string {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw invalidConfig('Supabase URL must be an absolute HTTP(S) URL');
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') {
    throw invalidConfig('Supabase URL must use HTTP or HTTPS');
  }
  if (url.username || url.password) {
    throw invalidConfig('Supabase URL must not contain credentials');
  }
  url.hash = '';
  url.search = '';
  url.pathname = url.pathname.replace(/\/+$/, '');
  return url.href;
}

export function normalizeBrowserPublishableKey(value: string): string {
  const publishableKey = value.trim();
  if (!publishableKey) {
    throw invalidConfig('A Supabase publishable key is required');
  }
  if (
    publishableKey.startsWith('sb_secret_') ||
    isLegacyServiceRoleKey(publishableKey)
  ) {
    throw invalidConfig('Supabase secret keys must never be used in a browser');
  }
  return publishableKey;
}

function isSupabaseSourceOptions(
  value: unknown,
): value is SupabaseSourceOptions {
  if (
    !isExactRecord(value, [
      'kind',
      'url',
      'publishableKey',
      'tables',
      'id',
      'pageSize',
      'maxSnapshotPasses',
    ]) ||
    value.kind !== 'supabase' ||
    !Object.hasOwn(value, 'kind') ||
    !Object.hasOwn(value, 'url') ||
    !Object.hasOwn(value, 'publishableKey') ||
    !Object.hasOwn(value, 'tables') ||
    typeof value.url !== 'string' ||
    typeof value.publishableKey !== 'string' ||
    !isArrayOf(value.tables, isSupabaseTableOptions)
  ) {
    return false;
  }
  return (
    optionalString(value, 'id') &&
    optionalSafeInteger(value, 'pageSize') &&
    optionalSafeInteger(value, 'maxSnapshotPasses')
  );
}

function isSupabaseTableOptions(value: unknown): value is SupabaseTableOptions {
  if (
    !isExactRecord(value, [
      'table',
      'schema',
      'primaryKey',
      'localName',
      'columns',
    ]) ||
    !Object.hasOwn(value, 'table') ||
    !Object.hasOwn(value, 'primaryKey') ||
    typeof value.table !== 'string' ||
    !isStringArray(value.primaryKey)
  ) {
    return false;
  }
  return (
    optionalString(value, 'schema') &&
    optionalString(value, 'localName') &&
    optionalStringArray(value, 'columns')
  );
}

function isExactRecord(
  value: unknown,
  allowedKeys: readonly string[],
): value is Record<string, unknown> {
  return (
    typeof value === 'object' &&
    value !== null &&
    !Array.isArray(value) &&
    Reflect.ownKeys(value).every(
      (key) => typeof key === 'string' && allowedKeys.includes(key),
    )
  );
}

function isStringArray(value: unknown): value is string[] {
  return isArrayOf(
    value,
    (entry): entry is string => typeof entry === 'string',
  );
}

function isArrayOf<Value>(
  value: unknown,
  predicate: (entry: unknown) => entry is Value,
): value is Value[] {
  if (
    !Array.isArray(value) ||
    Reflect.ownKeys(value).length !== value.length + 1
  ) {
    return false;
  }
  for (let index = 0; index < value.length; index += 1) {
    if (!Object.hasOwn(value, index) || !predicate(value[index])) {
      return false;
    }
  }
  return true;
}

function optionalString(value: Record<string, unknown>, key: string): boolean {
  return !Object.hasOwn(value, key) || typeof value[key] === 'string';
}

function optionalStringArray(
  value: Record<string, unknown>,
  key: string,
): boolean {
  return !Object.hasOwn(value, key) || isStringArray(value[key]);
}

function optionalSafeInteger(
  value: Record<string, unknown>,
  key: string,
): boolean {
  return !Object.hasOwn(value, key) || Number.isSafeInteger(value[key]);
}

function uniqueIdentifiers(
  values: readonly string[],
  description: string,
): string[] {
  const seen = new Set<string>();
  return values.map((value) => {
    validateIdentifier(value, description);
    if (!seen.add(value)) {
      throw invalidConfig(
        `Duplicate identifier \`${value}\` in ${description}`,
      );
    }
    return value;
  });
}

function validateIdentifier(value: string, description: string): void {
  if (!IDENTIFIER.test(value)) {
    throw invalidConfig(
      `Invalid ${description} identifier \`${value}\`; quoted identifiers are not supported yet`,
    );
  }
}

function isLegacyServiceRoleKey(value: string): boolean {
  const parts = value.split('.');
  if (parts.length !== 3 || !parts[1]) {
    return false;
  }
  try {
    const encoded = parts[1].replace(/-/g, '+').replace(/_/g, '/');
    const padded = encoded.padEnd(Math.ceil(encoded.length / 4) * 4, '=');
    const payload: unknown = JSON.parse(globalThis.atob(padded));
    return (
      typeof payload === 'object' &&
      payload !== null &&
      'role' in payload &&
      payload.role === 'service_role'
    );
  } catch {
    return false;
  }
}

function invalidConfig(message: string): Error {
  return Object.assign(new Error(message), {code: 'SUPABASE_INVALID_CONFIG'});
}

const IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_$]*$/;
const LOCAL_NAME = /^[A-Za-z_][A-Za-z0-9_$]*(?:\.[A-Za-z_][A-Za-z0-9_$]*)?$/;
