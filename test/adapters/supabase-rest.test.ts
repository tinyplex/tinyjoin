import {describe, expect, it, vi} from 'vitest';

import {SupabaseRestSnapshotReader} from '../../src/adapters/supabase/rest.ts';
import type {NormalizedSupabaseTable} from '../../src/adapters/supabase/types.ts';

const memberships: NormalizedSupabaseTable = {
  schema: 'private',
  table: 'memberships',
  localName: 'private.memberships',
  primaryKey: ['team_id', 'user_id'],
  columns: ['team_id', 'user_id', 'role'],
};

describe('SupabaseRestSnapshotReader', () => {
  it('paginates with inclusive ranges and stable primary-key ordering', async () => {
    const calls: Array<{url: URL; init: RequestInit}> = [];
    const fetch = vi.fn(async (input: URL | RequestInfo, init?: RequestInit) => {
      const url = new URL(String(input));
      calls.push({url, init: init ?? {}});
      const range = new Headers(init?.headers).get('Range');
      const rows =
        range === '0-1'
          ? [
              {team_id: 1, user_id: 1, role: 'owner'},
              {team_id: 1, user_id: 2, role: 'member'},
            ]
          : [{team_id: 2, user_id: 1, role: 'owner'}];
      return new Response(JSON.stringify(rows));
    });
    const reader = new SupabaseRestSnapshotReader({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      pageSize: 2,
      fetch,
      getAccessToken: () => 'user-jwt',
    });

    const pages = [];
    for await (const page of reader.snapshot(
      memberships,
      new AbortController().signal,
    )) {
      pages.push(page);
    }

    expect(pages).toEqual([
      {
        page: 0,
        rows: [
          {team_id: 1, user_id: 1, role: 'owner'},
          {team_id: 1, user_id: 2, role: 'member'},
        ],
        done: false,
      },
      {
        page: 1,
        rows: [{team_id: 2, user_id: 1, role: 'owner'}],
        done: true,
      },
    ]);
    expect(calls.map(({init}) => new Headers(init.headers).get('Range'))).toEqual([
      '0-1',
      '2-3',
    ]);
    expect(calls[0]!.url.pathname).toBe('/rest/v1/memberships');
    expect(calls[0]!.url.searchParams.get('select')).toBe(
      'team_id,user_id,role',
    );
    expect(calls[0]!.url.searchParams.get('order')).toBe(
      'team_id.asc,user_id.asc',
    );
    const headers = new Headers(calls[0]!.init.headers);
    expect(headers.get('Accept-Profile')).toBe('private');
    expect(headers.get('Authorization')).toBe('Bearer user-jwt');
    expect(headers.get('apikey')).toBe('sb_publishable_test');
  });

  it('rejects HTTP failures and malformed or keyless rows', async () => {
    const table = {...memberships, schema: 'public'};
    const failing = new SupabaseRestSnapshotReader({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      fetch: async () => new Response('{}', {status: 503}),
    });
    await expect(
      collect(failing.snapshot(table, new AbortController().signal)),
    ).rejects.toMatchObject({
      code: 'SUPABASE_SNAPSHOT_FAILED',
      retryable: true,
    });

    const malformed = new SupabaseRestSnapshotReader({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      fetch: async () => new Response(JSON.stringify([{team_id: 1}])),
    });
    await expect(
      collect(malformed.snapshot(table, new AbortController().signal)),
    ).rejects.toMatchObject({code: 'SUPABASE_INVALID_ROW'});
  });

  it('stops before issuing a request when aborted', async () => {
    const fetch = vi.fn<typeof globalThis.fetch>();
    const reader = new SupabaseRestSnapshotReader({
      url: 'https://project.supabase.co',
      publishableKey: 'sb_publishable_test',
      fetch,
    });
    const controller = new AbortController();
    controller.abort();

    await expect(
      collect(reader.snapshot(memberships, controller.signal)),
    ).rejects.toMatchObject({code: 'SUPABASE_SOURCE_ABORTED'});
    expect(fetch).not.toHaveBeenCalled();
  });
});

async function collect<T>(iterable: AsyncIterable<T>): Promise<T[]> {
  const values: T[] = [];
  for await (const value of iterable) {
    values.push(value);
  }
  return values;
}
