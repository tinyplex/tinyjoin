import {isRecord} from '../../protocol.js';
import {
  normalizeBrowserPublishableKey,
  normalizeSupabaseProjectUrl,
} from './config.js';
import type {
  CreateSupabaseRealtimeTransportOptions,
  SupabaseRealtimeConnectOptions,
  SupabaseRealtimeConnection,
  SupabaseRealtimeObserver,
  SupabaseRealtimeTimer,
  SupabaseRealtimeTransport,
  SupabaseRealtimeWebSocket,
} from './types.js';
import {SupabaseSourceError} from './types.js';

const DEFAULT_HEARTBEAT_INTERVAL_MS = 20_000;
const DEFAULT_JOIN_TIMEOUT_MS = 10_000;
const DEFAULT_RECONNECT_DELAYS_MS = [1_000, 2_000, 5_000, 10_000] as const;
const MAX_HEARTBEAT_INTERVAL_MS = 24_999;
const MAX_DELAY_MS = 60_000;

let topicSequence = 0;

export function createSupabaseRealtimeTransport(
  options: CreateSupabaseRealtimeTransportOptions,
): SupabaseRealtimeTransport {
  const endpoint = realtimeEndpoint(options.url, options.publishableKey);
  const heartbeatIntervalMs = boundedInteger(
    options.heartbeatIntervalMs ?? DEFAULT_HEARTBEAT_INTERVAL_MS,
    'heartbeatIntervalMs',
    1,
    MAX_HEARTBEAT_INTERVAL_MS,
  );
  const joinTimeoutMs = boundedInteger(
    options.joinTimeoutMs ?? DEFAULT_JOIN_TIMEOUT_MS,
    'joinTimeoutMs',
    1,
    MAX_DELAY_MS,
  );
  const reconnectDelaysMs = normalizeDelays(
    options.reconnectDelaysMs ?? DEFAULT_RECONNECT_DELAYS_MS,
  );
  const timer = options.timer ?? defaultTimer;
  const createWebSocket = options.createWebSocket ?? defaultWebSocketFactory;

  return {
    connect(connectOptions, observer) {
      const session = new NativeRealtimeSession({
        endpoint,
        heartbeatIntervalMs,
        joinTimeoutMs,
        reconnectDelaysMs,
        timer,
        createWebSocket,
        connectOptions,
        observer,
      });
      return session.connect();
    },
  };
}

interface NativeRealtimeSessionOptions {
  readonly endpoint: string;
  readonly heartbeatIntervalMs: number;
  readonly joinTimeoutMs: number;
  readonly reconnectDelaysMs: readonly number[];
  readonly timer: SupabaseRealtimeTimer;
  readonly createWebSocket: (url: string) => SupabaseRealtimeWebSocket;
  readonly connectOptions: SupabaseRealtimeConnectOptions;
  readonly observer: SupabaseRealtimeObserver;
}

interface SocketListeners {
  readonly open: (event: Event | CloseEvent | MessageEvent<unknown>) => void;
  readonly error: (event: Event | CloseEvent | MessageEvent<unknown>) => void;
  readonly close: (event: Event | CloseEvent | MessageEvent<unknown>) => void;
  readonly message: (event: Event | CloseEvent | MessageEvent<unknown>) => void;
}

class NativeRealtimeSession {
  readonly #options: NativeRealtimeSessionOptions;
  readonly #topic: string;
  readonly #ready: Promise<SupabaseRealtimeConnection>;
  #resolveReady:
    | ((connection: SupabaseRealtimeConnection) => void)
    | undefined;
  #rejectReady: ((error: unknown) => void) | undefined;
  #socket: SupabaseRealtimeWebSocket | undefined;
  #listeners: SocketListeners | undefined;
  #joinTimeout: unknown;
  #heartbeatTimer: unknown;
  #reconnectTimer: unknown;
  #joinRef: string | undefined;
  #heartbeatRef: string | undefined;
  #subscriptionIds = new Set<number>();
  #reference = 0;
  #reconnectAttempt = 0;
  #established = false;
  #readySettled = false;
  #closed = false;

