/**
 * The kysely module connects the Kysely query builder to a TinyJoin Client. It
 * needs `kysely` 0.28 or later, which the application installs itself:
 * TinyJoin declares it as an optional peer dependency and never bundles it.
 *
 * Kysely writes PostgreSQL, so its queries run in TinyJoin's
 * [SQL dialect](/guides/sql-compatibility/). See the
 * [Kysely guide](/guides/kysely/) for what works, and what does not.
 * @packageDocumentation
 * @module kysely
 * @since v0.5.0
 */
/// kysely

/**
 * The TinyJoinDialectConfig interface configures a TinyJoinDialect.
 * @category Kysely
 * @since v0.5.0
 */
/// TinyJoinDialectConfig
{
  /**
   * The client property is the TinyJoin Client, from create(), on which
   * Kysely runs its queries. The application closes it: Kysely's `destroy`
   * leaves it open.
   * @category Kysely
   * @since v0.5.0
   */
  /// TinyJoinDialectConfig.client
}

/**
 * The TinyJoinDialect class is a Kysely dialect that runs Kysely's PostgreSQL
 * queries on a TinyJoin Client.
 *
 * Kysely runs one query at a time on the Client, so a query outside a
 * transaction waits until the transaction ends rather than failing. Kysely's
 * `db.transaction` runs on the Client's callback transaction, which holds the
 * database, and every tab sharing it, until it commits or rolls back. It takes
 * no isolation level or access mode, and there are no savepoints.
 *
 * Its introspector reads the Client's getSchema, and Kysely's Migrator runs
 * each migration's statements in turn, since TinyJoin runs DDL outside
 * transactions, with a Web Lock keeping one tab migrating at a time.
 * @example
 * ```ts
 * import {create} from 'tinyjoin';
 * import {TinyJoinDialect} from 'tinyjoin/kysely';
 * import {Kysely} from 'kysely';
 *
 * interface Database {
 *   tasks: {id: string; title: string; done: boolean};
 * }
 *
 * const client = await create('opfs://my-app-v1');
 * const db = new Kysely<Database>({dialect: new TinyJoinDialect({client})});
 * const open = await db
 *   .selectFrom('tasks')
 *   .select(['id', 'title'])
 *   .where('done', '=', false)
 *   .execute();
 * ```
 * @category Kysely
 * @since v0.5.0
 */
/// TinyJoinDialect
{
  /**
   * The constructor creates a dialect over a TinyJoin Client.
   * @param config The dialect's configuration, with its Client.
   * @category Kysely
   * @since v0.5.0
   */
  /// TinyJoinDialect.constructor

  /**
   * The createDriver method creates the driver that runs Kysely's queries on
   * the Client. Kysely calls it.
   * @returns A Kysely driver.
   * @category Kysely
   * @since v0.5.0
   */
  /// TinyJoinDialect.createDriver

  /**
   * The createQueryCompiler method creates Kysely's PostgreSQL query compiler.
   * Kysely calls it.
   * @returns A Kysely query compiler.
   * @category Kysely
   * @since v0.5.0
   */
  /// TinyJoinDialect.createQueryCompiler

  /**
   * The createAdapter method creates the adapter that tells Kysely how
   * TinyJoin differs from PostgreSQL. Kysely calls it.
   * @returns A Kysely dialect adapter.
   * @category Kysely
   * @since v0.5.0
   */
  /// TinyJoinDialect.createAdapter

  /**
   * The createIntrospector method creates the introspector that lists the
   * database's tables and columns from the Client's getSchema. Kysely calls it.
   * @returns A Kysely database introspector.
   * @category Kysely
   * @since v0.5.0
   */
  /// TinyJoinDialect.createIntrospector
}
