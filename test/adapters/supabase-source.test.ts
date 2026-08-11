import {describe, expect, it, vi} from 'vitest';

import type {ReplicaSourceContext} from '../../src/adapters/types.ts';
import {
  createSupabaseSource,
  SUPABASE_SOURCE_CAPABILITIES,
} from '../../src/adapters/supabase/source.ts';
import type {
  SupabaseRealtimeConnectOptions,
  SupabaseRealtimeConnection,
  SupabaseRealtimeObserver,
  SupabaseRealtimeTransport,
} from '../../src/adapters/supabase/types.ts';
import {SupabaseSourceError} from '../../src/adapters/supabase/types.ts';
import type {
  ApplyOutcome,
  ChangeBatch,
  Row,
  SyncState,
  TableSchema,
} from '../../src/protocol.ts';

describe('SupabaseSource', () => {
  it('declares honest capabilities, snapshots, and applies live changes', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {
          schema: 'public',
          table: 'posts',
          primaryKey: ['id'],
          columns: ['id', 'title'],
        },
      ],
      realtime,
      fetch: async (_input, init) =>
        snapshotPage(init, [{id: 1, title: 'snapshot'}]),
      now: () => '2026-08-09T00:00:00.000Z',
    });

    await source.start(context.value);

    expect(source.capabilities).toEqual(SUPABASE_SOURCE_CAPABILITIES);
    expect(source.capabilities).toEqual({
      snapshotConsistency: 'eventual',
      changes: 'best-effort',
      resume: 'none',
      atomicity: 'row',
      writes: false,
    });
    expect(context.defineTable).toHaveBeenCalledWith({
      name: 'posts',
      primaryKey: ['id'],
    });
    expect(context.replaceTable).toHaveBeenCalledWith(
      {name: 'posts', primaryKey: ['id']},
      [{id: 1, title: 'snapshot'}],
    );
    expect(context.states).toEqual([
      {phase: 'snapshotting', sourceId: source.id},
      {
        phase: 'live-best-effort',
        sourceId: source.id,
        lastReconciledAt: '2026-08-09T00:00:00.000Z',
      },
    ]);

    realtime.observer!.payload({
      schema: 'public',
      table: 'posts',
      eventType: 'INSERT',
      new: {id: 2, title: 'live'},
      old: {},
      errors: null,
    });
    await vi.waitFor(() => expect(context.applyBatch).toHaveBeenCalledOnce());
    expect(context.applyBatch).toHaveBeenCalledWith({
      sourceId: source.id,
      changes: [
        {type: 'upsert', table: 'posts', row: {id: 2, title: 'live'}},
      ],
    });
  });

  it('retries a snapshot dirtied by a concurrent Realtime event', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    let fetches = 0;
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {schema: 'public', table: 'posts', primaryKey: ['id']},
      ],
      realtime,
      fetch: async (_input, init) => {
        if (!isFirstSnapshotPage(init)) {
          return new Response('[]');
        }
        fetches += 1;
        if (fetches === 1) {
          realtime.observer!.payload({
            schema: 'public',
            table: 'posts',
            eventType: 'UPDATE',
            new: {id: 1, title: 'changed'},
            old: {id: 1},
            errors: null,
          });
        }
        return new Response(
          JSON.stringify([
            {id: 1, title: fetches === 1 ? 'first' : 'reconciled'},
          ]),
        );
      },
    });

    await source.start(context.value);

    expect(fetches).toBe(2);
    expect(context.replaceTable).toHaveBeenCalledOnce();
    expect(context.replaceTable).toHaveBeenCalledWith(
      {name: 'posts', primaryKey: ['id']},
      [{id: 1, title: 'reconciled'}],
    );
    expect(context.applyBatch).not.toHaveBeenCalled();
  });

  it('marks disconnects stale and resnapshots after the SDK reconnects', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    let snapshotVersion = 0;
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {schema: 'public', table: 'posts', primaryKey: ['id']},
      ],
      realtime,
      fetch: async (_input, init) => {
        if (!isFirstSnapshotPage(init)) {
          return new Response('[]');
        }
        return new Response(JSON.stringify([{id: ++snapshotVersion}]));
      },
      now: () => `reconciled-${snapshotVersion}`,
    });
    await source.start(context.value);

    realtime.observer!.status('CHANNEL_ERROR', new Error('offline'));
    expect(context.states.at(-1)).toMatchObject({
      phase: 'stale',
      sourceId: source.id,
    });
    realtime.observer!.status('SUBSCRIBED');

    await vi.waitFor(() =>
      expect(context.replaceTable).toHaveBeenCalledTimes(2),
    );
    await vi.waitFor(() =>
      expect(context.states.at(-1)).toEqual({
        phase: 'live-best-effort',
        sourceId: source.id,
        lastReconciledAt: 'reconciled-2',
      }),
    );

    realtime.observer!.status('TIMED_OUT');
    realtime.observer!.status('SUBSCRIBED');
    await vi.waitFor(() =>
      expect(context.replaceTable).toHaveBeenCalledTimes(3),
    );
    await vi.waitFor(() =>
      expect(context.states.at(-1)).toEqual({
        phase: 'live-best-effort',
        sourceId: source.id,
        lastReconciledAt: 'reconciled-3',
      }),
    );
    expect(
      context.states.filter(({phase}) => phase === 'resyncing'),
    ).toHaveLength(2);
  });

  it('resnapshots instead of applying a partial Realtime row', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    let snapshots = 0;
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {
          schema: 'public',
          table: 'posts',
          primaryKey: ['id'],
          columns: ['id', 'title'],
        },
      ],
      realtime,
      fetch: async (_input, init) => {
        if (!isFirstSnapshotPage(init)) {
          return new Response('[]');
        }
        snapshots += 1;
        return new Response(JSON.stringify([{id: 1, title: 'complete'}]));
      },
    });
    await source.start(context.value);

    realtime.observer!.payload({
      schema: 'public',
      table: 'posts',
      eventType: 'UPDATE',
      new: {id: 1},
      old: {id: 1},
      errors: null,
    });

    await vi.waitFor(() => expect(snapshots).toBe(2));
    expect(context.applyBatch).not.toHaveBeenCalled();
    expect(context.states).toContainEqual(
      expect.objectContaining({
        phase: 'stale',
        error: expect.objectContaining({
          code: 'SUPABASE_INVALID_REALTIME_PAYLOAD',
        }),
      }),
    );
  });

  it('quarantines a generation interrupted during its snapshot', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    let snapshots = 0;
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {
          schema: 'public',
          table: 'posts',
          primaryKey: ['id'],
          columns: ['id', 'title'],
        },
      ],
      realtime,
      retryDelaysMs: [0],
      sleep: async () => undefined,
      fetch: async (_input, init) => {
        if (!isFirstSnapshotPage(init)) {
          return new Response('[]');
        }
        snapshots += 1;
        if (snapshots === 1) {
          realtime.observer!.status(
            'CHANNEL_ERROR',
            new SupabaseSourceError(
              'SUPABASE_REALTIME_DISCONNECTED',
              'offline',
              true,
            ),
          );
          realtime.observer!.status('SUBSCRIBED');
        }
        return new Response(
          JSON.stringify([
            {id: 1, title: snapshots === 1 ? 'unsafe' : 'reconciled'},
          ]),
        );
      },
    });

    await source.start(context.value);

    expect(snapshots).toBe(2);
    expect(context.replaceTable).toHaveBeenCalledOnce();
    expect(context.replaceTable).toHaveBeenCalledWith(
      {name: 'posts', primaryKey: ['id']},
      [{id: 1, title: 'reconciled'}],
    );
    const liveIndexes = context.states
      .map(({phase}, index) => (phase === 'live-best-effort' ? index : -1))
      .filter((index) => index >= 0);
    const staleIndex = context.states.findIndex(({phase}) => phase === 'stale');
    expect(staleIndex).toBeGreaterThanOrEqual(0);
    expect(liveIndexes).toHaveLength(1);
    expect(liveIndexes[0]).toBeGreaterThan(staleIndex);
  });

  it('drops queued deltas from an old connection generation', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    let snapshot = 0;
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {schema: 'public', table: 'posts', primaryKey: ['id']},
      ],
      realtime,
      retryDelaysMs: [0],
      sleep: async () => undefined,
      fetch: async (_input, init) => {
        if (!isFirstSnapshotPage(init)) {
          return new Response('[]');
        }
        return new Response(JSON.stringify([{id: 1, snapshot: ++snapshot}]));
      },
    });
    await source.start(context.value);

    realtime.observer!.payload({
      schema: 'public',
      table: 'posts',
      eventType: 'INSERT',
      new: {id: 2, snapshot: 1},
      old: {},
      errors: null,
    });
    realtime.observer!.status(
      'CHANNEL_ERROR',
      new SupabaseSourceError(
        'SUPABASE_REALTIME_DISCONNECTED',
        'offline',
        true,
      ),
    );
    realtime.observer!.status('SUBSCRIBED');

    await vi.waitFor(() =>
      expect(context.states.at(-1)?.phase).toBe('live-best-effort'),
    );
    expect(context.replaceTable.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(context.applyBatch).not.toHaveBeenCalled();
    expect(context.replaceTable).toHaveBeenLastCalledWith(
      {name: 'posts', primaryKey: ['id']},
      [{id: 1, snapshot}],
    );
  });

  it('repairs an UPDATE without an old primary key and exposes stale before live', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    let snapshots = 0;
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {
          schema: 'public',
          table: 'posts',
          primaryKey: ['id'],
          columns: ['id', 'title'],
        },
      ],
      realtime,
      retryDelaysMs: [0],
      sleep: async () => undefined,
      fetch: async (_input, init) => {
        if (!isFirstSnapshotPage(init)) {
          return new Response('[]');
        }
        snapshots += 1;
        return new Response(JSON.stringify([{id: 1, title: 'complete'}]));
      },
    });
    await source.start(context.value);

    realtime.observer!.payload({
      schema: 'public',
      table: 'posts',
      eventType: 'UPDATE',
      new: {id: 1, title: 'partial-history'},
      old: {},
      errors: null,
    });

    await vi.waitFor(() => expect(snapshots).toBe(2));
    expect(context.applyBatch).not.toHaveBeenCalled();
    const phases = context.states.map(({phase}) => phase);
    expect(phases.slice(-3)).toEqual([
      'stale',
      'resyncing',
      'live-best-effort',
    ]);
    expect(context.states.at(-3)).toMatchObject({
      error: {code: 'SUPABASE_INVALID_REALTIME_PAYLOAD'},
    });
  });

  it('retries transient transport and snapshot failures with capped delays', async () => {
    const realtime = new FakeRealtime([
      new SupabaseSourceError('SUPABASE_REALTIME_CONNECT_FAILED', 'one', true),
      new SupabaseSourceError('SUPABASE_REALTIME_CONNECT_FAILED', 'two', true),
      new SupabaseSourceError('SUPABASE_REALTIME_CONNECT_FAILED', 'three', true),
    ]);
    const context = fakeContext();
    const delays: number[] = [];
    let fetches = 0;
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {schema: 'public', table: 'posts', primaryKey: ['id']},
      ],
      realtime,
      retryDelaysMs: [1, 2],
      sleep: async (delay) => {
        delays.push(delay);
      },
      fetch: async (_input, init) => {
        if (!isFirstSnapshotPage(init)) {
          return new Response('[]');
        }
        fetches += 1;
        if (fetches === 1) {
          return new Response('{}', {status: 503});
        }
        return new Response(JSON.stringify([{id: 1}]));
      },
    });

    await source.start(context.value);

    expect(realtime.connectCalls).toBe(4);
    expect(fetches).toBe(2);
    expect(delays).toEqual([1, 2, 2, 1]);
    expect(context.states.at(-1)?.phase).toBe('live-best-effort');
  });

  it('treats permanent snapshot permission failures as terminal', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    const sleep = vi.fn(async () => undefined);
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [
        {schema: 'public', table: 'posts', primaryKey: ['id']},
      ],
      realtime,
      sleep,
      fetch: async () => new Response('{}', {status: 403}),
    });

    await expect(source.start(context.value)).rejects.toMatchObject({
      code: 'SUPABASE_SNAPSHOT_FAILED',
      retryable: false,
    });
    expect(sleep).not.toHaveBeenCalled();
    expect(context.states).not.toContainEqual(
      expect.objectContaining({phase: 'live-best-effort'}),
    );
  });

  it('samples one access token for Realtime and every REST page', async () => {
    const realtime = new FakeRealtime();
    const context = fakeContext();
    const getAccessToken = vi.fn(async () => 'fixed-user-jwt');
    const authorizations: string[] = [];
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      pageSize: 1,
      tables: [
        {schema: 'public', table: 'posts', primaryKey: ['id']},
      ],
      realtime,
      getAccessToken,
      fetch: async (_input, init) => {
        authorizations.push(
          new Headers(init?.headers).get('Authorization') ?? '',
        );
        return new Response(
          authorizations.length === 1 ? JSON.stringify([{id: 1}]) : '[]',
        );
      },
    });

    await source.start(context.value);

    expect(getAccessToken).toHaveBeenCalledOnce();
    expect(realtime.options?.accessToken).toBe('fixed-user-jwt');
    expect(authorizations).toEqual([
      'Bearer fixed-user-jwt',
      'Bearer fixed-user-jwt',
    ]);
  });

  it('validates browser-safe configuration and closes its subscription', async () => {
    const realtime = new FakeRealtime();
    expect(() =>
      createSupabaseSource({
        url: 'https://user:password@project.supabase.co',
        publishableKey: 'sb_publishable_test',
        tables: [{table: 'posts', primaryKey: ['id']}],
        realtime,
      }),
    ).toThrow(/must not contain credentials/);
    expect(() =>
      createSupabaseSource({
        url: 'https://project.supabase.co',
        publishableKey: 'sb_secret_do-not-use',
        tables: [
          {schema: 'public', table: 'posts', primaryKey: ['id']},
        ],
        realtime,
      }),
    ).toThrow(/secret keys/);
    const legacyServiceRole = [
      encodeBase64Url({alg: 'HS256', typ: 'JWT'}),
      encodeBase64Url({role: 'service_role'}),
      'signature',
    ].join('.');
    expect(() =>
      createSupabaseSource({
        url: 'https://project.supabase.co',
        publishableKey: legacyServiceRole,
        tables: [
          {schema: 'public', table: 'posts', primaryKey: ['id']},
        ],
        realtime,
      }),
    ).toThrow(/secret keys/);
    expect(() =>
      createSupabaseSource({
        url: 'https://project.supabase.co',
        publishableKey: 'sb_publishable_test',
        tables: [
          {schema: 'public', table: 'posts', primaryKey: []},
        ],
        realtime,
      }),
    ).toThrow(/primary key/);

    const context = fakeContext();
    const source = createSupabaseSource({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      tables: [{schema: 'public', table: 'posts', primaryKey: ['id']}],
      realtime,
      fetch: async () => new Response('[]'),
    });
    await source.start(context.value);
    await source.close();
    await source.close();
    expect(realtime.close).toHaveBeenCalledOnce();
  });
});

