import type {TableSchema} from '../../protocol.js';
import type {
  CreateSupabaseSourceOptions,
  NormalizedSupabaseTable,
  SupabaseTableConfig,
} from './types.js';
import {SupabaseSourceError} from './types.js';

const IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_$]*$/;
const LOCAL_NAME = /^[A-Za-z_][A-Za-z0-9_$]*(?:\.[A-Za-z_][A-Za-z0-9_$]*)?$/;

export interface NormalizedSupabaseSourceConfig {
  readonly id: string;
  readonly url: string;
  readonly publishableKey: string;
  readonly pageSize: number;
  readonly maxSnapshotPasses: number;
  readonly tables: NormalizedSupabaseTable[];
}

export function normalizeSupabaseConfig(
  options: CreateSupabaseSourceOptions,
): NormalizedSupabaseSourceConfig {
  let url: URL;
  try {
    url = new URL(options.url);
  } catch {
    throw configError('Supabase URL must be an absolute HTTP(S) URL');
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') {
    throw configError('Supabase URL must use HTTP or HTTPS');
  }
  url.hash = '';
  url.search = '';
  url.pathname = url.pathname.replace(/\/+$/, '');

  if (!options.publishableKey.trim()) {
    throw configError('A Supabase publishable key is required');
  }
  if (options.publishableKey.startsWith('sb_secret_')) {
    throw configError('Supabase secret keys must never be used in a browser');
  }

  const pageSize = options.pageSize ?? 500;
  if (!Number.isSafeInteger(pageSize) || pageSize < 1 || pageSize > 1_000) {
    throw configError('Snapshot pageSize must be an integer from 1 to 1000');
  }

  const maxSnapshotPasses = options.maxSnapshotPasses ?? 3;
  if (
    !Number.isSafeInteger(maxSnapshotPasses) ||
    maxSnapshotPasses < 1 ||
    maxSnapshotPasses > 10
  ) {
    throw configError('maxSnapshotPasses must be an integer from 1 to 10');
  }

  if (options.tables.length === 0) {
    throw configError('At least one Supabase table must be configured');
  }

  const relations = new Set<string>();
  const localNames = new Set<string>();
  const tables = options.tables.map((table) => {
    const normalized = normalizeTable(table);
    const relation = relationKey(normalized.schema, normalized.table);
    if (!relations.add(relation)) {
      throw configError(`Supabase relation \`${relation}\` is configured twice`);
    }
    if (!localNames.add(normalized.localName)) {
      throw configError(
        `Local table name \`${normalized.localName}\` is configured twice`,
      );
    }
    return normalized;
  });

  return {
    id: options.id?.trim() || `supabase:${url.href}`,
    url: url.href,
    publishableKey: options.publishableKey,
    pageSize,
    maxSnapshotPasses,
    tables,
  };
}

export function toLocalSchema(table: NormalizedSupabaseTable): TableSchema {
  return {
    name: table.localName,
    primaryKey: [...table.primaryKey],
  };
}

export function relationKey(schema: string, table: string): string {
  return `${schema}.${table}`;
}

function normalizeTable(table: SupabaseTableConfig): NormalizedSupabaseTable {
  validateIdentifier(table.schema, 'schema');
  validateIdentifier(table.table, 'table');

  if (table.primaryKey.length === 0) {
    const relation = relationKey(table.schema, table.table);
    throw configError(
      `Supabase relation \`${relation}\` must declare a primary key`,
    );
  }
  const primaryKey = uniqueIdentifiers(
    table.primaryKey,
    `primary key for ${relationKey(table.schema, table.table)}`,
  );

  const localName =
    table.localName ??
    (table.schema === 'public'
      ? table.table
      : relationKey(table.schema, table.table));
  if (!LOCAL_NAME.test(localName)) {
    throw configError(
      `Local table name \`${localName}\` must be an unquoted or schema-qualified SQL identifier`,
    );
  }

  let columns: string[] | undefined;
  if (table.columns) {
    const relation = relationKey(table.schema, table.table);
    if (table.columns.length === 0) {
      throw configError(
        `Column projection for \`${relation}\` cannot be empty`,
      );
    }
    columns = uniqueIdentifiers(
      table.columns,
      `columns for ${relationKey(table.schema, table.table)}`,
    );
    for (const primaryKeyColumn of primaryKey) {
      if (!columns.includes(primaryKeyColumn)) {
        throw configError(
          `Column projection for \`${relation}\` must include primary-key column \`${primaryKeyColumn}\``,
        );
      }
    }
  }

  return {
    schema: table.schema,
    table: table.table,
    primaryKey,
    localName,
    ...(columns ? {columns} : {}),
  };
}

function uniqueIdentifiers(values: string[], description: string): string[] {
  const seen = new Set<string>();
  return values.map((value) => {
    validateIdentifier(value, description);
    if (!seen.add(value)) {
      throw configError(`Duplicate identifier \`${value}\` in ${description}`);
    }
    return value;
  });
}

function validateIdentifier(value: string, description: string): void {
  if (!IDENTIFIER.test(value)) {
    throw configError(
      `Invalid ${description} identifier \`${value}\`; quoted identifiers are not supported yet`,
    );
  }
}

function configError(message: string): SupabaseSourceError {
  return new SupabaseSourceError('SUPABASE_INVALID_CONFIG', message);
}
