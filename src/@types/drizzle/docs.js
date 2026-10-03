/**
 * The drizzle module connects the Drizzle ORM query builder to a TinyJoin
 * Client. It needs `drizzle-orm` 0.45 or later, which the application installs
 * itself: TinyJoin declares it as an optional peer dependency and never bundles
 * it.
 *
 * Drizzle writes PostgreSQL, so its queries run in TinyJoin's
 * [SQL dialect](/guides/sql-compatibility/), and its schemas use the column
 * types that dialect has. See the [Drizzle guide](/guides/drizzle/) for what
 * works, and what does not.
 * @packageDocumentation
 * @module drizzle
 * @since v0.5.0
 */
/// drizzle

/**
 * The TinyJoinDatabase type is the Drizzle database the drizzle function
 * returns: Drizzle's PostgreSQL database with the TinyJoin Client it queries as
 * its `$client`.
 *
 * Drizzle's raw `db.execute` resolves to the rows its statement returns, as
 * objects.
 * @category Drizzle
 * @since v0.5.0
 */
/// TinyJoinDatabase

/**
 * The drizzle function creates a Drizzle database over a TinyJoin Client.
 *
 * Queries run on the Client as array rows, which Drizzle maps to its own
 * result shapes. Drizzle's `db.transaction` runs on the Client's callback
 * transaction, so it holds the database as any TinyJoin transaction does,
 * `tx.rollback` discards its work, and a transaction cannot nest, since there
 * are no savepoints, or take an isolation level.
 *
 * A value Drizzle encodes for a `json` or `jsonb` column is bound as the JSON
 * value it is, rather than as the JSON text Drizzle makes of it, which TinyJoin
 * would store as a string.
 *
 * Close the Client yourself when the database is no longer needed.
 * @param client The TinyJoin Client to query, from create().
 * @param config Drizzle's configuration: its schema, for relational queries,
 * and its logger, casing, and cache.
 * @returns A Drizzle database.
 * @example
 * ```ts
 * import {create} from 'tinyjoin';
 * import {drizzle} from 'tinyjoin/drizzle';
 * import {eq} from 'drizzle-orm';
 * import {boolean, pgTable, text} from 'drizzle-orm/pg-core';
 *
 * const tasks = pgTable('tasks', {
 *   id: text('id').primaryKey(),
 *   title: text('title').notNull(),
 *   done: boolean('done').notNull().default(false),
 * });
 *
 * const client = await create('opfs://my-app-v1');
 * const db = drizzle(client, {schema: {tasks}});
 * await db.insert(tasks).values({id: crypto.randomUUID(), title: 'Try Drizzle'});
 * const open = await db.select().from(tasks).where(eq(tasks.done, false));
 * ```
 * @category Drizzle
 * @since v0.5.0
 */
/// drizzle.drizzle

/**
 * The MigrationJournal interface is the journal that `drizzle-kit generate`
 * writes beside its migrations, as `meta/_journal.json`, of which the migrate
 * function reads each entry's tag and time.
 * @category Migrations
 * @since v0.5.0
 */
/// MigrationJournal
{
  /**
   * The entries property lists the migrations in the order to apply them, each
   * with its tag, which names its SQL file, and the time it was generated.
   * @category Migrations
   * @since v0.5.0
   */
  /// MigrationJournal.entries
}

/**
 * The MigrationConfig interface describes the migrations that the migrate
 * function applies.
 * @category Migrations
 * @since v0.5.0
 */
/// MigrationConfig
{
  /**
   * The journal property is the Drizzle Kit journal, which an application can
   * import from `meta/_journal.json`.
   * @category Migrations
   * @since v0.5.0
   */
  /// MigrationConfig.journal

  /**
   * The migrations property maps each migration's file to its SQL text. A key
   * may be the migration's tag, its file name, or any path ending in its file
   * name, so the object that a bundler's glob import returns can be passed as
   * it is.
   * @category Migrations
   * @since v0.5.0
   */
  /// MigrationConfig.migrations

  /**
   * The migrationsTable property names the table that records the migrations
   * applied, `__drizzle_migrations` unless it is given.
   * @category Migrations
   * @since v0.5.0
   */
  /// MigrationConfig.migrationsTable
}

/**
 * The migrate function applies the migrations that `drizzle-kit generate`
 * wrote, which the database has not yet recorded, in the journal's order.
 *
 * Each migration runs as one script together with the row that records it, so
 * it commits whole or not at all, and a migration that fails leaves the
 * database as the previous one left it, and rejects with its error. A
 * migration another Client of the same database applied first is skipped, so
 * every tab of an application can call migrate as it starts.
 *
 * A migration is recorded by its tag and applied once: a file changed after it
 * was applied is not applied again, and a migration that joins the journal
 * before others already applied is still applied. The migrations need no file
 * system, so a browser application bundles them.
 * @param db The Drizzle database, from drizzle.
 * @param config The journal and the SQL text of each migration.
 * @returns A Promise that resolves when every migration has been applied.
 * @example
 * ```ts
 * import {create} from 'tinyjoin';
 * import {drizzle, migrate} from 'tinyjoin/drizzle';
 * import journal from './drizzle/meta/_journal.json';
 *
 * const db = drizzle(await create('opfs://my-app-v1'));
 * await migrate(db, {
 *   journal,
 *   migrations: import.meta.glob<string>('./drizzle/*.sql', {
 *     query: '?raw',
 *     import: 'default',
 *     eager: true,
 *   }),
 * });
 * ```
 * @category Migrations
 * @since v0.5.0
 */
/// drizzle.migrate