  constructor(options: NativeRealtimeSessionOptions) {
    this.#options = options;
    this.#topic = `realtime:tinygres:${sanitizeTopic(options.connectOptions.sourceId)}:${++topicSequence}`;
    this.#ready = new Promise((resolve, reject) => {
      this.#resolveReady = resolve;
      this.#rejectReady = reject;
    });
  }

  connect(): Promise<SupabaseRealtimeConnection> {
    const {signal} = this.#options.connectOptions;
    if (signal.aborted) {
      this.#rejectInitial(abortedError());
      return this.#ready;
    }
    signal.addEventListener('abort', this.#onAbort, {once: true});
    this.#openSocket();
    return this.#ready;
  }

  async close(): Promise<void> {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#options.connectOptions.signal.removeEventListener(
      'abort',
      this.#onAbort,
    );
    this.#clearReconnect();
    this.#cleanupSocket(true);
    if (!this.#readySettled) {
      this.#rejectInitial(abortedError());
    }
  }

  readonly #onAbort = (): void => {
    void this.close();
  };

  #openSocket(): void {
    if (this.#closed) {
      return;
    }
    this.#cleanupSocket(true);
    let socket: SupabaseRealtimeWebSocket;
    try {
      socket = this.#options.createWebSocket(this.#options.endpoint);
    } catch (error) {
      this.#failAttempt(
        error instanceof SupabaseSourceError &&
          error.code === 'SUPABASE_WEBSOCKET_UNAVAILABLE'
          ? error
          : new SupabaseSourceError(
              'SUPABASE_REALTIME_CONNECT_FAILED',
              'Could not open the Supabase Realtime WebSocket',
              true,
            ),
      );
      return;
    }

    const listeners: SocketListeners = {
      open: () => {
        if (this.#socket === socket) {
          this.#sendJoin();
        }
      },
      error: () => {
        if (this.#socket === socket) {
          this.#failAttempt(disconnectedError('WebSocket error'));
        }
      },
      close: () => {
        if (this.#socket === socket) {
          this.#failAttempt(disconnectedError('WebSocket closed'));
        }
      },
      message: (event) => {
        if (this.#socket === socket && 'data' in event) {
          this.#receiveMessage(event.data);
        }
      },
    };
    this.#socket = socket;
    this.#listeners = listeners;
    for (const type of ['open', 'error', 'close', 'message'] as const) {
      socket.addEventListener(type, listeners[type]);
    }
    this.#joinTimeout = this.#options.timer.setTimeout(() => {
      this.#joinTimeout = undefined;
      this.#failAttempt(
        new SupabaseSourceError(
          'SUPABASE_REALTIME_CONNECT_FAILED',
          'Timed out joining Supabase Realtime',
          true,
        ),
      );
    }, this.#options.joinTimeoutMs);
  }

  #sendJoin(): void {
    const ref = this.#nextRef();
    this.#joinRef = ref;
    const postgresChanges = this.#options.connectOptions.tables.map((table) => ({
      event: '*',
      schema: table.schema,
      table: table.table,
      ...(table.columns ? {select: [...table.columns]} : {}),
    }));
    this.#send({
      topic: this.#topic,
      event: 'phx_join',
      payload: {
        config: {
          broadcast: {ack: false, self: false},
          presence: {enabled: false},
          postgres_changes: postgresChanges,
          private: false,
        },
        ...(this.#options.connectOptions.accessToken
          ? {access_token: this.#options.connectOptions.accessToken}
          : {}),
      },
      ref,
      join_ref: ref,
    });
  }

  #receiveMessage(data: unknown): void {
    if (typeof data !== 'string') {
      this.#failAttempt(
        protocolError('received a non-text frame', this.#established),
      );
      return;
    }
    let value: unknown;
    try {
      value = JSON.parse(data);
    } catch {
      this.#failAttempt(
        protocolError('received malformed JSON', this.#established),
      );
      return;
    }
    if (!isPhoenixMessage(value)) {
      this.#failAttempt(
        protocolError('received an invalid message envelope', this.#established),
      );
      return;
    }

    if (
      value.event === 'phx_reply' &&
      value.topic === 'phoenix' &&
      value.ref === this.#heartbeatRef
    ) {
      this.#heartbeatRef = undefined;
      return;
    }

    if (
      value.event === 'phx_reply' &&
      value.topic === this.#topic &&
      value.ref === this.#joinRef
    ) {
      this.#receiveJoinReply(value.payload);
      return;
    }

    if (value.topic !== this.#topic) {
      return;
    }
    if (value.event === 'postgres_changes') {
      this.#receivePostgresChanges(value.payload);
      return;
    }
    if (value.event === 'phx_error' || value.event === 'phx_close') {
      this.#failAttempt(disconnectedError(`channel entered ${value.event}`));
      return;
    }
    if (
      value.event === 'system' &&
      isRecord(value.payload) &&
      (value.payload.status === 'error' || value.payload.status === 'timeout')
    ) {
      this.#failAttempt(
        new SupabaseSourceError(
          'SUPABASE_REALTIME_SYSTEM_ERROR',
          'Supabase Realtime reported a channel error',
          true,
        ),
      );
    }
  }

  #receiveJoinReply(payload: unknown): void {
    if (!isRecord(payload) || typeof payload.status !== 'string') {
      this.#failAttempt(protocolError('returned an invalid join reply'));
      return;
    }
    if (payload.status !== 'ok') {
      this.#failAttempt(
        new SupabaseSourceError(
          'SUPABASE_REALTIME_JOIN_REJECTED',
          'Supabase Realtime rejected the channel join',
        ),
      );
      return;
    }
    try {
      this.#subscriptionIds = validateSubscriptions(
        payload.response,
        this.#options.connectOptions.tables,
      );
    } catch (error) {
      this.#failAttempt(errorAsSourceError(error));
      return;
    }

    this.#clearJoinTimeout();
    this.#heartbeatRef = undefined;
    this.#reconnectAttempt = 0;
    const firstJoin = !this.#established;
    this.#established = true;
    this.#scheduleHeartbeat();
    this.#options.observer.status('SUBSCRIBED');
    if (firstJoin && !this.#readySettled) {
      this.#readySettled = true;
      this.#resolveReady?.({close: () => this.close()});
      this.#resolveReady = undefined;
      this.#rejectReady = undefined;
    }
  }

  #receivePostgresChanges(payload: unknown): void {
    if (
      !isRecord(payload) ||
      !Array.isArray(payload.ids) ||
      payload.ids.length === 0 ||
      !payload.ids.every(
        (id) => Number.isSafeInteger(id) && this.#subscriptionIds.has(id),
      ) ||
      !isRecord(payload.data)
    ) {
      this.#failAttempt(
        protocolError('received a change for an unknown subscription', true),
      );
      return;
    }
    this.#options.observer.payload(payload.data);
  }

  #scheduleHeartbeat(): void {
    this.#clearHeartbeat();
    this.#heartbeatTimer = this.#options.timer.setTimeout(() => {
      this.#heartbeatTimer = undefined;
      if (this.#heartbeatRef !== undefined) {
        this.#failAttempt(
          new SupabaseSourceError(
            'SUPABASE_REALTIME_HEARTBEAT_TIMEOUT',
            'Supabase Realtime did not acknowledge a heartbeat',
            true,
          ),
        );
        return;
      }
      const ref = this.#nextRef();
      this.#heartbeatRef = ref;
      if (!this.#send({
        topic: 'phoenix',
        event: 'heartbeat',
        payload: {},
        ref,
        join_ref: null,
      })) {
        return;
      }
      this.#scheduleHeartbeat();
    }, this.#options.heartbeatIntervalMs);
  }

  #send(message: PhoenixOutgoingMessage): boolean {
    const socket = this.#socket;
    if (!socket) {
      this.#failAttempt(disconnectedError('WebSocket is not open'));
      return false;
    }
    try {
      socket.send(JSON.stringify(message));
      return true;
    } catch {
      this.#failAttempt(disconnectedError('could not send a WebSocket frame'));
      return false;
    }
  }

  #failAttempt(error: SupabaseSourceError): void {
    if (this.#closed) {
      return;
    }
    this.#cleanupSocket(true);
    if (!this.#established) {
      this.#closed = true;
      this.#options.connectOptions.signal.removeEventListener(
        'abort',
        this.#onAbort,
      );
      this.#rejectInitial(error);
      return;
    }

    this.#options.observer.status('CHANNEL_ERROR', error);
    if (!error.retryable) {
      return;
    }
    const delay = delayForAttempt(
      this.#options.reconnectDelaysMs,
      this.#reconnectAttempt++,
    );
    this.#reconnectTimer = this.#options.timer.setTimeout(() => {
      this.#reconnectTimer = undefined;
      this.#openSocket();
    }, delay);
  }

  #rejectInitial(error: unknown): void {
    if (this.#readySettled) {
      return;
    }
    this.#readySettled = true;
    this.#rejectReady?.(error);
    this.#resolveReady = undefined;
    this.#rejectReady = undefined;
  }

  #cleanupSocket(close: boolean): void {
    this.#clearJoinTimeout();
    this.#clearHeartbeat();
    this.#heartbeatRef = undefined;
    this.#joinRef = undefined;
    this.#subscriptionIds.clear();
    const socket = this.#socket;
    const listeners = this.#listeners;
    this.#socket = undefined;
    this.#listeners = undefined;
    if (!socket) {
      return;
    }
    if (listeners) {
      for (const type of ['open', 'error', 'close', 'message'] as const) {
        socket.removeEventListener(type, listeners[type]);
      }
    }
    if (close) {
      try {
        socket.close(1000, 'TinyGres reconnecting');
      } catch {
        // Cleanup is best effort and must not expose connection details.
      }
    }
  }

  #clearJoinTimeout(): void {
    if (this.#joinTimeout !== undefined) {
      this.#options.timer.clearTimeout(this.#joinTimeout);
      this.#joinTimeout = undefined;
    }
  }

  #clearHeartbeat(): void {
    if (this.#heartbeatTimer !== undefined) {
      this.#options.timer.clearTimeout(this.#heartbeatTimer);
      this.#heartbeatTimer = undefined;
    }
  }

  #clearReconnect(): void {
    if (this.#reconnectTimer !== undefined) {
      this.#options.timer.clearTimeout(this.#reconnectTimer);
      this.#reconnectTimer = undefined;
    }
  }

  #nextRef(): string {
    this.#reference += 1;
    return String(this.#reference);
  }
}

