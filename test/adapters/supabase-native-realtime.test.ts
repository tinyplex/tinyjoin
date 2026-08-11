import {describe, expect, it, vi} from 'vitest';

import {createSupabaseRealtimeTransport} from '../../src/adapters/supabase/native-realtime.ts';
import type {
  SupabaseRealtimeObserver,
  SupabaseRealtimeTimer,
  SupabaseRealtimeWebSocket,
} from '../../src/adapters/supabase/types.ts';

const publishableKey = 'sb_publishable_safe_test_key';

describe('createSupabaseRealtimeTransport', () => {
  it('validates browser-safe endpoints, keys, and timing bounds', () => {
    expect(() =>
      createSupabaseRealtimeTransport({
        url: 'file:///database',
        publishableKey,
      }),
    ).toThrow(/HTTP or HTTPS/);
    expect(() =>
      createSupabaseRealtimeTransport({
        url: 'https://user:password@project.supabase.co',
        publishableKey,
      }),
    ).toThrow(/must not contain credentials/);
    expect(() =>
      createSupabaseRealtimeTransport({
        url: 'https://project.supabase.co',
        publishableKey: 'sb_secret_never',
      }),
    ).toThrow(/secret keys/);
    expect(() =>
      createSupabaseRealtimeTransport({
        url: 'https://project.supabase.co',
        publishableKey,
        heartbeatIntervalMs: 25_000,
      }),
    ).toThrow(/heartbeatIntervalMs/);
    expect(() =>
      createSupabaseRealtimeTransport({
        url: 'https://project.supabase.co',
        publishableKey,
        reconnectDelaysMs: [],
      }),
    ).toThrow(/at least one/);
  });

  it('joins explicit subscriptions and unwraps validated change data', async () => {
    const harness = createHarness();
    const {promise, observer} = harness.connect({accessToken: 'user-jwt'});
    const socket = harness.sockets[0]!;
    expect(new URL(harness.urls[0]!)).toMatchObject({
      protocol: 'wss:',
      pathname: '/realtime/v1/websocket',
    });
    expect(new URL(harness.urls[0]!).searchParams.get('apikey')).toBe(
      publishableKey,
    );
    expect(new URL(harness.urls[0]!).searchParams.get('vsn')).toBe('1.0.0');

    socket.open();
    const join = socket.lastSent();
    expect(join).toMatchObject({
      event: 'phx_join',
      payload: {
        access_token: 'user-jwt',
        config: {
          broadcast: {ack: false, self: false},
          presence: {enabled: false},
          private: false,
          postgres_changes: [
            {
              event: '*',
              schema: 'public',
              table: 'posts',
              select: ['id', 'title'],
            },
            {event: '*', schema: 'private', table: 'memberships'},
          ],
        },
      },
    });
    socket.joinReply(join, [
      {id: 7, event: '*', schema: 'private', table: 'memberships'},
      {id: 4, event: '*', schema: 'public', table: 'posts'},
    ]);
    const connection = await promise;
    expect(observer.status).toHaveBeenCalledWith('SUBSCRIBED');

    const data = {
      schema: 'public',
      table: 'posts',
      type: 'INSERT',
      record: {id: 1, title: 'live'},
    };
    socket.message({
      topic: join.topic,
      event: 'postgres_changes',
      payload: {ids: [4], data},
      ref: null,
      join_ref: join.ref,
    });
    expect(observer.payload).toHaveBeenCalledWith(data);

    await connection.close();
    expect(socket.closeCalls).toBe(1);
    expect(harness.timer.pending()).toBe(0);
  });

  it('rejects mismatched joins and never leaks the key through failures', async () => {
    const harness = createHarness();
    const {promise} = harness.connect();
    const socket = harness.sockets[0]!;
    socket.open();
    const join = socket.lastSent();
    socket.joinReply(join, [
      {id: 1, event: '*', schema: 'public', table: 'wrong_table'},
      {id: 2, event: '*', schema: 'private', table: 'memberships'},
    ]);

    await expect(promise).rejects.toMatchObject({
      code: 'SUPABASE_REALTIME_PROTOCOL_ERROR',
      retryable: false,
    });
    await expect(promise).rejects.not.toMatchObject({
      message: expect.stringContaining(publishableKey),
    });
    expect(socket.closeCalls).toBe(1);

    const throwing = createSupabaseRealtimeTransport({
      url: 'https://project.supabase.co',
      publishableKey,
      createWebSocket: (url) => {
        throw new Error(`failed ${url}`);
      },
    });
    const failed = throwing.connect(connectOptions(), observer());
    await expect(failed).rejects.toMatchObject({
      code: 'SUPABASE_REALTIME_CONNECT_FAILED',
      message: expect.not.stringContaining(publishableKey),
      retryable: true,
    });
  });

  it('detects a missing heartbeat acknowledgement and reconnects with backoff', async () => {
    const harness = createHarness({
      heartbeatIntervalMs: 20,
      reconnectDelaysMs: [5, 10],
    });
    const {promise, observer} = harness.connect();
    const first = harness.sockets[0]!;
    first.open();
    const firstJoin = first.lastSent();
    first.joinReply(firstJoin, expectedSubscriptions());
    const connection = await promise;

    harness.timer.advance(20);
    expect(first.lastSent()).toMatchObject({
      topic: 'phoenix',
      event: 'heartbeat',
      payload: {},
    });
    harness.timer.advance(20);
    expect(observer.status).toHaveBeenLastCalledWith(
      'CHANNEL_ERROR',
      expect.objectContaining({
        code: 'SUPABASE_REALTIME_HEARTBEAT_TIMEOUT',
        retryable: true,
      }),
    );
    expect(harness.sockets).toHaveLength(1);

    harness.timer.advance(5);
    expect(harness.sockets).toHaveLength(2);
    const second = harness.sockets[1]!;
    second.open();
    const secondJoin = second.lastSent();
    second.joinReply(secondJoin, expectedSubscriptions());
    expect(observer.status.mock.calls.filter(([status]) => status === 'SUBSCRIBED'))
      .toHaveLength(2);

    await connection.close();
    expect(harness.timer.pending()).toBe(0);
  });

  it('caps repeated reconnect failures at the final configured delay', async () => {
    const harness = createHarness({reconnectDelaysMs: [3, 7]});
    const connected = harness.connect();
    const first = harness.sockets[0]!;
    first.open();
    const join = first.lastSent();
    first.joinReply(join, expectedSubscriptions());
    const connection = await connected.promise;

    first.serverClose();
    expect(harness.timer.nextDelay()).toBe(3);
    harness.timer.advance(3);
    const second = harness.sockets[1]!;
    second.serverClose();
    expect(harness.timer.nextDelay()).toBe(7);
    harness.timer.advance(7);
    const third = harness.sockets[2]!;
    third.serverClose();
    expect(harness.timer.nextDelay()).toBe(7);

    await connection.close();
    expect(harness.timer.pending()).toBe(0);
  });

  it('accepts heartbeat replies and reconnects on unknown subscription ids or system errors', async () => {
    const harness = createHarness({
      heartbeatIntervalMs: 20,
      reconnectDelaysMs: [1],
    });
    const {promise, observer} = harness.connect();
    const first = harness.sockets[0]!;
    first.open();
    const join = first.lastSent();
    first.joinReply(join, expectedSubscriptions());
    const connection = await promise;

    harness.timer.advance(20);
    const heartbeat = first.lastSent();
    first.message({
      topic: 'phoenix',
      event: 'phx_reply',
      payload: {status: 'ok', response: {}},
      ref: heartbeat.ref,
      join_ref: null,
    });
    harness.timer.advance(20);
    expect(first.lastSent().event).toBe('heartbeat');

    first.message({
      topic: join.topic,
      event: 'postgres_changes',
      payload: {ids: [999], data: {schema: 'public', table: 'posts'}},
      ref: null,
      join_ref: join.ref,
    });
    expect(observer.status).toHaveBeenLastCalledWith(
      'CHANNEL_ERROR',
      expect.objectContaining({
        code: 'SUPABASE_REALTIME_PROTOCOL_ERROR',
        retryable: true,
      }),
    );
    harness.timer.advance(1);
    const second = harness.sockets[1]!;
    second.open();
    const secondJoin = second.lastSent();
    second.joinReply(secondJoin, expectedSubscriptions());
    second.message({
      topic: secondJoin.topic,
      event: 'system',
      payload: {status: 'error', extension: 'postgres_changes'},
      ref: null,
      join_ref: secondJoin.ref,
    });
    expect(observer.status).toHaveBeenLastCalledWith(
      'CHANNEL_ERROR',
      expect.objectContaining({code: 'SUPABASE_REALTIME_SYSTEM_ERROR'}),
    );

    await connection.close();
  });

  it('aborts pending joins and cancels reconnects after close', async () => {
    const harness = createHarness({reconnectDelaysMs: [5]});
    const controller = new AbortController();
    const pending = harness.connect({signal: controller.signal}).promise;
    controller.abort();
    await expect(pending).rejects.toMatchObject({
      code: 'SUPABASE_SOURCE_ABORTED',
    });
    expect(harness.sockets[0]!.closeCalls).toBe(1);
    expect(harness.timer.pending()).toBe(0);

    const secondHarness = createHarness({reconnectDelaysMs: [5]});
    const connected = secondHarness.connect();
    const socket = secondHarness.sockets[0]!;
    socket.open();
    const join = socket.lastSent();
    socket.joinReply(join, expectedSubscriptions());
    const connection = await connected.promise;
    socket.serverClose();
    await connection.close();
    secondHarness.timer.advance(100);
    expect(secondHarness.sockets).toHaveLength(1);
  });
});

