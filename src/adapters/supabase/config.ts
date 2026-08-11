import type {TableSchema} from '../../protocol.js';
import {
  normalizeBrowserPublishableKey as normalizePublishableKey,
  normalizeSupabaseProjectUrl as normalizeProjectUrl,
  normalizeSupabaseSourceOptions,
} from '../../source-options.js';
import type {
  CreateSupabaseSourceOptions,
  NormalizedSupabaseTable,
} from './types.js';
import {SupabaseSourceError} from './types.js';

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
  try {
    const normalized = normalizeSupabaseSourceOptions({
      kind: 'supabase',
      url: options.url,
      publishableKey: options.publishableKey,
      tables: options.tables,
      ...(options.id === undefined ? {} : {id: options.id}),
      ...(options.pageSize === undefined ? {} : {pageSize: options.pageSize}),
      ...(options.maxSnapshotPasses === undefined
        ? {}
        : {maxSnapshotPasses: options.maxSnapshotPasses}),
    });
    return {
      id: normalized.id,
      url: normalized.url,
      publishableKey: normalized.publishableKey,
      pageSize: normalized.pageSize,
      maxSnapshotPasses: normalized.maxSnapshotPasses,
      tables: normalized.tables,
    };
  } catch (error) {
    throw asConfigError(error);
  }
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

export function normalizeBrowserPublishableKey(value: string): string {
  try {
    return normalizePublishableKey(value);
  } catch (error) {
    throw asConfigError(error);
  }
}

export function normalizeSupabaseProjectUrl(value: string): string {
  try {
    return normalizeProjectUrl(value);
  } catch (error) {
    throw asConfigError(error);
  }
}

function asConfigError(error: unknown): SupabaseSourceError {
  return error instanceof SupabaseSourceError
    ? error
    : new SupabaseSourceError(
        'SUPABASE_INVALID_CONFIG',
        error instanceof Error ? error.message : 'Invalid Supabase configuration',
      );
}
