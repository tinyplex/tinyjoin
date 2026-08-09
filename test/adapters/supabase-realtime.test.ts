import {describe, expect, it, vi} from 'vitest';

import {
  createSupabaseJsRealtimeTransport,
  normalizeSupabaseChange,
} from '../../src/adapters/supabase/realtime.ts';
import type {NormalizedSupabaseTable} from '../../src/adapters/supabase/types.ts';

const memberships: NormalizedSupabaseTable = {
  schema: 'public',
  table: 'memberships',
  localName: 'memberships',
  primaryKey: ['team_id', 'user_id'],
  columns: ['team_id', 'user_id', 'role'],
};

describe('normalizeSupabaseChange', () => {
  it('normalizes transformed insert and raw-protocol delete payloads', () => {
    expect(
      normalizeSupabaseChange(
        {
          schema: 'public',
          table: 'memberships',
          eventType: 'INSERT',
          commit_timestamp: '2026-08-09T01:02:03.000Z',
          new: {
            team_id: 1,
            user_id: 2,
            role: 'member',
            not_selected: 'discard me',
          },
          old: {},
          errors: null,
        },
        memberships,
        {
          sourceId: 'source',
          expectedColumns: ['team_id', 'user_id', 'role'],
        },
      ),
    ).toEqual({
      sourceId: 'source',
      committedAt: '2026-08-09T01:02:03.000Z',
      changes: [
        {
          type: 'upsert',
          table: 'memberships',
          row: {team_id: 1, user_id: 2, role: 'member'},
        },
      ],
    });

    expect(
      normalizeSupabaseChange(
        {
          schema: 'public',
          table: 'memberships',
          type: 'DELETE',
          old_record: {team_id: 1, user_id: 2},
          errors: null,
        },
        memberships,
        {sourceId: 'source'},
      ),
    ).toEqual({
      sourceId: 'source',
      changes: [
        {
          type: 'delete',
          table: 'memberships',
          key: {team_id: 1, user_id: 2},
        },
      ],
    });
  });

  it('turns a primary-key update into an atomic delete and upsert batch', () => {
    expect(
      normalizeSupabaseChange(
        {
          schema: 'public',
          table: 'memberships',
          eventType: 'UPDATE',
          new: {team_id: 2, user_id: 7, role: 'owner'},
          old: {team_id: 1, user_id: 7},
          errors: null,
        },
        memberships,
        {
          sourceId: 'source',
          expectedColumns: ['team_id', 'user_id', 'role'],
        },
      ).changes,
    ).toEqual([
      {
        type: 'delete',
        table: 'memberships',
        key: {team_id: 1, user_id: 7},
      },
      {
        type: 'upsert',
        table: 'memberships',
        row: {team_id: 2, user_id: 7, role: 'owner'},
      },
    ]);
  });

  it('rejects truncated, erroneous, or keyless events', () => {
    const base = {
      schema: 'public',
      table: 'memberships',
      eventType: 'UPDATE',
      old: {team_id: 1, user_id: 2},
      errors: null,
    };
    expect(() =>
      normalizeSupabaseChange(
        {...base, new: {team_id: 1, user_id: 2}},
        memberships,
        {
          sourceId: 'source',
          expectedColumns: ['team_id', 'user_id', 'role'],
        },
      ),
    ).toThrow(/missing columns: role/);
    expect(() =>
      normalizeSupabaseChange(
        {...base, new: {team_id: 1, user_id: 2, role: 'x'}, errors: 'too big'},
        memberships,
        {sourceId: 'source'},
      ),
    ).toThrow(/decoding error/);
    expect(() =>
      normalizeSupabaseChange(
        {
          ...base,
          eventType: 'DELETE',
          old: {team_id: 1},
        },
        memberships,
        {sourceId: 'source'},
      ),
    ).toThrow(/user_id/);
  });
});

describe('createSupabaseJsRealtimeTransport', () => {
  it('subscribes all configured tables and forwards later connection states', async () => {
    const channel = new FakeChannel();
    const setAuth = vi.fn();
    const client = {channel: vi.fn(() => channel), realtime: {setAuth}};
    const transport = createSupabaseJsRealtimeTransport(client);
    const status = vi.fn();
    const payload = vi.fn();
    const controller = new AbortController();

    const connectionPromise = transport.connect(
      {
        sourceId: 'supabase:test',
        tables: [
          {schema: 'public', table: 'posts'},
          {schema: 'public', table: 'users'},
        ],
        accessToken: 'jwt',
        signal: controller.signal,
      },
      {status, payload},
    );
    await vi.waitFor(() => expect(channel.subscribeCallback).toBeDefined());
    channel.subscribeCallback!('SUBSCRIBED');
    const connection = await connectionPromise;

    expect(setAuth).toHaveBeenCalledWith('jwt');
    expect(channel.filters).toEqual([
      {event: '*', schema: 'public', table: 'posts'},
      {event: '*', schema: 'public', table: 'users'},
    ]);
    channel.callbacks[0]!({eventType: 'INSERT'});
    channel.subscribeCallback!('CHANNEL_ERROR', new Error('offline'));
    expect(payload).toHaveBeenCalledWith({eventType: 'INSERT'});
    expect(status).toHaveBeenLastCalledWith(
      'CHANNEL_ERROR',
      expect.any(Error),
    );

    await connection.close();
    expect(channel.unsubscribe).toHaveBeenCalledOnce();
  });
});

class FakeChannel {
  readonly filters: Array<{event: '*'; schema: string; table: string}> = [];
  readonly callbacks: Array<(payload: unknown) => void> = [];
  subscribeCallback: ((status: string, error?: unknown) => void) | undefined;
  readonly unsubscribe = vi.fn();

  on(
    _type: 'postgres_changes',
    filter: {event: '*'; schema: string; table: string},
    callback: (payload: unknown) => void,
  ): this {
    this.filters.push(filter);
    this.callbacks.push(callback);
    return this;
  }

  subscribe(callback: (status: string, error?: unknown) => void): this {
    this.subscribeCallback = callback;
    return this;
  }
}
