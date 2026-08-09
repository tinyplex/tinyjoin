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
      fetch: async () =>
        new Response(JSON.stringify([{id: 1, title: 'snapshot'}])),
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
      fetch: async () => {
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
      fetch: async () =>
        new Response(JSON.stringify([{id: ++snapshotVersion}])),
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
      fetch: async () => {
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

  it('validates browser-safe configuration and closes its subscription', async () => {
    const realtime = new FakeRealtime();
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

  async connect(
    options: SupabaseRealtimeConnectOptions,
    observer: SupabaseRealtimeObserver,
  ): Promise<SupabaseRealtimeConnection> {
    this.options = options;
    this.observer = observer;
    observer.status('SUBSCRIBED');
    return {close: this.close};
  }
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
