import type {SupabaseSourceOptions} from '../../source-options.js';
import {createSupabaseRealtimeTransport} from './native-realtime.js';
import {createSupabaseSource, type SupabaseSource} from './source.js';

/** Creates the dependency-free, anonymous Supabase source used by the default worker. */
export function createBuiltinSupabaseSource(
  options: SupabaseSourceOptions,
): SupabaseSource {
  return createSupabaseSource({
    url: options.url,
    publishableKey: options.publishableKey,
    tables: options.tables.map((table) => ({
      schema: table.schema ?? 'public',
      table: table.table,
      primaryKey: [...table.primaryKey],
      ...(table.localName ? {localName: table.localName} : {}),
      ...(table.columns ? {columns: [...table.columns]} : {}),
    })),
    realtime: createSupabaseRealtimeTransport({
      url: options.url,
      publishableKey: options.publishableKey,
    }),
    ...(options.id ? {id: options.id} : {}),
    ...(options.pageSize === undefined ? {} : {pageSize: options.pageSize}),
    ...(options.maxSnapshotPasses === undefined
      ? {}
      : {maxSnapshotPasses: options.maxSnapshotPasses}),
  });
}
