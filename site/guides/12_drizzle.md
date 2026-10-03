# Drizzle

Use `tinyjoin/drizzle` to query a TinyJoin database with the
[Drizzle ORM](https://orm.drizzle.team) query builder. It connects Drizzle to a
TinyJoin Client, runs Drizzle's transactions as TinyJoin transactions, and
binds JSON values as JSON.

Drizzle writes PostgreSQL, and TinyJoin runs a
[bounded, PostgreSQL-shaped dialect](/guides/sql-compatibility/), so most of
Drizzle's query builder works, and some of it does not. This guide says which.

## Get started

Install TinyJoin and Drizzle. TinyJoin declares `drizzle-orm` 0.45 or later as
an optional peer dependency, so an application that does not use Drizzle never
installs it.

```sh
npm install tinyjoin drizzle-orm
```

Describe the tables with Drizzle's PostgreSQL columns, create them with SQL,
and query them through Drizzle:

```ts
import {create} from 'tinyjoin';
import {drizzle} from 'tinyjoin/drizzle';
import {eq, sql} from 'drizzle-orm';
import {boolean, integer, pgTable, text} from 'drizzle-orm/pg-core';

const tasks = pgTable('tasks', {
  id: text('id').primaryKey(),
  title: text('title').notNull(),
  done: boolean('done').notNull().default(false),
  edits: integer('edits').notNull().default(0),
});

const client = await create('opfs://my-app-v1');
await client.exec(`
  CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false,
    edits INTEGER NOT NULL DEFAULT 0
  )
`);

const db = drizzle(client, {schema: {tasks}});
await db.insert(tasks).values({id: crypto.randomUUID(), title: 'Try Drizzle'});
await db
  .update(tasks)
  .set({title: 'Tried Drizzle', edits: sql`${tasks.edits} + 1`})
  .where(eq(tasks.done, false));
const open = await db.select().from(tasks).where(eq(tasks.done, false));
```

The Client is still yours: subscribe to it for changes, and close it when the
database is no longer needed. `db.$client` returns it.

## Schemas

Every table needs a primary key. Drizzle's columns map onto TinyJoin's five
runtime types:

| Drizzle column | TinyJoin type |
| --- | --- |
| `boolean` | Boolean |
| `integer`, `smallint`, `bigint({mode: 'number'})` | Integer, within the JavaScript-safe range |
| `real`, `doublePrecision` | Float |
| `text`, `varchar` without a length | Text |
| `json`, `jsonb` | JSON |

TinyJoin has no `serial`, `uuid`, `timestamp`, `date`, `numeric`, enum, or
array types, and no length modifiers. Generate identifiers in the application,
as `text('id').primaryKey().$defaultFn(() => crypto.randomUUID())` does, and
store a time as an integer or text column, or as a Drizzle `customType` over
one.

Create tables with SQL through the Client, idempotently with
`IF NOT EXISTS`, and version the OPFS name with the schema. Drizzle Kit's
`push`, `pull`, and Studio read PostgreSQL's system catalogs, which TinyJoin
does not have (its Client reads the same facts with getSchema()), and Drizzle's
migrators create a `SERIAL` table in a schema of
their own, so none of them works. The SQL that `drizzle-kit generate` writes
is a useful start, but TinyJoin refuses its foreign keys, table-level `UNIQUE`
constraints, and `USING btree` index methods; write the equivalent
`CREATE UNIQUE INDEX` instead.

## Queries

These work as Drizzle documents them:

- `select`, `insert`, `update`, and `delete`, with `returning`;
- filters such as `eq`, `ne`, `gt`, `and`, `or`, `not`, `isNull`, `like`,
  `ilike`, `between`, and `inArray`, including `inArray` with a subquery;
- comparisons of two columns, and `sql` expressions with `+`, `-`, `*`, `/`,
  `%`, and `||`, in filters, select lists, and `set`;
- `orderBy`, `limit`, and `offset`;
- inner and left joins of up to eight tables, including columns that share a
  name, and table aliases;
- `groupBy` with `count`, `sum`, `avg`, `min`, and `max`, `$count`, and
  `selectDistinct`;
- `onConflictDoNothing` and `onConflictDoUpdate`, assigning `excluded.column`
  in an `sql` template, or an expression of the stored row's columns;
- prepared statements and placeholders;
- relational queries, `db.query.table.findMany()` and `findFirst()`, without
  `with`.

These are refused with an error rather than run:

- relational queries with `with`, which Drizzle writes as lateral joins of
  JSON aggregates;
- SQL functions such as `lower()`, `coalesce()`, and `now()`, casts, and
  `CASE`;
- `having`, window functions, `$with` CTEs, `union`, correlated subqueries,
  and `exists`;
- right, full, and cross joins, `insert ... select`, and `update ... from`;
- JSON operators.

A failed query rejects with Drizzle's `DrizzleQueryError`, whose `cause` is the
TinyJoin [ClientError](/api/tinyjoin/classes/errors/clienterror/) with its
`code`. The [SQL compatibility contract](/guides/sql-compatibility/) is the
exact list of what runs.

## Transactions

Drizzle's `db.transaction` runs on the Client's
[callback transaction](/guides/transactions-and-changes/). It holds the
database, and every tab sharing it, until the callback finishes, so keep it
short. Use the `tx` it passes, not `db`, inside the callback: awaiting `db`
there waits for the transaction to finish, which waits for the callback.

Drizzle's `tx.rollback` discards the transaction's work and rejects with
`TransactionRollbackError`, as does any error the callback throws. A
transaction cannot nest, since TinyJoin has no savepoints, and cannot take an
isolation level or access mode.

## JSON values

Drizzle encodes a value for a `json` or `jsonb` column as JSON text, which
PostgreSQL parses again. TinyJoin binds every parameter as the JavaScript value
it is, so the driver parses that text back first, and the column stores the
object rather than a string. Reading the column returns the object.

Drizzle's raw `db.execute` resolves to the rows its statement returns, as
objects.