interface PhoenixOutgoingMessage {
  readonly topic: string;
  readonly event: string;
  readonly payload: unknown;
  readonly ref: string;
  readonly join_ref: string | null;
}

interface PhoenixIncomingMessage {
  readonly topic: string;
  readonly event: string;
  readonly payload: unknown;
  readonly ref?: string | null;
}

function isPhoenixMessage(value: unknown): value is PhoenixIncomingMessage {
  return (
    isRecord(value) &&
    typeof value.topic === 'string' &&
    typeof value.event === 'string' &&
    'payload' in value &&
    (value.ref === undefined || value.ref === null || typeof value.ref === 'string')
  );
}

function validateSubscriptions(
  response: unknown,
  expected: SupabaseRealtimeConnectOptions['tables'],
): Set<number> {
  if (!isRecord(response) || !Array.isArray(response.postgres_changes)) {
    throw protocolError('returned no Postgres change subscriptions');
  }
  if (response.postgres_changes.length !== expected.length) {
    throw protocolError('returned an unexpected number of subscriptions');
  }
  const expectedRelations = new Set(
    expected.map((table) => relation(table.schema, table.table)),
  );
  const seenRelations = new Set<string>();
  const ids = new Set<number>();
  for (const subscription of response.postgres_changes) {
    if (
      !isRecord(subscription) ||
      !Number.isSafeInteger(subscription.id) ||
      subscription.event !== '*' ||
      typeof subscription.schema !== 'string' ||
      typeof subscription.table !== 'string'
    ) {
      throw protocolError('returned an invalid Postgres subscription');
    }
    const configuredRelation = relation(subscription.schema, subscription.table);
    if (
      !expectedRelations.has(configuredRelation) ||
      !seenRelations.add(configuredRelation) ||
      !ids.add(Number(subscription.id))
    ) {
      throw protocolError('returned mismatched Postgres subscriptions');
    }
  }
  return ids;
}

