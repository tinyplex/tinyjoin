import {
  type DrizzleConfig,
  DefaultLogger,
  type Logger,
  Param,
  Placeholder,
  type QueryWithTypings,
  type RelationalSchemaConfig,
  SQL,
  type TablesRelationalConfig,
  createTableRelationsHelpers,
  entityKind,
  extractTablesRelationalConfig,
  is,
} from 'drizzle-orm';
import type {Cache} from 'drizzle-orm/cache/core';
import type {WithCacheConfig} from 'drizzle-orm/cache/core/types';
import {
  type PgColumn,
  PgDialect,
  PgJson,
  PgJsonb,
  PgTable,
  getTableConfig,
} from 'drizzle-orm/pg-core';
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

import type {
  Client,
  ColumnSchema,
  ColumnType,
  ForeignKeySchema,
  IndexSchema,
  JsonValue,
  TableSchema,
  Transaction,
} from '../index.js';

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

/** The journal that `drizzle-kit generate` writes as `meta/_journal.json`. */
export interface MigrationJournal {
  entries: {tag: string; when: number}[];
}

export interface MigrationConfig {
  journal: MigrationJournal;
  migrations: Record<string, string>;
  migrationsTable?: string;
}

const quoteIdentifier = (name: string): string =>
  `"${name.replaceAll('"', '""')}"`;

/**
 * Applies each migration in a Drizzle Kit journal that the database has not
 * recorded, in journal order, each atomically with its record.
 */
export async function migrate<
  TSchema extends Record<string, unknown> = Record<string, never>,
>(db: TinyJoinDatabase<TSchema>, config: MigrationConfig): Promise<void> {
  const client = db.$client;
  const table = quoteIdentifier(
    config.migrationsTable ?? '__drizzle_migrations',
  );
  await client.exec(
    `CREATE TABLE IF NOT EXISTS ${table} (tag TEXT PRIMARY KEY, created_at INTEGER NOT NULL)`,
  );
  const applied = new Set(
    (await client.query<{tag: string}>(`SELECT tag FROM ${table}`)).rows.map(
      ({tag}) => tag,
    ),
  );
  const isApplied = async (tag: string): Promise<boolean> =>
    (await client.query(`SELECT tag FROM ${table} WHERE tag = $1`, [tag])).rows
      .length > 0;
  const keys = Object.keys(config.migrations);
  for (const {tag, when} of config.journal.entries) {
    if (applied.has(tag)) {
      continue;
    }
    if (!Number.isSafeInteger(when)) {
      throw new TypeError(`Migration ${tag} has no whole-number time`);
    }
    const files = keys.filter(
      (key) => key === tag || key === `${tag}.sql` || key.endsWith(`/${tag}.sql`),
    );
    if (files.length !== 1) {
      throw new Error(
        `Migration ${tag} matches ${files.length} of the migrations given`,
      );
    }
    // A script is atomic, so the migration and its record commit together. The
    // record goes first, so a migration another Client of the database applied
    // since it was read fails at once, and changes nothing.
    try {
      await client.exec(
        `INSERT INTO ${table} (tag, created_at) VALUES ('${tag.replaceAll("'", "''")}', ${when});\n${config.migrations[files[0]!]}`,
      );
    } catch (error) {
      if (!(await isApplied(tag).catch(() => false))) {
        throw error;
      }
    }
  }
}

export interface PushOptions {
  version?: number;
  drop?: boolean;
  renames?: Record<string, string>;
}

// Each PostgreSQL type a Drizzle column can declare that TinyJoin has.
const COLUMN_TYPES: {[type: string]: ColumnType} = {
  boolean: 'boolean',
  smallint: 'integer',
  integer: 'integer',
  bigint: 'integer',
  real: 'float',
  'double precision': 'float',
  text: 'text',
  varchar: 'text',
  json: 'json',
  jsonb: 'json',
};

