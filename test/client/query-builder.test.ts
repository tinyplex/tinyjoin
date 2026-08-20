import {describe, expect, it, vi} from 'vitest';

import {
  QueryBuilder,
  type QueryExecutor,
} from '../../src/client/query-builder.ts';
import {ClientError} from '../../src/client/error.ts';
import {
  MAX_QUERY_POSITION,
  type QueryPlan,
  type Row,
} from '../../src/protocol.ts';

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

  it('returns structured errors from execution', async () => {
    const executor: QueryExecutor = {
      executePlan: () =>
        Promise.reject(
          new ClientError({
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

  it('builds typed comparison filters without mutating its base', async () => {
    const executePlan = vi.fn(async () => ({revision: 1, rows: []}));
    const executor: QueryExecutor = {executePlan};
    const base = new QueryBuilder(executor, {table: 'tasks', filters: []});

    await base
      .neq('state', 'archived')
      .gt('priority', 1)
      .gte('score', 2)
      .lt('age', 10)
      .lte('attempts', 3);

    expect(executePlan).toHaveBeenCalledWith({
      table: 'tasks',
      filters: [
        {column: 'state', operator: 'neq', value: 'archived'},
        {column: 'priority', operator: 'gt', value: 1},
        {column: 'score', operator: 'gte', value: 2},
        {column: 'age', operator: 'lt', value: 10},
        {column: 'attempts', operator: 'lte', value: 3},
      ],
    });
    await base;
    expect(executePlan).toHaveBeenLastCalledWith({
      table: 'tasks',
      filters: [],
    });
  });

  it('builds stable ordering, offsets, and inclusive ranges', async () => {
    const executePlan = vi.fn(async () => ({revision: 1, rows: []}));
    const builder = new QueryBuilder({executePlan}, {
      table: 'tasks',
      filters: [],
    });

    await builder
      .order('done', {ascending: false, nullsFirst: false})
      .order('id')
      .range(10, 19);
    expect(executePlan).toHaveBeenCalledWith({
      table: 'tasks',
      filters: [],
      orderBy: [
        {column: 'done', direction: 'desc', nulls: 'last'},
        {column: 'id', direction: 'asc', nulls: 'default'},
      ],
      offset: 10,
      limit: 10,
    });

    await builder.offset(3);
    expect(executePlan).toHaveBeenLastCalledWith({
      table: 'tasks',
      filters: [],
      offset: 3,
    });
  });

  it('validates limits and projections before crossing the worker boundary', () => {
    const executor: QueryExecutor = {
      executePlan: () => Promise.resolve({revision: 0, rows: []}),
    };
    const builder = new QueryBuilder(executor, {table: 'posts', filters: []});

    expect(() => builder.limit(MAX_QUERY_POSITION)).not.toThrow();
    expect(() => builder.offset(MAX_QUERY_POSITION)).not.toThrow();
    expect(() =>
      builder.range(MAX_QUERY_POSITION - 1, MAX_QUERY_POSITION - 1),
    ).not.toThrow();
    expect(() => builder.limit(-1)).toThrow('between 0');
    expect(() => builder.offset(-1)).toThrow('between 0');
    expect(() => builder.limit(MAX_QUERY_POSITION + 1)).toThrow('between 0');
    expect(() => builder.offset(MAX_QUERY_POSITION + 1)).toThrow('between 0');
    expect(() => builder.range(0, MAX_QUERY_POSITION)).toThrow('range length');
    expect(() =>
      builder.range(MAX_QUERY_POSITION, MAX_QUERY_POSITION),
    ).toThrow('offset plus limit');
    expect(() => builder.offset(MAX_QUERY_POSITION).limit(1)).toThrow(
      'offset plus limit',
    );
    expect(() => builder.limit(1).offset(MAX_QUERY_POSITION)).toThrow(
      'offset plus limit',
    );
    expect(() => builder.range(3, 2)).toThrow('cannot be smaller');
    expect(() => builder.order('')).toThrow('cannot be empty');
    expect(() => builder.select('id, ')).toThrow('one or more column names');
  });
});