function realtimeEndpoint(projectUrl: string, publishableKey: string): string {
  const endpoint = new URL(normalizeSupabaseProjectUrl(projectUrl));
  const browserKey = normalizeBrowserPublishableKey(publishableKey);
  endpoint.protocol = endpoint.protocol === 'https:' ? 'wss:' : 'ws:';
  endpoint.hash = '';
  endpoint.pathname = `${endpoint.pathname.replace(/\/+$/, '')}/realtime/v1/websocket`;
  endpoint.search = '';
  endpoint.searchParams.set('apikey', browserKey);
  endpoint.searchParams.set('vsn', '1.0.0');
  return endpoint.href;
}

function boundedInteger(
  value: number,
  name: string,
  minimum: number,
  maximum: number,
): number {
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new SupabaseSourceError(
      'SUPABASE_INVALID_CONFIG',
      `${name} must be an integer from ${minimum} to ${maximum}`,
    );
  }
  return value;
}

function normalizeDelays(delays: readonly number[]): readonly number[] {
  if (delays.length === 0) {
    throw new SupabaseSourceError(
      'SUPABASE_INVALID_CONFIG',
      'reconnectDelaysMs must contain at least one delay',
    );
  }
  return delays.map((delay) =>
    boundedInteger(delay, 'Realtime reconnect delay', 0, MAX_DELAY_MS),
  );
}