const tableSchema = (
  table: PgTable,
  renames: Record<string, string>,
): TableSchema => {
  const config = getTableConfig(table);
  const name = config.schema ? `${config.schema}.${config.name}` : config.name;
  const refuse = (what: string): never => {
    throw new Error(`Table ${name} ${what}, which TinyJoin does not have`);
  };
  if (config.checks.length > 0) {
    refuse('declares check constraints');
  }
  const indexes: IndexSchema[] = [];
  const unique = (
    indexName: string,
    columns: PgColumn[],
    nullsNotDistinct: boolean,
  ): void => {
    if (nullsNotDistinct) {
      refuse(`makes ${indexName} treat NULLs as equal`);
    }
    indexes.push({
      name: indexName,
      columns: columns.map((column) => column.name),
      unique: true,
    });
  };
  const columns = config.columns.map((column): ColumnSchema => {
    const [, sqlType = '', length] =
      /^([a-z ]+)(?:\((\d+)\))?$/.exec(column.getSQLType()) ?? [];
    const type = COLUMN_TYPES[sqlType] ?? refuse(`types ${column.name} as ${column.getSQLType()}`);
    if (column.isUnique) {
      unique(column.uniqueName!, [column], column.uniqueType === 'not distinct');
    }
    // A default Drizzle works out in JavaScript, with $defaultFn, is not the
    // database's.
    if (is(column.default, SQL)) {
      refuse(`defaults ${column.name} with SQL`);
    }
    const renamedFrom = renames[`${name}.${column.name}`];
    return {
      name: column.name,
      type,
      nullable: !column.notNull,
      ...(column.default === undefined
        ? {}
        : {default: column.default as JsonValue}),
      ...(length === undefined ? {} : {maxLength: Number(length)}),
      ...(renamedFrom === undefined ? {} : {renamedFrom}),
    };
  });
  for (const constraint of config.uniqueConstraints) {
    unique(
      constraint.getName() ??
        `${config.name}_${constraint.columns.map(({name}) => name).join('_')}_unique`,
      constraint.columns,
      constraint.nullsNotDistinct,
    );
  }
  for (const {config: index} of config.indexes) {
    const columnNames = index.columns.map((column) => {
      const {name: columnName, indexConfig} = (is(column, SQL) ? {} : column) as {
        name?: string;
        indexConfig?: {order?: string; nulls?: string; opClass?: string};
      };
      return columnName === undefined
        ? refuse('indexes an expression')
        : indexConfig?.order === 'desc' ||
            indexConfig?.nulls === 'first' ||
            indexConfig?.opClass !== undefined
          ? refuse('orders an index or gives it an operator class')
          : columnName;
    });
    if (index.where || (index.method ?? 'btree') !== 'btree') {
      refuse(`makes ${index.name ?? 'an index'} partial or not a B-tree`);
    }
    indexes.push({
      name: index.name ?? `${config.name}_${columnNames.join('_')}_index`,
      columns: columnNames,
      unique: index.unique,
    });
  }
  const foreignKeys = config.foreignKeys.map((key): ForeignKeySchema => {
    const {columns, foreignColumns, foreignTable} = key.reference();
    const parent = getTableConfig(foreignTable);
    return {
      name: key.getName(),
      columns: columns.map((column) => column.name),
      references: parent.schema ? `${parent.schema}.${parent.name}` : parent.name,
      referencedColumns: foreignColumns.map((column) => column.name),
      onDelete: key.onDelete ?? 'no action',
      onUpdate: key.onUpdate ?? 'no action',
    };
  });
  const renamedFrom = renames[name];
  return {
    name,
    columns,
    primaryKey:
      config.primaryKeys[0]?.columns.map((column) => column.name) ??
      config.columns.filter((column) => column.primary).map(({name}) => name),
    indexes,
    foreignKeys,
    ...(renamedFrom === undefined ? {} : {renamedFrom}),
  };
};

/**
 * Makes the Client's database hold the tables of a Drizzle schema, as Drizzle
 * Kit's push would, but in the application and as one atomic change.
 */
export async function push<
  TSchema extends Record<string, unknown> = Record<string, never>,
>(
  db: TinyJoinDatabase<TSchema>,
  schema: Record<string, unknown>,
  {version = 0, drop = false, renames = {}}: PushOptions = {},
): Promise<boolean> {
  const tables = Object.values(schema)
    .filter((value): value is PgTable => is(value, PgTable))
    .map((table) => tableSchema(table, renames));
  return db.$client.setSchema({version, tables}, {drop});
}