function createHarness(
  overrides: Partial<{
    heartbeatIntervalMs: number;
    joinTimeoutMs: number;
    reconnectDelaysMs: readonly number[];
  }> = {},
): {
  readonly timer: FakeTimer;
  readonly sockets: FakeSocket[];
  readonly urls: string[];
  connect(
    overrides?: Partial<ReturnType<typeof connectOptions>>,
  ): {
    promise: ReturnType<ReturnType<typeof createSupabaseRealtimeTransport>['connect']>;
    observer: ReturnType<typeof observer>;
  };
} {
  const timer = new FakeTimer();
  const sockets: FakeSocket[] = [];
  const urls: string[] = [];
  const transport = createSupabaseRealtimeTransport({
    url: 'https://project.supabase.co/',
    publishableKey,
    timer,
    heartbeatIntervalMs: overrides.heartbeatIntervalMs ?? 20_000,
    joinTimeoutMs: overrides.joinTimeoutMs ?? 10_000,
    reconnectDelaysMs: overrides.reconnectDelaysMs ?? [1_000, 2_000],
    createWebSocket: (url) => {
      urls.push(url);
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
  });
  return {
    timer,
    sockets,
    urls,
    connect(connectOverrides = {}) {
      const target = observer();
      return {
        promise: transport.connect(
          {...connectOptions(), ...connectOverrides},
          target,
        ),
        observer: target,
      };
    },
  };
}

function connectOptions() {
  return {
    sourceId: 'supabase:test/source',
    tables: [
      {
        schema: 'public',
        table: 'posts',
        columns: ['id', 'title'],
      },
      {schema: 'private', table: 'memberships'},
    ],
    accessToken: null as string | null,
    signal: new AbortController().signal,
  };
}

function observer() {
  return {
    payload: vi.fn<SupabaseRealtimeObserver['payload']>(),
    status: vi.fn<SupabaseRealtimeObserver['status']>(),
  };
}

function expectedSubscriptions() {
  return [
    {id: 4, event: '*', schema: 'public', table: 'posts'},
    {id: 7, event: '*', schema: 'private', table: 'memberships'},
  ];
}

class FakeSocket implements SupabaseRealtimeWebSocket {
  readyState = 0;
  readonly sent: string[] = [];
  closeCalls = 0;
  readonly #listeners = new Map<
    string,
    Set<(event: Event | CloseEvent | MessageEvent<unknown>) => void>
  >();

  send(data: string): void {
    if (this.readyState !== 1) {
      throw new Error('socket is not open');
    }
    this.sent.push(data);
  }

  close(): void {
    this.closeCalls += 1;
    this.readyState = 3;
  }

  addEventListener(
    type: 'open' | 'error' | 'close' | 'message',
    listener: (event: Event | CloseEvent | MessageEvent<unknown>) => void,
  ): void {
    const listeners = this.#listeners.get(type) ?? new Set();
    listeners.add(listener);
    this.#listeners.set(type, listeners);
  }

  removeEventListener(
    type: 'open' | 'error' | 'close' | 'message',
    listener: (event: Event | CloseEvent | MessageEvent<unknown>) => void,
  ): void {
    this.#listeners.get(type)?.delete(listener);
  }

  open(): void {
    this.readyState = 1;
    this.#emit('open', new Event('open'));
  }

  message(value: unknown): void {
    this.#emit(
      'message',
      {data: JSON.stringify(value)} as MessageEvent<unknown>,
    );
  }

  serverClose(): void {
    this.readyState = 3;
    this.#emit('close', new Event('close'));
  }

  lastSent(): Record<string, any> {
    const data = this.sent.at(-1);
    if (!data) {
      throw new Error('No WebSocket message was sent');
    }
    return JSON.parse(data);
  }

  joinReply(
    join: Record<string, any>,
    subscriptions: Array<Record<string, unknown>>,
  ): void {
    this.message({
      topic: join.topic,
      event: 'phx_reply',
      payload: {
        status: 'ok',
        response: {postgres_changes: subscriptions},
      },
      ref: join.ref,
      join_ref: join.ref,
    });
  }

  #emit(
    type: 'open' | 'error' | 'close' | 'message',
    event: Event | CloseEvent | MessageEvent<unknown>,
  ): void {
    for (const listener of [...(this.#listeners.get(type) ?? [])]) {
      listener(event);
    }
  }
}

class FakeTimer implements SupabaseRealtimeTimer {
  #now = 0;
  #sequence = 0;
  readonly #tasks = new Map<number, {at: number; callback: () => void}>();

  setTimeout(callback: () => void, delayMs: number): number {
    const handle = ++this.#sequence;
    this.#tasks.set(handle, {at: this.#now + delayMs, callback});
    return handle;
  }

  clearTimeout(handle: unknown): void {
    if (typeof handle === 'number') {
      this.#tasks.delete(handle);
    }
  }

  advance(delayMs: number): void {
    const target = this.#now + delayMs;
    while (true) {
      const next = [...this.#tasks.entries()]
        .filter(([, task]) => task.at <= target)
        .sort((left, right) => left[1].at - right[1].at || left[0] - right[0])[0];
      if (!next) {
        break;
      }
      const [handle, task] = next;
      this.#tasks.delete(handle);
      this.#now = task.at;
      task.callback();
    }
    this.#now = target;
  }

  pending(): number {
    return this.#tasks.size;
  }

  nextDelay(): number | undefined {
    const next = [...this.#tasks.values()].sort(
      (left, right) => left.at - right.at,
    )[0];
    return next ? next.at - this.#now : undefined;
  }
}