function delayForAttempt(delays: readonly number[], attempt: number): number {
  return delays[Math.min(attempt, delays.length - 1)]!;
}

function relation(schema: string, table: string): string {
  return `${schema}\u0000${table}`;
}

function sanitizeTopic(value: string): string {
  return value.replace(/[^A-Za-z0-9:_-]/g, '_').slice(0, 80);
}

function protocolError(detail: string, retryable = false): SupabaseSourceError {
  return new SupabaseSourceError(
    'SUPABASE_REALTIME_PROTOCOL_ERROR',
    `Supabase Realtime ${detail}`,
    retryable,
  );
}

function disconnectedError(detail: string): SupabaseSourceError {
  return new SupabaseSourceError(
    'SUPABASE_REALTIME_DISCONNECTED',
    `Supabase Realtime disconnected: ${detail}`,
    true,
  );
}

function abortedError(): SupabaseSourceError {
  return new SupabaseSourceError(
    'SUPABASE_SOURCE_ABORTED',
    'Supabase Realtime connection was aborted',
  );
}

function errorAsSourceError(error: unknown): SupabaseSourceError {
  return error instanceof SupabaseSourceError
    ? error
    : protocolError('returned an invalid join reply');
}

const defaultTimer: SupabaseRealtimeTimer = {
  setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
  clearTimeout: (handle) =>
    globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
};

function defaultWebSocketFactory(url: string): SupabaseRealtimeWebSocket {
  if (typeof globalThis.WebSocket !== 'function') {
    throw new SupabaseSourceError(
      'SUPABASE_WEBSOCKET_UNAVAILABLE',
      'Supabase Realtime requires the WebSocket API',
    );
  }
  return new globalThis.WebSocket(url) as SupabaseRealtimeWebSocket;
}
