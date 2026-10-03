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
