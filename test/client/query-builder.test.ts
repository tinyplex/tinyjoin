import {describe, expect, it, vi} from 'vitest';

import {
  QueryBuilder,
  type QueryExecutor,
} from '../../src/client/query-builder.ts';
import {TinygresError} from '../../src/client/error.ts';
import type {QueryPlan, Row} from '../../src/protocol.ts';

describe('QueryBuilder', () => {
  it('builds immutable structured plans without interpolating values', async () => {
    const executePlan = vi.fn(async (plan: QueryPlan) => ({
      revision: 4,
      rows: [{id: 1, title: String(plan.filters[0]?.value)}],
    }));
    const executor: QueryExecutor = {executePlan};
    const base = new QueryBuilder(executor, {table: 'posts', filters: []});
    const filtered = base
      .select('id, title')
      .eq('title', "Robert'); drop table posts; --")
      .limit(1);

    const response = await filtered;
    expect(response).toEqual({
      data: [{id: 1, title: "Robert'); drop table posts; --"}],
      error: null,
      revision: 4,
    });
    expect(executePlan).toHaveBeenCalledWith({
      table: 'posts',
      columns: ['id', 'title'],
      filters: [
        {
          column: 'title',
          operator: 'eq',
          value: "Robert'); drop table posts; --",
        },
      ],
      limit: 1,
    });

    await base.execute();
    expect(executePlan).toHaveBeenLastCalledWith({table: 'posts', filters: []});
  });

  it('returns Supabase-shaped errors from execution', async () => {
    const executor: QueryExecutor = {
      executePlan: () =>
        Promise.reject(
          new TinygresError({
            code: 'TABLE_NOT_FOUND',
            message: 'missing',
          }),
        ),
    };

    const response = await new QueryBuilder<Row>(executor, {
      table: 'missing',
      filters: [],
    });

    expect(response.data).toBeNull();
    expect(response.error).toMatchObject({code: 'TABLE_NOT_FOUND'});
  });

  it('validates limits and projections before crossing the worker boundary', () => {
    const executor: QueryExecutor = {
      executePlan: () => Promise.resolve({revision: 0, rows: []}),
    };
    const builder = new QueryBuilder(executor, {table: 'posts', filters: []});

    expect(() => builder.limit(-1)).toThrow('non-negative safe integer');
    expect(() => builder.select('id, ')).toThrow('one or more column names');
  });
});
