import {
  type DrizzleConfig,
  DefaultLogger,
  type Logger,
  Param,
  Placeholder,
  type QueryWithTypings,
  type RelationalSchemaConfig,
  type TablesRelationalConfig,
  createTableRelationsHelpers,
  entityKind,
  extractTablesRelationalConfig,
  is,
} from 'drizzle-orm';
import type {Cache} from 'drizzle-orm/cache/core';
import type {WithCacheConfig} from 'drizzle-orm/cache/core/types';
import {PgDialect, PgJson, PgJsonb} from 'drizzle-orm/pg-core';
import type {
  PgTransactionConfig,
  PreparedQueryConfig,
  SelectedFieldsOrdered,
} from 'drizzle-orm/pg-core';
import {
  PgProxyTransaction,
  PgRemoteDatabase,
  PgRemoteSession,
} from 'drizzle-orm/pg-proxy';

import type {Client, JsonValue, Transaction} from '../index.js';

export type TinyJoinDatabase<
  TSchema extends Record<string, unknown> = Record<string, never>,
> = PgRemoteDatabase<TSchema> & {$client: Client};

type Queryable = Client | Transaction;

interface SessionOptions {
  logger?: Logger | undefined;
  cache?: Cache | undefined;
}

// Drizzle's JSON columns encode a value as JSON text, which PostgreSQL parses
// again for the column. TinyJoin binds each parameter as the JavaScript value it
// is, so the text would be stored as a JSON string: the session parses it back.
const query =
  (target: Queryable) =>
  async (
    sql: string,
    params: unknown[],
    method: 'all' | 'execute',
    typings?: unknown[],
  ): Promise<{rows: unknown[]}> => {
    const values = params.map((value, index) =>
      typings?.[index] === 'json' && typeof value === 'string'
        ? (JSON.parse(value) as JsonValue)
        : (value as JsonValue),
    );
    const result = await target.query(
      sql,
      values,
      method === 'all' ? {rowMode: 'array'} : {},
    );
    return {rows: result.rows};
  };

class TinyJoinSession<
  TFullSchema extends Record<string, unknown>,
  TSchema extends TablesRelationalConfig,
> extends PgRemoteSession<TFullSchema, TSchema> {
  static override readonly [entityKind]: string = 'TinyJoinSession';

  constructor(
    private readonly target: Queryable,
    dialect: PgDialect,
    private readonly relations: RelationalSchemaConfig<TSchema> | undefined,
    private readonly options: SessionOptions,
  ) {
    super(query(target), dialect, relations, {
      ...(options.logger ? {logger: options.logger} : {}),
      ...(options.cache ? {cache: options.cache} : {}),
    });
  }

  // A parameter that fills a placeholder later is typed as nothing at all, even
  // for a JSON column, so its typing is found from the column that encodes it.
  override prepareQuery<T extends PreparedQueryConfig>(
    query: QueryWithTypings,
    fields: SelectedFieldsOrdered | undefined,
    name: string | undefined,
    isResponseInArrayMode: boolean,
    customResultMapper?: (rows: unknown[][]) => T['execute'],
    queryMetadata?: {
      type: 'select' | 'update' | 'delete' | 'insert';
      tables: string[];
    },
    cacheConfig?: WithCacheConfig,
  ) {
    const typings = query.params.map((param, index) =>
      is(param, Param) &&
      is(param.value, Placeholder) &&
      (is(param.encoder, PgJsonb) || is(param.encoder, PgJson))
        ? 'json'
        : (query.typings?.[index] ?? 'none'),
    );
    return super.prepareQuery<T>(
      {...query, typings},
      fields,
      name,
      isResponseInArrayMode,
      customResultMapper,
      queryMetadata,
      cacheConfig,
    );
  }

  override async transaction<T>(
    transaction: (tx: TinyJoinTransaction<TFullSchema, TSchema>) => Promise<T>,
    config?: PgTransactionConfig,
  ): Promise<T> {
    if (config !== undefined) {
      throw new Error(
        'A TinyJoin transaction takes no isolation level, access mode, or deferral',
      );
    }
    if (!('transaction' in this.target)) {
      return nestedTransaction();
    }
    return this.target.transaction((tx) =>
      transaction(
        new TinyJoinTransaction(
          this.dialect,
          new TinyJoinSession(tx, this.dialect, this.relations, this.options),
          this.relations,
        ),
      ),
    );
  }
}

class TinyJoinTransaction<
  TFullSchema extends Record<string, unknown>,
  TSchema extends TablesRelationalConfig,
> extends PgProxyTransaction<TFullSchema, TSchema> {
  static override readonly [entityKind]: string = 'TinyJoinTransaction';

  override transaction<T>(): Promise<T> {
    return nestedTransaction();
  }
}

const nestedTransaction = (): never => {
  throw new Error(
    'TinyJoin transactions do not nest: there are no savepoints. Use the transaction you have.',
  );
};

/**
 * Creates a Drizzle database over a TinyJoin Client, with TinyJoin's callback
 * transactions and JSON values.
 */
export function drizzle<
  TSchema extends Record<string, unknown> = Record<string, never>,
>(
  client: Client,
  config: DrizzleConfig<TSchema> = {},
): TinyJoinDatabase<TSchema> {
  const dialect = new PgDialect(
    config.casing ? {casing: config.casing} : undefined,
  );
  const logger =
    config.logger === true
      ? new DefaultLogger()
      : config.logger === false
        ? undefined
        : config.logger;
  let relations: RelationalSchemaConfig<TablesRelationalConfig> | undefined;
  if (config.schema) {
    const tables = extractTablesRelationalConfig(
      config.schema,
      createTableRelationsHelpers,
    );
    relations = {
      fullSchema: config.schema,
      schema: tables.tables,
      tableNamesMap: tables.tableNamesMap,
    };
  }
  const session = new TinyJoinSession(client, dialect, relations, {
    logger,
    cache: config.cache,
  });
  const db = new PgRemoteDatabase<TSchema>(
    dialect,
    session as never,
    relations as never,
  ) as TinyJoinDatabase<TSchema>;
  db.$client = client;
  if (config.cache) {
    db.$cache = Object.assign(config.cache, {invalidate: config.cache.onMutate});
  }
  return db;
}
