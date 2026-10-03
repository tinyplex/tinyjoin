import {
  type CompiledQuery,
  type DatabaseConnection,
  type DatabaseIntrospector,
  type DatabaseMetadataOptions,
  type Dialect,
  type Driver,
  type MigrationLockOptions,
  PostgresAdapter,
  PostgresQueryCompiler,
  type QueryResult,
  type SchemaMetadata,
  type TableMetadata,
  type TransactionSettings,
} from 'kysely';

import type {Client, ColumnType, JsonValue, Transaction} from '../index.js';

export interface TinyJoinDialectConfig {
  client: Client;
}

// Kysely's own names for the tables its Migrator keeps, which it asks an
// introspector to leave out unless they are wanted.
const MIGRATION_TABLES = ['kysely_migration', 'kysely_migration_lock'];

// The PostgreSQL types that the Client's results already report for each
// runtime type.
const DATA_TYPES: {[type in ColumnType]: string} = {
  boolean: 'bool',
  integer: 'int8',
  float: 'float8',
  text: 'text',
  json: 'json',
};

const ROLLBACK = Symbol('rollback');

// Each connection runs its queries on the Client, or on the Client's callback
// transaction while it has one open. The callback waits until Kysely commits
// or rolls the transaction back.
class TinyJoinConnection implements DatabaseConnection {
  #transaction: Transaction | undefined;
  #settle: ((rollback: boolean) => void) | undefined;
  #done: Promise<unknown> | undefined;

  constructor(private readonly client: Client) {}

  async executeQuery<R>(compiled: CompiledQuery): Promise<QueryResult<R>> {
    const {rows, affectedRows} = await (
      this.#transaction ?? this.client
    ).query<R>(compiled.sql, compiled.parameters as JsonValue[]);
    return affectedRows === undefined
      ? {rows}
      : {rows, numAffectedRows: BigInt(affectedRows)};
  }

  async *streamQuery<R>(): AsyncIterableIterator<QueryResult<R>> {
    throw new Error('TinyJoin does not stream query results');
  }

  begin(): Promise<void> {
    return new Promise((begun, failed) => {
      this.#done = this.client
        .transaction(
          (transaction) =>
            new Promise((resolve, reject) => {
              this.#transaction = transaction;
              this.#settle = (rollback) =>
                rollback ? reject(ROLLBACK) : resolve(undefined);
              begun();
            }),
        )
        .finally(() => {
          this.#transaction = undefined;
          this.#settle = undefined;
        });
      this.#done.catch(failed);
    });
  }

  async end(rollback: boolean): Promise<void> {
    this.#settle?.(rollback);
    await this.#done?.catch((error: unknown) => {
      if (error !== ROLLBACK) {
        throw error;
      }
    });
    this.#done = undefined;
  }
}

class TinyJoinDriver implements Driver {
  constructor(private readonly client: Client) {}

  async init(): Promise<void> {}

  async acquireConnection(): Promise<DatabaseConnection> {
    return new TinyJoinConnection(this.client);
  }

  async beginTransaction(
    connection: DatabaseConnection,
    settings: TransactionSettings,
  ): Promise<void> {
    if (settings.isolationLevel || settings.accessMode) {
      throw new Error(
        'A TinyJoin transaction takes no isolation level or access mode',
      );
    }
    await (connection as TinyJoinConnection).begin();
  }

  async commitTransaction(connection: DatabaseConnection): Promise<void> {
    await (connection as TinyJoinConnection).end(false);
  }

  async rollbackTransaction(connection: DatabaseConnection): Promise<void> {
    await (connection as TinyJoinConnection).end(true);
  }

  async releaseConnection(): Promise<void> {}

  // The Client is the application's to close.
  async destroy(): Promise<void> {}
}

class TinyJoinAdapter extends PostgresAdapter {
  #release: (() => void) | undefined;

  // Kysely then runs every query on one connection at a time, as TinyJoin runs
  // them, so a query outside a transaction waits for it rather than failing.
  override get supportsMultipleConnections(): boolean {
    return false;
  }

  override get supportsTransactionalDdl(): boolean {
    return false;
  }

  // Each tab of a browser application may migrate the database it shares as it
  // starts, so a Web Lock keeps one migrating at a time. Elsewhere there is no
  // other Client of the database to wait for.
  override async acquireMigrationLock(
    _db: unknown,
    {lockTable}: MigrationLockOptions,
  ): Promise<void> {
    const locks = (
      globalThis as {navigator?: {locks?: LockManager}}
    ).navigator?.locks;
    if (locks) {
      await new Promise<void>((acquired) => {
        void locks.request(
          `tinyjoin:kysely:${lockTable}`,
          () =>
            new Promise<void>((release) => {
              this.#release = release;
              acquired();
            }),
        );
      });
    }
  }

  override async releaseMigrationLock(): Promise<void> {
    this.#release?.();
    this.#release = undefined;
  }
}

class TinyJoinIntrospector implements DatabaseIntrospector {
  constructor(private readonly client: Client) {}

  // TinyJoin has no schemas: a qualified table name is one flat name.
  async getSchemas(): Promise<SchemaMetadata[]> {
    return [];
  }

  async getTables(
    options: DatabaseMetadataOptions = {withInternalKyselyTables: false},
  ): Promise<TableMetadata[]> {
    const {tables} = await this.client.getSchema();
    return tables
      .filter(
        ({name}) =>
          options.withInternalKyselyTables || !MIGRATION_TABLES.includes(name),
      )
      .map(({name, columns}) => ({
        name,
        isView: false,
        isForeign: false,
        columns: columns.map((column) => ({
          name: column.name,
          dataType:
            column.maxLength === undefined
              ? DATA_TYPES[column.type]
              : 'varchar',
          isNullable: column.nullable,
          isAutoIncrementing: false,
          hasDefaultValue: 'default' in column,
        })),
      }));
  }

  // Kysely 0.28 asks for this too.
  async getMetadata(
    options?: DatabaseMetadataOptions,
  ): Promise<{tables: TableMetadata[]}> {
    return {tables: await this.getTables(options)};
  }
}

/**
 * A Kysely dialect that runs Kysely's PostgreSQL queries on a TinyJoin Client.
 */
export class TinyJoinDialect implements Dialect {
  readonly #client: Client;

  constructor(config: TinyJoinDialectConfig) {
    this.#client = config.client;
  }

  createDriver(): Driver {
    return new TinyJoinDriver(this.#client);
  }

  createQueryCompiler(): PostgresQueryCompiler {
    return new PostgresQueryCompiler();
  }

  createAdapter(): PostgresAdapter {
    return new TinyJoinAdapter();
  }

  createIntrospector(): DatabaseIntrospector {
    return new TinyJoinIntrospector(this.#client);
  }
}
