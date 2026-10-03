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
| `text`, `varchar`, with or without a length | Text |
| `json`, `jsonb` | JSON |

TinyJoin has no `serial`, `uuid`, `timestamp`, `date`, `numeric`, enum, or
array types. Generate identifiers in the application,
as `text('id').primaryKey().$defaultFn(() => crypto.randomUUID())` does, and
store a time as an integer or text column, or as a Drizzle `customType` over
one.

Leave `.references()` out of the schema: TinyJoin cannot enforce foreign keys,
so it refuses the SQL that declares them. Drizzle's relations need no foreign
keys.

Create the tables with SQL through the Client, idempotently with
`IF NOT EXISTS`; push the schema from the application, as the next section
describes; or apply the migrations that Drizzle Kit generates. Drizzle Kit's
own `push`, `pull`, and Studio read PostgreSQL's system catalogs, which TinyJoin
does not have; the Client reads the same facts with getSchema().

## Push

push() from `tinyjoin/drizzle` makes the database hold the tables of a Drizzle
schema, as Drizzle Kit's `push` would, but from the application itself and as
one change that commits whole or not at all. There are no migration files to
generate or bundle:

```ts
import {create} from 'tinyjoin';
import {drizzle, push} from 'tinyjoin/drizzle';
import * as schema from './schema';

const db = drizzle(await create('opfs://my-app-v1'), {schema});
await push(db, schema, {version: 2, renames: {'tasks.name': 'title'}});
```

push() reads each table's columns, primary key, unique constraints, and
indexes from Drizzle, and passes them to the Client's
[setSchema()](/guides/storage-and-lifecycle/#setting-the-schema). It creates
the tables and columns the database lacks, changes defaults, nullability, and
`varchar` lengths to the schema's, and makes each table's indexes the schema's,
keeping every row. A schema the database already holds changes nothing, so
every tab can call push() as it starts.

A rename is not something a schema can show, so name each one in `renames`,
from the new name, as `table` or `table.column`, to the old one; otherwise the
new name is created empty. Tables and columns the schema leaves out stay unless
push() is given `drop: true`. Give each change of schema a higher `version`:
push() then refuses an older schema, from a tab still running an older copy of
the application, with `SCHEMA_OUTDATED`, rather than undoing the change.

A table that declares what TinyJoin cannot hold, such as a foreign key, a
`timestamp` column, an `sql` default, or a descending index, is refused before
anything changes. A default from `$defaultFn` is Drizzle's to fill in, so the
database has none.

Use push() when the schema in code is the record of truth, and
[migrations](#migrations) when the history of changes, or data moved between
them, matters too. Keep to one of the two for a database.

## Migrations

`drizzle-kit generate` compares the schema with the last one it saw and writes
the SQL that turns one into the other, with a journal of every migration so
far. Configure it for PostgreSQL:

```ts
// drizzle.config.ts
import {defineConfig} from 'drizzle-kit';

export default defineConfig({
  dialect: 'postgresql',
  schema: './src/schema.ts',
  out: './drizzle',
});
```

Drizzle's own migrators read those files from disk and record them in a
`SERIAL` table of a schema of their own, so they cannot run here. migrate()
from `tinyjoin/drizzle` applies the same migrations from SQL that the
application bundles. With Vite:

```ts
import {create} from 'tinyjoin';
import {drizzle, migrate} from 'tinyjoin/drizzle';
import journal from '../drizzle/meta/_journal.json';
import * as schema from './schema';

const db = drizzle(await create('opfs://my-app-v1'), {schema});
await migrate(db, {
  journal,
  migrations: import.meta.glob<string>('../drizzle/*.sql', {
    query: '?raw',
    import: 'default',
    eager: true,
  }),
});
```

migrate() applies, in the journal's order, each migration that the database
has not recorded, and records it in a `__drizzle_migrations` table. A migration
runs as one script together with its record, so it commits whole or not at
all: one that fails rejects with its error and leaves the database as the
migration before it left it. Every tab can call migrate() as it starts, since
a migration that another tab applied first is skipped. A migration must fit in
one script, of at most 255 statements and 1 MiB of SQL.

The SQL that `drizzle-kit generate` writes runs as it is: new tables with their
`UNIQUE` constraints, composite primary keys, `USING btree` indexes, and JSON
defaults; added, renamed, and dropped columns, tables, indexes, and
constraints; and changed defaults and nullability. A column's type can change
only between spellings of one runtime type, such as `integer` and `bigint`,
since TinyJoin cannot convert stored values. Dropping a column, changing its
default, or making it `NOT NULL` rewrites every row of its table, within the
work limits of one script.

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
