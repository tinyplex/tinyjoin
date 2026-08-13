import {
  MAX_QUERY_POSITION,
  type JsonValue,
  type QueryPlan,
  type Row,
} from '../protocol.js';
import {ClientError} from './error.js';

export type QueryResponse<RowType extends object> =
  | {data: RowType[]; error: null; revision: number}
  | {data: null; error: ClientError; revision?: number};

export interface QueryExecutor {
  executePlan(plan: QueryPlan): Promise<{
    rows: Row[];
    revision: number;
  }>;
}

export interface OrderOptions {
  ascending?: boolean;
  nullsFirst?: boolean;
}

export class QueryBuilder<RowType extends object = Row>
  implements PromiseLike<QueryResponse<RowType>>
{
  readonly #executor: QueryExecutor;
  readonly #plan: QueryPlan;

  constructor(executor: QueryExecutor, plan: QueryPlan) {
    this.#executor = executor;
    this.#plan = plan;
  }

  select(columns = '*'): QueryBuilder<RowType> {
    const projection = parseProjection(columns);
    const {columns: _currentColumns, ...plan} = this.#plan;
    return new QueryBuilder(
      this.#executor,
      projection ? {...plan, columns: projection} : plan,
    );
  }

  eq(column: string, value: JsonValue): QueryBuilder<RowType> {
    return this.#filter(column, 'eq', value);
  }

  neq(column: string, value: JsonValue): QueryBuilder<RowType> {
    return this.#filter(column, 'neq', value);
  }

  lt(column: string, value: JsonValue): QueryBuilder<RowType> {
    return this.#filter(column, 'lt', value);
  }

  lte(column: string, value: JsonValue): QueryBuilder<RowType> {
    return this.#filter(column, 'lte', value);
  }

  gt(column: string, value: JsonValue): QueryBuilder<RowType> {
    return this.#filter(column, 'gt', value);
  }

  gte(column: string, value: JsonValue): QueryBuilder<RowType> {
    return this.#filter(column, 'gte', value);
  }

  #filter(
    column: string,
    operator: import('../protocol.js').Filter['operator'],
    value: JsonValue,
  ): QueryBuilder<RowType> {
    if (!column.trim()) {
      throw new ClientError({
        code: 'INVALID_QUERY',
        message: 'A filter column cannot be empty',
      });
    }
    return this.#with({
      filters: [
        ...this.#plan.filters,
        {column, operator, value},
      ],
    });
  }

  limit(limit: number): QueryBuilder<RowType> {
    assertNonNegativeInteger(limit, 'limit');
    assertQueryWindow(this.#plan.offset, limit);
    return this.#with({limit});
  }

  offset(offset: number): QueryBuilder<RowType> {
    assertNonNegativeInteger(offset, 'offset');
    assertQueryWindow(offset, this.#plan.limit);
    return this.#with({offset});
  }

  /** Uses the same option names as Supabase's query builder. */
  order(column: string, options: OrderOptions = {}): QueryBuilder<RowType> {
    if (!column.trim()) {
      throw new ClientError({
        code: 'INVALID_QUERY',
        message: 'An order column cannot be empty',
      });
    }
    return this.#with({
      orderBy: [
        ...(this.#plan.orderBy ?? []),
        {
          column,
          direction: options.ascending === false ? 'desc' : 'asc',
          nulls:
            options.nullsFirst === undefined
              ? 'default'
              : options.nullsFirst
                ? 'first'
                : 'last',
        },
      ],
    });
  }

  /** Selects the inclusive zero-based row range after filtering and ordering. */
  range(from: number, to: number): QueryBuilder<RowType> {
    assertNonNegativeInteger(from, 'range start');
    assertNonNegativeInteger(to, 'range end');
    if (to < from) {
      throw new ClientError({
        code: 'INVALID_QUERY',
        message: 'A query range end cannot be smaller than its start',
      });
    }
    const limit = to - from + 1;
    assertNonNegativeInteger(limit, 'range length');
    assertQueryWindow(from, limit);
    return this.#with({offset: from, limit});
  }

  async execute(): Promise<QueryResponse<RowType>> {
    try {
      const result = await this.#executor.executePlan(this.#plan);
      return {
        data: result.rows as RowType[],
        error: null,
        revision: result.revision,
      };
    } catch (error) {
      return {data: null, error: ClientError.fromUnknown(error)};
    }
  }

  then<TResult1 = QueryResponse<RowType>, TResult2 = never>(
    onfulfilled?:
      | ((value: QueryResponse<RowType>) => TResult1 | PromiseLike<TResult1>)
      | null,
    onrejected?: ((reason: unknown) => TResult2 | PromiseLike<TResult2>) | null,
  ): PromiseLike<TResult1 | TResult2> {
    return this.execute().then(onfulfilled, onrejected);
  }

  #with(patch: Partial<QueryPlan>): QueryBuilder<RowType> {
    return new QueryBuilder(this.#executor, {...this.#plan, ...patch});
  }
}

function assertNonNegativeInteger(value: number, description: string): void {
  if (
    !Number.isSafeInteger(value) ||
    value < 0 ||
    value > MAX_QUERY_POSITION
  ) {
    throw new ClientError({
      code: 'INVALID_QUERY',
      message: `A query ${description} must be an integer between 0 and ${MAX_QUERY_POSITION}`,
    });
  }
}

function assertQueryWindow(
  offset: number | undefined,
  limit: number | undefined,
): void {
  if (
    offset !== undefined &&
    limit !== undefined &&
    offset + limit > MAX_QUERY_POSITION
  ) {
    throw new ClientError({
      code: 'INVALID_QUERY',
      message: `A query offset plus limit cannot exceed ${MAX_QUERY_POSITION}`,
    });
  }
}

function parseProjection(columns: string): string[] | undefined {
  if (columns.trim() === '*') {
    return undefined;
  }
  const projection = columns.split(',').map((column) => column.trim());
  if (projection.length === 0 || projection.some((column) => !column)) {
    throw new ClientError({
      code: 'INVALID_QUERY',
      message: 'A projection must contain one or more column names',
    });
  }
  return projection;
}
