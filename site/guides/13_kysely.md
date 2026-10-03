# Kysely

Use `tinyjoin/kysely` to query a TinyJoin database with the
[Kysely](https://kysely.dev) query builder. Its dialect runs Kysely's queries
on a TinyJoin Client, runs Kysely's transactions as TinyJoin transactions, and
lets Kysely's introspector and Migrator read and change the schema.

Kysely writes PostgreSQL, and TinyJoin runs a
[bounded, PostgreSQL-shaped dialect](/guides/sql-compatibility/), so most of
Kysely's query builder works, and some of it does not. This guide says which.

## Get started

Install TinyJoin and Kysely. TinyJoin declares `kysely` 0.28 or later as an
optional peer dependency, so an application that does not use Kysely never
installs it.

```sh
npm install tinyjoin kysely
```

Describe the tables as a Kysely database interface, and pass the Client to the
dialect:

```ts
import {create} from 'tinyjoin';
import {TinyJoinDialect} from 'tinyjoin/kysely';
import {Kysely} from 'kysely';

interface Database {
  tasks: {id: string; title: string; done: boolean; edits: number};
}

const client = await create('opfs://my-app-v1');
await client.exec(`
  CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title VARCHAR(200) NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false,
    edits INTEGER NOT NULL DEFAULT 0
  )
`);

const db = new Kysely<Database>({dialect: new TinyJoinDialect({client})});
await db
  .insertInto('tasks')
  .values({id: crypto.randomUUID(), title: 'Try Kysely', done: false, edits: 0})
  .execute();
await db
  .updateTable('tasks')
  .set((eb) => ({edits: eb('edits', '+', 1)}))
  .where('done', '=', false)
  .execute();
const open = await db.selectFrom('tasks').selectAll().where('done', '=', false).execute();
```

The Client is still yours: subscribe to it for changes, and close it when the
database is no longer needed. Kysely's `db.destroy` leaves it open.

## Schemas and migrations

Every table needs a primary key. Kysely's column types map onto TinyJoin's
five runtime types:

| Kysely data type | TinyJoin type |
| --- | --- |
| `boolean` | Boolean |
| `integer`, `bigint` | Integer, within the JavaScript-safe range |
| `real`, `double precision` | Float |
| `text`, `varchar`, `varchar(n)` | Text |
| `json`, `jsonb` | JSON |

TinyJoin has no `serial`, `uuid`, `timestamp`, `date`, `numeric`, enum, or
array types. Generate identifiers in the application, and store a time as an
integer or text column. Leave out `references` and foreign-key constraints:
TinyJoin cannot enforce them, so it refuses the SQL that declares them.

Kysely's schema builder works within TinyJoin's
[DDL](/guides/sql-compatibility/#statements-and-clauses): creating and dropping
tables and indexes, with primary-key and unique constraints, and altering
tables to add, rename, and drop columns, change defaults and nullability, and
rename the table.

Kysely's `Migrator` works as Kysely documents it. It creates its own tables,
then runs each migration that the database has not recorded. TinyJoin cannot
run DDL inside a transaction, so, as on MySQL, Kysely runs a migration's
statements one at a time: a migration that fails partway leaves the statements
before the failure applied. Write migrations whose statements each stand on
their own, or that an `IF NOT EXISTS` makes safe to run again. A Web Lock keeps
one tab of an application migrating at a time, so every tab can migrate as it
starts.

Instead of migrations, an application can declare its schema and pass it to
the Client's [setSchema()](/guides/storage-and-lifecycle/#setting-the-schema)
as it starts.

`db.introspection.getTables` lists the tables, without the Migrator's own,
from the Client's getSchema(), with each column's PostgreSQL type: `bool`,
`int8`, `float8`, `text`, `varchar`, or `json`. TinyJoin has no schemas, so
`getSchemas` lists none.

## Queries

These work as Kysely documents them:

- `selectFrom` with `select` and `selectAll`, including `selectAll('table')`
  over a join;
- `where` with comparisons, `and`, `or`, `not`, `in` with values or a
  subquery, `between`, `like`, `ilike`, and `is null`;
- `orderBy`, `limit`, and `offset`;
- inner and left joins of up to eight tables;
- `groupBy` with `countAll`, `count`, `sum`, `avg`, `min`, and `max`;
- `insertInto` with `values`, `defaultValues`, and `onConflict`, with
  `doNothing` or `doUpdateSet`;
- `updateTable` and `deleteFrom`, with `returning` and `returningAll`, and
  arithmetic and `||` in `set`;
- `sql` templates in TinyJoin's dialect.

These are refused with an error rather than run:

- `selectAll()` over a join whose tables share a column name, such as `id`,
  since Kysely reads rows as objects, which cannot hold both;
- `jsonArrayFrom` and `jsonObjectFrom`, which Kysely writes as correlated JSON
  subqueries, `exists`, and other correlated subqueries;
- SQL functions such as `coalesce`, `case`, and casts;
- `distinctOn`, `having`, a distinct `count`, `with`, and `union`;
- `updateTable(...).from` and `deleteFrom(...).using`;
- streaming with `stream`.

A failed query rejects with the TinyJoin
[ClientError](/api/tinyjoin/classes/errors/clienterror/), with its `code`.
The [SQL compatibility contract](/guides/sql-compatibility/) is the exact list
of what runs.

## Transactions

Kysely's `db.transaction` and `db.startTransaction` run on the Client's
[callback transaction](/guides/transactions-and-changes/). It holds the
database, and every tab sharing it, until it commits or rolls back, so keep it
short. A transaction takes no isolation level or access mode, and there are no
savepoints.

Kysely runs one query at a time on the Client, so a query outside a
transaction waits until the transaction ends rather than failing. Use the
transaction Kysely passes inside the callback: a query on `db` there waits for
the transaction to finish, which waits for the callback. Kysely 0.28 does not
run queries one at a time, so there a query outside an open transaction fails
with `TRANSACTION_ACTIVE` instead.

## Results

Rows come back as objects. An integer is a JavaScript number, and a `json`
column holds the value itself, not JSON text, so pass objects rather than
`JSON.stringify` output. Updates and deletes report `numUpdatedRows` and
`numDeletedRows` as Kysely does, as `bigint` values.