class FakeRealtime implements SupabaseRealtimeTransport {
  options: SupabaseRealtimeConnectOptions | undefined;
  observer: SupabaseRealtimeObserver | undefined;
  readonly close = vi.fn();
  connectCalls = 0;

  constructor(readonly failures: unknown[] = []) {}

  async connect(
    options: SupabaseRealtimeConnectOptions,
    observer: SupabaseRealtimeObserver,
  ): Promise<SupabaseRealtimeConnection> {
    this.connectCalls += 1;
    const failure = this.failures.shift();
    if (failure) {
      throw failure;
    }
    this.options = options;
    this.observer = observer;
    observer.status('SUBSCRIBED');
    return {close: this.close};
  }
}

function encodeBase64Url(value: unknown): string {
  return Buffer.from(JSON.stringify(value)).toString('base64url');
}

function isFirstSnapshotPage(init?: RequestInit): boolean {
  return new Headers(init?.headers).get('Range')?.startsWith('0-') ?? false;
}

function snapshotPage(init: RequestInit | undefined, rows: Row[]): Response {
  return new Response(JSON.stringify(isFirstSnapshotPage(init) ? rows : []));
}

function fakeContext(): {
  value: ReplicaSourceContext;
  defineTable: ReturnType<typeof vi.fn>;
  replaceTable: ReturnType<typeof vi.fn>;
  applyBatch: ReturnType<typeof vi.fn>;
  states: SyncState[];
} {
  let revision = 0;
  const states: SyncState[] = [];
  const defineTable = vi.fn(async (_schema: TableSchema) => undefined);
  const replaceTable = vi.fn(
    async (schema: TableSchema, _rows: Row[]): Promise<ApplyOutcome> => ({
      revision: ++revision,
      tables: [schema.name],
    }),
  );
  const applyBatch = vi.fn(
    async (batch: ChangeBatch): Promise<ApplyOutcome> => ({
      revision: ++revision,
      tables: [...new Set(batch.changes.map((change) => change.table))],
    }),
  );
  return {
    defineTable,
    replaceTable,
    applyBatch,
    states,
    value: {
      signal: new AbortController().signal,
      defineTable,
      replaceTable,
      applyBatch,
      setSyncState: (state) => states.push(state),
    },
  };
}
