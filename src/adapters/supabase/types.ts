import type {Row} from '../../protocol.js';
import type {
  NormalizedSupabaseTableOptions,
  SupabaseTableOptions,
} from '../../source-options.js';

/** Table configuration shared by built-in and application-owned sources. */
export type SupabaseTableConfig = SupabaseTableOptions;

export type NormalizedSupabaseTable = NormalizedSupabaseTableOptions;

export type MaybePromise<Value> = Value | PromiseLike<Value>;

export interface SupabaseRealtimeConnectOptions {
  readonly sourceId: string;
  readonly tables: readonly Pick<
    NormalizedSupabaseTable,
    'schema' | 'table' | 'columns'
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

export interface SupabaseRealtimeWebSocket {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  addEventListener(
    type: 'open' | 'error' | 'close' | 'message',
    listener: (event: Event | CloseEvent | MessageEvent<unknown>) => void,
  ): void;
  removeEventListener(
    type: 'open' | 'error' | 'close' | 'message',
    listener: (event: Event | CloseEvent | MessageEvent<unknown>) => void,
  ): void;
}

export interface SupabaseRealtimeTimer {
  setTimeout(callback: () => void, delayMs: number): unknown;
  clearTimeout(handle: unknown): void;
}

export interface CreateSupabaseRealtimeTransportOptions {
  readonly url: string;
  readonly publishableKey: string;
  /** Injectable WebSocket factory for non-browser runtimes and tests. */
  readonly createWebSocket?: (url: string) => SupabaseRealtimeWebSocket;
  /** Injectable timer for deterministic reconnect and heartbeat tests. */
  readonly timer?: SupabaseRealtimeTimer;
  /** Must remain below Supabase's 25-second heartbeat deadline. */
  readonly heartbeatIntervalMs?: number;
  readonly joinTimeoutMs?: number;
  /** Retry schedule; the final delay is reused for subsequent attempts. */
  readonly reconnectDelaysMs?: readonly number[];
}

export interface SupabaseRestSnapshotOptions {
  readonly url: string;
  readonly publishableKey: string;
  readonly pageSize?: number;
  readonly fetch?: typeof globalThis.fetch;
  /** Invoked at most once; one reader never changes authorization principal. */
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
  /**
   * Invoked exactly once when synchronization starts. The sampled token is
   * shared by Realtime and every REST page for the lifetime of this source.
   */
  readonly getAccessToken?: () => MaybePromise<string | null>;
  /** Retry schedule; the final delay is reused for subsequent attempts. */
  readonly retryDelaysMs?: readonly number[];
  /** Injectable abort-aware delay used by deterministic tests. */
  readonly sleep?: (delayMs: number, signal: AbortSignal) => Promise<void>;
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
