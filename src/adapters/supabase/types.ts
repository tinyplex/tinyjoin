import type {Row} from '../../protocol.js';

export interface SupabaseTableConfig {
  /** Postgres schema exposed through the Supabase Data API. */
  schema: string;
  /** Remote Postgres table name. */
  table: string;
  /** Columns that uniquely identify a row. Required for Realtime deletes. */
  primaryKey: string[];
  /**
   * Optional local TinyGres name. Public tables default to their table name;
   * other schemas default to `schema.table`.
   */
  localName?: string;
  /** Optional flat column projection. Primary-key columns must be included. */
  columns?: string[];
}

export interface NormalizedSupabaseTable {
  readonly schema: string;
  readonly table: string;
  readonly primaryKey: string[];
  readonly localName: string;
  readonly columns?: string[];
}

export type MaybePromise<Value> = Value | PromiseLike<Value>;

export interface SupabaseRealtimeConnectOptions {
  readonly sourceId: string;
  readonly tables: readonly Pick<
    NormalizedSupabaseTable,
    'schema' | 'table'
  >[];
  readonly accessToken: string | null;
  readonly signal: AbortSignal;
}

export interface SupabaseRealtimeObserver {
  payload(payload: unknown): void;
  status(status: string, error?: unknown): void;
}

export interface SupabaseRealtimeConnection {
  close(): Promise<void>;
}

/**
 * Narrow injection boundary around Supabase Realtime. Production callers can
 * use the official supabase-js wrapper while tests can provide a tiny fake.
 */
export interface SupabaseRealtimeTransport {
  connect(
    options: SupabaseRealtimeConnectOptions,
    observer: SupabaseRealtimeObserver,
  ): Promise<SupabaseRealtimeConnection>;
}

export interface SupabaseRestSnapshotOptions {
  readonly url: string;
  readonly publishableKey: string;
  readonly pageSize?: number;
  readonly fetch?: typeof globalThis.fetch;
  readonly getAccessToken?: () => MaybePromise<string | null>;
}

export interface SupabaseSnapshotPage {
  readonly page: number;
  readonly rows: Row[];
  readonly done: boolean;
}

export interface CreateSupabaseSourceOptions
  extends SupabaseRestSnapshotOptions {
  readonly tables: SupabaseTableConfig[];
  readonly realtime: SupabaseRealtimeTransport;
  readonly id?: string;
  /** Number of snapshot attempts if changes arrive during a snapshot. */
  readonly maxSnapshotPasses?: number;
  /** Injectable clock for deterministic tests. */
  readonly now?: () => string;
}

export interface SupabaseRealtimePayload {
  readonly schema?: unknown;
  readonly table?: unknown;
  readonly eventType?: unknown;
  readonly type?: unknown;
  readonly new?: unknown;
  readonly old?: unknown;
  readonly record?: unknown;
  readonly old_record?: unknown;
  readonly commit_timestamp?: unknown;
  readonly errors?: unknown;
}

export class SupabaseSourceError extends Error {
  readonly code: string;
  readonly retryable: boolean;

  constructor(code: string, message: string, retryable = false) {
    super(message);
    this.name = 'SupabaseSourceError';
    this.code = code;
    this.retryable = retryable;
  }
}
