import type {JsonValue, QueryPlan, Row} from '../protocol.js';
import {TinygresError} from './error.js';

export type QueryResponse<RowType extends object> =
  | {data: RowType[]; error: null; revision: number}
  | {data: null; error: TinygresError; revision?: number};

export interface QueryExecutor {
  executePlan(plan: QueryPlan): Promise<{
    rows: Row[];
    revision: number;
  }>;
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
    if (!column.trim()) {
      throw new TinygresError({
        code: 'INVALID_QUERY',
        message: 'A filter column cannot be empty',
      });
    }
    return this.#with({
      filters: [
        ...this.#plan.filters,
        {column, operator: 'eq', value},
      ],
    });
  }

  limit(limit: number): QueryBuilder<RowType> {
    if (!Number.isSafeInteger(limit) || limit < 0) {
      throw new TinygresError({
        code: 'INVALID_QUERY',
        message: 'A query limit must be a non-negative safe integer',
      });
    }
    return this.#with({limit});
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
      return {data: null, error: TinygresError.fromUnknown(error)};
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

function parseProjection(columns: string): string[] | undefined {
  if (columns.trim() === '*') {
    return undefined;
  }
  const projection = columns.split(',').map((column) => column.trim());
  if (projection.length === 0 || projection.some((column) => !column)) {
    throw new TinygresError({
      code: 'INVALID_QUERY',
      message: 'A projection must contain one or more column names',
    });
  }
  return projection;
}
