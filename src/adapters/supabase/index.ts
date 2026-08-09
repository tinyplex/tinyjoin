export {
  createSupabaseJsRealtimeTransport,
  normalizeSupabaseChange,
  payloadRelation,
} from './realtime.js';
export {SupabaseRestSnapshotReader} from './rest.js';
export {
  createSupabaseSource,
  SupabaseSource,
  SUPABASE_SOURCE_CAPABILITIES,
} from './source.js';
export type {
  CreateSupabaseSourceOptions,
  NormalizedSupabaseTable,
  SupabaseRealtimeConnectOptions,
  SupabaseRealtimeConnection,
  SupabaseRealtimeObserver,
  SupabaseRealtimePayload,
  SupabaseRealtimeTransport,
  SupabaseRestSnapshotOptions,
  SupabaseSnapshotPage,
  SupabaseTableConfig,
} from './types.js';
export {SupabaseSourceError} from './types.js';
