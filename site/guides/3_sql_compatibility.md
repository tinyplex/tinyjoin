# SQL compatibility

TinyJoin implements its own deliberately bounded, PostgreSQL-shaped SQL
dialect. It is not PostgreSQL compiled to WebAssembly, a PostgreSQL server, or
a general PostgreSQL replacement. Familiar syntax is used where the smaller
runtime can give it clear and deterministic semantics.

This document is the compatibility contract for the current dialect. A form
not listed as supported here is unsupported, even if its keywords happen to be
accepted by PostgreSQL. Unsupported forms fail explicitly rather than being
silently reinterpreted.

## JavaScript entry point

SQL is the primary relational interface. The basic lifecycle has four calls:

```ts
import { create } from "tinyjoin";

const db = await create();

await db.exec(`
  CREATE TABLE tasks (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  );
`);
await db.query("INSERT INTO tasks (id, title) VALUES ($1, $2)", [
  1,
  "Write the compatibility contract",
]);

const { rows } = await db.query<{ id: number; title: string }>(
  "SELECT id, title FROM tasks WHERE done = $1",
  [false],
);

await db.close();
```

Calling create() opens the Worker-backed database and resolves after
initialization. With no argument, or `memory://`, storage is ephemeral. A named
`opfs://database-name` data directory opts into persistent browser storage.
The client also exposes read-only `ready`, `waitReady`, and `closed`
properties.

query(sql, params?, options?) executes one read or write statement with
optional JSON-compatible `$1` parameters. exec(sql, options?) executes one or
more statements without parameters as one implicit transaction and returns one
result per statement. Both use `{rows, fields, affectedRows?, command?,
rowCount?}` results; TinyJoin adds `revision` and `tables`. `fields` contains
ordered `{name, dataTypeID}` entries, including for empty typed results. Each
object row lists its keys in that field order, and `rowMode: "array"` returns
its values in that order. The `sql` tagged template
is a parameterizing form of query(). `rowMode` is the only query option
implemented today; parser, serializer, notice, parameter-type, and blob
options are rejected. The tag accepts parameter values only and does not
provide raw-SQL, identifier, or nested-template helpers.
close() is asynchronous and idempotent.

Interactive atomicity uses transaction(callback), not SQL transaction
statements:

```ts
await db.transaction(async (tx) => {
  await tx.query("UPDATE tasks SET done = true WHERE id = $1", [1]);
  await tx.query("INSERT INTO tasks (id, title) VALUES ($1, $2)", [
    2,
    "Committed together",
  ]);
});
```

Invalidation subscriptions and Worker/storage configuration form the small
JavaScript control surface around the SQL-first engine; relational reads,
writes, and schema changes use SQL.

## Prepared statements

Use a prepared statement when the same parameterized read or row mutation will
execute repeatedly:

```ts
const tasksByDone = await db.prepare<{
  id: number;
  title: string;
  done: boolean;
}>("SELECT id, title, done FROM tasks WHERE done = $1 ORDER BY id");
const setTaskDone = await db.prepare(
  "UPDATE tasks SET done = $1 WHERE id = $2",
);

const { rows } = await tasksByDone.execute([false]);

await db.transaction(async (tx) => {
  await tx.execute(setTaskDone, [true, 1]);
  console.log((await tx.execute(tasksByDone, [true])).rows);
});

await tasksByDone.close();
await setTaskDone.close();
```

prepare<Row>(sql) parses and retains one `SELECT`, aggregate, join, `INSERT`,
`UPDATE`, or `DELETE` statement in the Worker. It rejects DDL, an empty string,
and more than one statement. The returned PreparedStatement<Row> has
execute(params?, options?), asynchronous idempotent close(), and a read-only
`closed` property. execute() returns the same Results<Row> shape as
query() and accepts the same sole option, `rowMode`.

The prepared parameter count is the highest referenced `$n`. Every execution
must provide exactly that many JSON-compatible values; a numbering gap still
occupies a slot. Binding or execution failure leaves the handle open for a later
valid execution. exec() remains parameter-free and is not a prepared-script
API.

Preparation retains parsed syntax and parameter positions, not a schema
snapshot. Every execution resolves tables and columns and validates types
against the current catalog and, inside a transaction, its current staged view.
A compatible DDL change is transparent. An incompatible change returns the
ordinary current table, column, constraint, or type error and does not make the
handle permanently stale. Consequently, `SELECT *` or `RETURNING *` can expose
new fields after `ALTER TABLE ... ADD COLUMN`, including a new positional value
in array row mode. Use an explicit projection when callers require a stable
result shape. As with query<Row>(), the Row generic is a compile-time cast,
not runtime result validation.

A prepared statement is session-local: it belongs to the client that created
it, is not stored in OPFS, cannot be used by another client, and does not survive
db.close() or a Worker restart. Prepare handles before entering a callback
transaction. Inside the callback, use
tx.execute(statement, params?, options?); it accepts only an open statement
from the same client and participates in the same staged commit or rollback as
tx.query(). Direct db.prepare(), statement.execute(), and
statement.close() calls are blocked while that client's transaction callback
is active.

Calling statement.close() seals it immediately, rejects new executions, waits
for executions that already started, and then releases its Worker resources.
Concurrent close calls share the same cleanup, and a cleanup failure does not
reopen the handle. Closing the database seals and releases every remaining
prepared statement. At most 128 handles and 8 MiB of conservatively accounted
prepared-statement state may be retained by one open database.

## How to read the matrices

- **Supported** means the exact form described here is implemented and tested.
- **Narrow** means TinyJoin implements a useful but intentionally smaller form
  than PostgreSQL.
- **No** means the form is rejected.

These labels do not claim compatibility with a particular PostgreSQL release.

## Statements and clauses

| Keyword or form | Status | TinyJoin form and boundary |
| --- | --- | --- |
| `SELECT ... FROM` | Narrow | One table, an aggregate over one table, or a left-deep join over two to eight typed table sources. A single-table projection is `*` or a list of columns and [expressions](#expressions), each optionally renamed with `AS`. A statement over one table may [qualify its columns](#qualified-columns) with that table's name or alias. Join projections list columns and expressions explicitly: neither `*` nor `table.*` is supported. Output names must be distinct unless the rows are [read as arrays](#repeated-output-names): otherwise duplicates return `INVALID_QUERY`, including for empty results and `LIMIT 0`. There is no `SELECT` without `FROM`. |
| `WHERE` | Supported | Predicates described below, with SQL three-valued null logic. |
| `ORDER BY` | Narrow | Up to 32 plain or [qualified](#qualified-columns) columns or projected output names for simple queries, projected output names for grouped/aggregate queries, and projected output names or qualified/unambiguous source columns for joins; `ASC`/`DESC` and `NULLS FIRST`/`LAST`. An output name takes precedence over a source column with the same name. JSON values cannot be ordered. |
| `LIMIT`, `OFFSET` | Supported | Non-negative integer literal or `$n` parameter. `LIMIT` is at most 100,000; `OFFSET` and `OFFSET + LIMIT` are at most 4,294,967,295. `OFFSET` may appear alone; when both occur, `LIMIT` must precede `OFFSET`. |
| `GROUP BY` | Narrow | Up to 32 plain boolean, integer, float, or text columns (not JSON) on one typed table. Every selected non-aggregate column must be grouped explicitly. |
| `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` | Narrow | Every aggregate query requires a typed column catalog, including `COUNT(*)`. Functions accept `COUNT(*)` or one plain column argument. `SUM`/`AVG` accept integer or float; `MIN`/`MAX` accept integer, float, or text. Up to 64 aggregate calls. |
| `HAVING`, aggregate `DISTINCT`, `FILTER`, windows | No | No post-group predicate, distinct aggregate, filter clause, or window form. |
| `JOIN`, `INNER JOIN` | Narrow | Adds one typed table to a left-deep chain of at most eight sources. Each `ON` has one or more column equalities joined by `AND`, with at most 32 across the query; every equality connects the incoming source to an earlier source. |
| `LEFT [OUTER] JOIN` | Narrow | The same bounded chain; an unmatched incoming source is represented by `NULL` columns. A later inner join can remove that null-extended row. |
| `RIGHT`, `FULL`, `CROSS`, `NATURAL`, `USING`, `LATERAL` | No | No additional join families, parenthesized/derived relations, or join reordering. |
| `AS` | Narrow | Output aliases on `SELECT` items in single-table, grouped/aggregate, and join queries, plus a table alias on every table a `SELECT` reads and on the table of an `UPDATE` or `DELETE`. A projection alias requires the `AS` keyword, and one column may be returned under several aliases. A table alias may omit `AS`. `INSERT` does not accept a table alias, and no alias takes a column list. |
| `SELECT DISTINCT` | Narrow | Removes duplicate rows from an explicit single-table or join projection. `NULL`s compare as equal to each other, and other values compare by SQL equality; JSON columns are rejected. `ORDER BY` must name projected columns: by output name in a single-table query, and by output name or projected source column in a join. A single-table `DISTINCT` compares at most 32 distinct source columns and shares the aggregate group limits. `DISTINCT *`, `DISTINCT ON`, and `DISTINCT` combined with `GROUP BY` or aggregate functions are rejected. |
| `IN (SELECT ...)` | Narrow | An uncorrelated [subquery](#subqueries) returning one column of at most 1,024 rows. |
| `WITH`, other subqueries, `UNION`/`INTERSECT`/`EXCEPT` | No | No CTEs, correlated, scalar, `EXISTS`, or `FROM` subqueries, or set operations. |
| `CREATE TABLE [IF NOT EXISTS]` | Narrow | Typed columns and a required inline or table-level primary key. Up to 256 columns. |
| `PRIMARY KEY` | Narrow | One inline single-column declaration or one table-level column list (single or composite). It implies `NOT NULL`; JSON keys are rejected. |
| `NULL`, `NOT NULL`, `DEFAULT` | Narrow | String, number, boolean, or `NULL` literal defaults only. No default expressions, functions, sequences, or parameters. |
| `CREATE [UNIQUE] INDEX [IF NOT EXISTS]` | Narrow | One or more boolean, integer, or text columns. No methods, expressions, predicates, `INCLUDE`, ordering, or concurrent build. |
| `ALTER TABLE ... ADD [COLUMN] [IF NOT EXISTS]` | Narrow | Adds one non-primary-key column and atomically backfills its literal default or `NULL`. On a nonempty table, `NOT NULL` requires a non-null default. Other `ALTER` forms are rejected. |
| `DROP TABLE [IF EXISTS]` | Narrow | Drops the table and its indexes. No `CASCADE`/`RESTRICT` dependency model. |
| `DROP INDEX [IF EXISTS]` | Supported | Drops one globally named index. |
| `INSERT ... VALUES` | Narrow | Optional column list, up to 4,096 literal/parameter rows, per-cell `DEFAULT`, and optional `RETURNING`. |
| `INSERT ... DEFAULT VALUES` | Supported | Inserts one row using defaults and `NULL` values. |
| `INSERT ... ON CONFLICT` | Narrow | `ON CONFLICT [(columns)] DO NOTHING` or `ON CONFLICT (columns) DO UPDATE SET column = value, ...`, before any `RETURNING`. See [upserts](#upserts). |
| `INSERT ... SELECT`, `MERGE` | No | No query-sourced insert or merge statement. |
| `UPDATE ... SET ... [WHERE ...]` | Narrow | Assigns `DEFAULT` or an [expression](#expressions) of the row's columns, literals, and parameters; optional `RETURNING`. No `UPDATE ... FROM`. |
| `DELETE FROM ... [WHERE ...]` | Narrow | Optional `RETURNING`. No `DELETE ... USING`. |
| `RETURNING` | Narrow | `*` or a list of distinct plain columns, which the table's name or alias may qualify; no expressions or output aliases. Duplicate names return `INVALID_QUERY` before any rows are changed, even when no rows match. |
| `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT` | No | Use the JavaScript callback transaction API. |
| `PREPARE`, `EXECUTE`, `DEALLOCATE` | No | SQL-level named statements are not implemented. Use the session-local JavaScript prepare() handle and its execute()/close() methods. |
| `COPY`, `TRUNCATE`, `EXPLAIN`, `VACUUM`, `ANALYZE` | No | No server maintenance or bulk-file SQL commands. |

query() accepts exactly one statement, with one optional trailing semicolon.
prepare() has the same one-statement and ordinary SQL text/token limits, but
accepts only the read and row-mutation statement families listed above.
exec() splits only top-level semicolons: strings, quoted identifiers, line
comments, nested block comments, and parentheses cannot accidentally terminate
a statement. A script contains at most 256 statements and 1 MiB of SQL text;
each statement retains the ordinary parser limits below.

## Predicates and expressions

| Form | Status | Semantics |
| --- | --- | --- |
| Strings, numbers, `TRUE`, `FALSE`, `NULL` | Supported | Single-quoted strings escape a single quote as `''`; numbers and booleans use their JSON-compatible scalar forms. |
| `$1`, `$2`, ... | Supported | One-based JSON-compatible parameters; at most 1,024. |
| `=`, `<>`, `!=`, `<`, `<=`, `>`, `>=` | Narrow | Strict scalar comparison, with integer/float cross-comparison. JSON supports structural equality/inequality only. |
| `AND`, `OR`, `NOT`, parentheses | Supported | Precedence is `NOT`, then `AND`, then `OR`; SQL unknown/null propagation is preserved. |
| `IS NULL`, `IS NOT NULL` | Supported | Tests the single runtime null value. |
| `IN (...)`, `NOT IN (...)` | Supported | One to 1,024 literals or parameters, or a [subquery](#subqueries), with SQL null behavior. |
| `BETWEEN`, `NOT BETWEEN` | Narrow | `column BETWEEN low AND high` means exactly `column >= low AND column <= high`, and `NOT BETWEEN` means `column < low OR column > high`, with those comparisons' type and null rules. Each bound is a literal or parameter. There is no `SYMMETRIC` form, so a reversed range matches nothing. |
| `+`, `-`, `*`, `/`, `%`, `\|\|` | Narrow | In the values `UPDATE` and `ON CONFLICT DO UPDATE` assign, on either side of a comparison, and in the select list of a query that does not aggregate. See [expressions](#expressions). |
| Casts, `CASE`, scalar functions | No | There are no casts, conditional expressions, or functions other than the aggregates. |
| `LIKE`, `NOT LIKE`, `ILIKE`, `NOT ILIKE` | Narrow | Matches a whole text column value against a literal or parameter pattern, where `%` matches any run of characters and `_` exactly one character; a non-text column is rejected. Backslash makes the next pattern character literal unless `ESCAPE` names another single character, or `''` for none; a pattern ending in its escape character is rejected. `ILIKE` folds only ASCII letters, as PostgreSQL does under the C locale. A `NULL` operand is unknown. A `LIKE` pattern that begins with literal characters, such as `'abc%'`, reads only the part of an index or primary key that can match; other patterns scan. |
| `IS DISTINCT FROM`, `SIMILAR TO`, `ANY`, `ALL` | No | These PostgreSQL predicate families are not implemented. |
| JSON/path operators | No | JSON can be stored, returned, and compared for structural equality only. |

Either side of a comparison may be an [expression](#expressions), including a
plain column, so `WHERE updated > created` and `WHERE price * quantity > $1`
compare values worked out from the row. `IN`, `BETWEEN`, `LIKE`, and `IS NULL`
take a plain column and literal or parameter values, and only `IN` takes a
subquery. A join's `ON` clause still holds only column equalities.

### Subqueries

`IN` and `NOT IN` can take the values a query returns, in any statement with a
`WHERE` clause, including joins, aggregates, `UPDATE`, and `DELETE`:

```sql
SELECT id, title FROM posts
WHERE user_id IN (SELECT id FROM users WHERE team = $1);

DELETE FROM sessions WHERE user_id NOT IN (SELECT id FROM users);
```

The subquery runs once, before the statement reads any row, and reads what the
statement reads: the rows before the statement changes any, and a
transaction's staged rows. It is a `SELECT` of one table, a join, or an
aggregate, and may hold subqueries of its own, up to 16 in a statement. It must
return exactly one column, and at most 1,024 rows, as many values as an `IN`
list takes, or the statement fails with `QUERY_WORK_LIMIT_EXCEEDED`. As in
PostgreSQL, a `NULL` among its values leaves `NOT IN` unknown for every row it
does not match, so `NOT IN` returns no rows then.

A subquery cannot read the statement around it, so a correlated reference such
as `WHERE posts.user_id = users.id` names an unknown column. It cannot have
`ORDER BY`, `LIMIT`, or `OFFSET`, and the values it returns are compared as an
`IN` list's are, so they never narrow the rows the statement reads. A larger or
correlated set is better written as a join.

## Expressions

An `UPDATE` or `ON CONFLICT DO UPDATE` assignment can work its value out from
the row it updates, a `WHERE` comparison can compare values worked out from
the rows it reads, and a query can return them:

```sql
UPDATE counters SET hits = hits + 1, label = label || ' (edited)'
WHERE id = $1;

SELECT id, hits * 2 AS doubled FROM counters
WHERE hits * 2 > target - $1 ORDER BY doubled DESC;

INSERT INTO counters (id, hits) VALUES ($1, 1)
ON CONFLICT (id) DO UPDATE SET hits = counters.hits + EXCLUDED.hits;
```

An expression combines columns, literals, and parameters with `+`, `-`, `*`,
`/`, `%`, unary `-`, `||`, and parentheses. As in PostgreSQL, `*`, `/`, and `%`
bind more tightly than `+` and `-`, and `||` more loosely than both. Every
assignment reads the row as it was before the statement, so
`SET a = b, b = a` swaps two columns of the same type.

- Arithmetic takes integers and floats. Two integers give an integer, and
  integer division truncates toward zero. A result outside the
  JavaScript-safe range fails with `NUMERIC_OVERFLOW`.
- A float operand makes the result a float, which must be finite or the
  statement fails with `NUMERIC_OVERFLOW`. As with PostgreSQL's `double
  precision`, `%` takes only integers.
- Dividing by zero, or taking a remainder by zero, fails with
  `DIVISION_BY_ZERO`.
- `||` joins two texts. There are no implicit casts, so it takes no other type,
  and `'1' + 1` is a `TYPE_MISMATCH`.
- A `NULL` operand gives `NULL`.

Operand types, whether a result's type fits the column it is assigned to, and
whether compared values can be compared, are checked before any row is read.
`SET hits = hits * 1.5` on an integer column fails with `TYPE_MISMATCH` even
when no row matches. A value that fails
as a row is written, such as an overflow or a `NULL` for a `NOT NULL` column,
fails the statement, which changes nothing. An expression that reads no column
is worked out once for the statement.

In `ON CONFLICT DO UPDATE`, a column of the stored row is named with the
table's name, such as `counters.hits`, and a column of the row proposed for
insertion with `EXCLUDED`. A plain name could mean either, so, as in PostgreSQL,
it is rejected.

In a single-table or join query, a select-list expression's field is named by
its `AS` alias, or `?column?` without one, as in PostgreSQL. Two expressions
without aliases therefore share a name, which only
[array rows](#repeated-output-names) can hold. The field's type is the type of the values the expression gives, and
text for an expression that gives only `NULL`. `ORDER BY` can name an
expression's alias; it then sorts the returned rows, so each of its names must
be an output, or a column an output returns. Aggregate and `SELECT DISTINCT`
queries return only columns and aggregates.

Casts, `CASE`, functions, and comparisons inside an expression are not
supported.

## Runtime types

PostgreSQL type spellings map onto five TinyJoin runtime types. The spelling
does not import PostgreSQL's storage width, coercion, operator, or catalog
semantics.

| Accepted SQL spellings | TinyJoin value | Important difference |
| --- | --- | --- |
| `BOOLEAN`, `BOOL` | JavaScript boolean | No PostgreSQL coercions. |
| `SMALLINT`, `INTEGER`, `INT`, `INT2`, `INT4`, `BIGINT`, `INT8` | One JavaScript-safe integer type | Range is -9,007,199,254,740,991 through 9,007,199,254,740,991. `SMALLINT`/`INTEGER` are wider and `BIGINT` is narrower than PostgreSQL. |
| `REAL`, `FLOAT`, `FLOAT4`, `FLOAT8`, `DOUBLE PRECISION` | One finite binary64 JavaScript number | No real/double distinction, `NaN`, or infinity. |
| `TEXT`, `VARCHAR`, `CHARACTER VARYING` | JavaScript string | No length modifiers or database collation. Ordering is deterministic Unicode code-point ordering. |
| `JSON`, `JSONB` | The same JSON-compatible value (scalar, array, or object) | No textual/binary distinction, JSON operators, casts, or JSON index type. |

SQL `NULL` and a JSON scalar `null` are the same runtime value, including in a
JSON column. TinyJoin cannot distinguish them for `NOT NULL`, `IS NULL`,
aggregates, or defaults.

There are no implicit PostgreSQL casts. Notable unavailable types include
`NUMERIC`/`DECIMAL`, date/time/interval types, UUID, `BYTEA`, arrays,
serial/identity, enum/domain, and user-defined types. Type modifiers such as
`VARCHAR(100)` are rejected.

## Identifiers, comments, and table names

- Unquoted identifiers are folded to ASCII lower case. Double-quoted
  identifiers preserve case and use doubled quotes to escape a quote.
  A quoted column name containing a dot is usable in a single-table query,
  but a table containing any such column cannot be a join source, even when
  the column is not selected. Quoted join aliases cannot contain dots either.
- An unquoted identifier may begin with `_`, an ASCII letter, or any non-ASCII
  character. Later characters may additionally be ASCII digits or `$`.
- TinyJoin reserves these unquoted words case-insensitively: `SELECT`, `FROM`,
  `WHERE`, `AND`, `OR`, `IS`, `IN`, `LIMIT`, `OFFSET`, `ORDER`, `BY`, `ASC`,
  `DESC`, `NULLS`, `FIRST`, `LAST`, `NULL`, `TRUE`, `FALSE`, `CREATE`, `TABLE`,
  `IF`, `NOT`, `EXISTS`, `PRIMARY`, `KEY`, `DEFAULT`, `INSERT`, `INTO`,
  `VALUES`, `UPDATE`, `SET`, `DELETE`, `RETURNING`, `AS`, `JOIN`, `INNER`,
  `LEFT`, `OUTER`, `ON`, `GROUP`, and `HAVING`. Double-quote one to use it as
  an identifier. Other words used contextually by supported statements are not
  necessarily reserved.
- `--` line comments and nested `/* ... */` comments are supported.
- Single-quoted strings use doubled single quotes. Dollar-quoted strings are
  not supported.
- A two-part table name such as `public.tasks` is accepted as one flat catalog
  key. It does **not** create or resolve a PostgreSQL schema. `tasks` and
  `public.tasks` are different TinyJoin table names.
- There is no `CREATE SCHEMA`, `search_path`, `information_schema`, or
  `pg_catalog`. Index names are global catalog keys.

### Qualified columns

A `SELECT`, `UPDATE`, or `DELETE` over one table may name a column as
`table.column`, and write `table.*` for `*`, wherever it reads that table's
columns: in a projection, aggregate argument, `WHERE`, `GROUP BY`, `ORDER BY`,
and `RETURNING`, including the `RETURNING` of an `INSERT`. The qualified and
plain spellings are the same statement, and are planned and narrowed alike:

```sql
SELECT tasks.id, tasks.title FROM tasks
WHERE tasks.done = false ORDER BY tasks.title;

UPDATE tasks SET done = true WHERE tasks.id = $1 RETURNING tasks.id;
```

A table alias, with or without `AS`, takes the place of the table's name as
the qualifier, as in PostgreSQL, so the name no longer qualifies anything:

```sql
SELECT t.id, t.title FROM tasks AS t WHERE t.done = false;

DELETE FROM tasks t WHERE t.id = $1;
```

A table named with a schema, such as `public.tasks`, is qualified by the name
after the dot, `tasks`, as it is in a join. `public.tasks.id` is rejected.
Naming any other table is an error, and the columns an `INSERT` lists or an
`UPDATE` assigns in `SET` are never qualified. An alias changes only how a
statement spells its columns: results, changed tables, and subscriptions name
the table itself.

In `ORDER BY`, a plain name refers to an output name before a source column,
but a qualified name is always the table's column. `SELECT title AS id FROM
tasks ORDER BY tasks.id` orders by the `id` column, not by the titles returned
as `id`. A grouped or aggregate query is ordered by its outputs, so there a
qualified column stands for the output that returns it, and must be projected.

A quoted name that holds a dot, such as `"extra.value"`, is one column name
rather than a qualified one.

### Repeated output names

An object row holds one value for each name, so a `SELECT` whose output names
repeat returns `INVALID_QUERY`, and each output needs a distinct `AS` alias.
An array row holds its values by position, so a `SELECT` read with
`rowMode: "array"` may repeat a name. This is how a query builder that reads
rows by position, and so never writes an alias, selects a column of the same
name from each side of a join:

```ts
const { fields, rows } = await db.query(
  `SELECT posts.id, users.id, users.name
   FROM posts JOIN users ON posts.user_id = users.id`,
  [],
  { rowMode: "array" },
);
// fields: id, id, name
// rows: [[10, 1, "Ann"], ...]
```

This applies to single-table, aggregate, and join queries, to prepared
statements, where each execution's `rowMode` decides, and to each statement of
an exec() script. An aggregate without an alias takes its function's name, so
`SELECT COUNT(*), COUNT(email)` returns two fields named `count`.

An `ORDER BY` name that matches more than one output is `INVALID_QUERY`, unless
those outputs all return the same column or the same aggregate. Order by a
qualified column, or give the outputs distinct aliases. `RETURNING` columns
must be distinct in either row mode.

## Upserts

`INSERT ... ON CONFLICT` inserts each proposed row unless it conflicts with an
*arbiter*: the primary key or a unique index named by the conflict target.
The target lists exactly the columns of the primary key or of one unique
index, in any order. `DO NOTHING` may omit the target, in which case the
primary key and every unique index of the table are arbiters.

```sql
INSERT INTO settings (name, value) VALUES ($1, $2)
ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value;
```

As in PostgreSQL, proposed rows are handled one at a time, so a row can
conflict with a stored row or with a row written earlier in the same
statement:

- `DO NOTHING` skips a conflicting row.
- `DO UPDATE` applies its `SET` assignments to the conflicting stored row. A
  statement that would update one row twice, or update a row it inserted, fails
  with `CONSTRAINT_VIOLATION` and changes nothing.
- A conflict on a constraint that is not an arbiter fails the statement as an
  ordinary insert would.

Each `SET` value is `DEFAULT` or an [expression](#expressions) of literals,
parameters, and columns: the stored row's, named with the table's name as in
`counters.hits`, and the proposed row's, named as in `EXCLUDED.hits`, which is
its value after column defaults. `DO UPDATE` cannot change a row's primary key,
although assigning it the same value is allowed. `NULL` never conflicts with a
unique index.

A primary-key arbiter uses direct key lookup. A unique-index arbiter reads the
index postings, except inside a callback transaction that has staged changes to
the table, where those rows are not yet indexed and each statement scans the
table once instead.

`RETURNING`, the affected-row count, and [changed keys](/guides/transactions-and-changes/#refreshing-individual-rows)
cover inserted and updated rows only. A statement that skips every row reports
no changed table. `ON CONFLICT ON CONSTRAINT`, a partial-index `WHERE` in the
target, and `DO UPDATE ... WHERE` are rejected.

## Constraints and indexes

Every SQL-created table has a primary key. TinyJoin currently implements:

- primary-key uniqueness and non-nullability;
- column `NOT NULL`;
- scalar literal column defaults; and
- separate unique indexes.

It does not implement foreign keys, `CHECK`, exclusion constraints, generated
columns, sequences, triggers, or dependency cascades.

A row is identified by its primary key: each table is stored keyed by that
value, and an `UPDATE` which changes a primary key is applied as a removal at
the old key and an insertion at the new one rather than an edit in place. Treat
primary keys as stable, opaque identifiers.

Composite primary and secondary indexes are supported. A unique index omits a
key containing `NULL`, so multiple null-containing keys are allowed, matching
PostgreSQL's default `NULLS DISTINCT` behavior.

Single-table `SELECT`, aggregates, `UPDATE`, and `DELETE` read only the rows
their `WHERE` clause can match when its `AND`-ed terms allow, and still check
the complete predicate on every row they read:

- complete primary-key equality is a direct lookup, when the key types and
  values permit an exact one, and sees staged changes inside a transaction;
- complete equality on every column of a secondary index reads that index's
  postings;
- comparisons, `BETWEEN`, and `LIKE` patterns that begin with literal
  characters read a range of an index's first column, while the range holds at
  most a quarter of the table's rows, or else a range of the primary key's first
  column. An index is used for a range only if its other columns are
  `NOT NULL`, since a row with a `NULL` indexed value is not indexed.

A comparison narrows the rows read only when one side is a plain column and the
other a value known without reading a row: a literal, a parameter, or an
[expression](#expressions) of them, such as `created > $1 - 86400`. A value on
the left is read as it would be on the right, so `5 < id` narrows as `id > 5`
does. A comparison that reads the row on both sides, such as `a > b`, is
checked on every row read.

An aggregate that reads only an index's columns and the primary key, in its
select list, `GROUP BY`, and `WHERE` clause, answers a range of that index from
its entries alone, without reading any rows, however much of the table the
range holds.

Inside a callback transaction, a table with staged changes narrows only by a
direct primary-key lookup, because its staged rows are not yet indexed or in key
order. `OR`, `NOT`, `IN`, and partial composite equality do not narrow the rows
read, and neither does a join.

## Aggregates and joins

Aggregate null behavior follows the familiar SQL rules: `COUNT(*)` counts
rows; other aggregates skip `NULL`; a global aggregate over no rows emits one
row with count zero and other aggregates `NULL`; an empty grouped input emits
no rows. Integer `SUM` fails beyond the JavaScript-safe range, and integer
`AVG` returns a floating-point value rather than PostgreSQL `numeric`.

Join keys containing `NULL` never match. Integer and float keys may compare;
JSON join keys are rejected. Every source requires a typed SQL catalog and a
unique alias, and the result must use distinct output names unless its rows
are [read as arrays](#repeated-output-names).
Unqualified columns are accepted only when exactly one source contains the
name. Without `ORDER BY`, row order is not part of the contract.

### Join projection and identifier boundaries

Join projections must name each output column explicitly. Wildcards `*` and
`table.*` are rejected. A join also rejects any source table whose catalog
contains a column name with a dot, even if the projection and `ON` clause do
not use that column. Quoting does not remove this restriction.

This small schema reproduces both boundaries:

```sql
CREATE TABLE left_items (id INTEGER PRIMARY KEY);
CREATE TABLE right_items (id INTEGER PRIMARY KEY);
CREATE TABLE dotted_items (id INTEGER PRIMARY KEY, "extra.value" TEXT);
INSERT INTO left_items VALUES (1);
INSERT INTO right_items VALUES (1);
INSERT INTO dotted_items VALUES (1, 'kept');
```

These single statements succeed:

```sql
SELECT left_items.id AS id
FROM left_items JOIN right_items ON left_items.id = right_items.id;
```

```sql
SELECT id, "extra.value" FROM dotted_items;
```

Each of the following statements is unsupported:

```sql
SELECT * FROM left_items JOIN right_items ON left_items.id = right_items.id;
```

```sql
SELECT left_items.* FROM left_items JOIN right_items ON left_items.id = right_items.id;
```

```sql
SELECT left_items.id AS id
FROM left_items JOIN dotted_items ON left_items.id = dotted_items.id;
```

The last query fails even though `"extra.value"` is never selected. Use column
names without dots for tables that will participate in joins.

### Join work budgets

Join chains are evaluated as written, from left to right; TinyJoin does not
reorder them. Each `ON` equality must connect its newly introduced source to
one of the sources already in scope. For each combination of earlier rows, a
joined table's matching rows are found:

- by direct lookup, when the `ON` equalities fix its complete primary key;
- through a secondary index whose every column they fix, unless the table has
  staged changes inside a callback transaction; or
- in a hash table of the table's rows keyed by the `ON` columns, built once
  per query.

`WHERE` terms that read only the first table, or only a table joined without
`LEFT`, also narrow how that table is read, as they would in a single-table
query, and rows that fail them are dropped before joining. A term on a table
joined with `LEFT` must see its null-extended rows, so it is never applied
early. The complete `WHERE` clause is still checked on every joined row.

Across the full chain, candidate-extension, retained-row, result-row, and byte
budgets are global rather than resetting for each `JOIN`. Aggregates over joins
are not supported. A candidate is a row a lookup or hash table returns for one
combination of earlier rows, so the budget counts only rows whose `ON` values
match: a join is never rejected because the Cartesian product is large. Rows of
hash-joined tables are retained and count toward the retained-build-row limit,
and tables read whole, without narrowing terms, are checked against the scan
and retained-build-row limits before execution.

For example, three tables of 100 rows joined on unique matching identifiers
examine 200 candidates and return 100 rows. Two tables of 1,001 rows whose join
keys all match examine over 1,000,000 candidates, and exceed the budget even
when a later `WHERE` term that reads both tables removes every result. An
unordered `LIMIT` can stop early; an ordered join must first collect its
matches. Standalone queries and exec() also charge scans and candidates to
their shared script-work budget, which can be reached before the join's own
limit.

Many-to-many relationships can use a bridge table with a composite primary
key, for example:

```sql
SELECT post.id AS post_id, tag.name AS tag_name
FROM posts AS post
JOIN post_tags AS post_tag ON post.id = post_tag.post_id
JOIN tags AS tag ON post_tag.tag_id = tag.id
ORDER BY post_id, tag_name
```

Foreign keys are not implemented, so TinyJoin does not enforce the bridge
table's references.

## Transactions and concurrency

Each query() or standalone prepared-statement write is atomic. A standalone
exec() script runs its supported reads, DDL, and DML against one page candidate
and publishes one durable generation only after every statement succeeds. A
callback transaction stages `INSERT`, `UPDATE`, and `DELETE` statements,
including through tx.execute(), exposes those staged rows to reads through its
transaction object, and publishes the complete result once.
transaction.exec() may group DML and reads as an atomic savepoint within that
staged transaction: a failure installs none of that script's changes. DDL is
rejected before any statement in a transaction script runs and must use a
standalone query() or exec() call.

If a transaction statement fails, that statement installs no partial change,
but the transaction is not put into PostgreSQL's aborted state. If the callback
catches the error, earlier staged writes may still commit. This is also true for
a prepared execution. Letting the error escape the callback rolls the
transaction back.

Calls to the same Client's transaction() queue in order. Awaiting one from
inside its own active callback deadlocks; pass the existing Transaction into
helpers instead. There is no AbortSignal or timeout option, and racing a
Promise against a timer does not cancel the work. See
[transaction composition and errors](/guides/transactions-and-changes/#composing-transaction-helpers).

Atomicity does not make every failed write's outcome knowable to its caller.
After `RECOVERY_REQUIRED`, `STORAGE_COMMIT_OUTCOME_UNKNOWN`, or
`STORAGE_ENGINE_POISONED`, stop using the Client, close and reopen it, and
reconcile stored state before replaying a write. The `retryable` flag is not a
safe-replay guarantee. See [storage recovery](/guides/storage-and-lifecycle/#recovering-after-an-uncertain-write).

Requests are serialized through one Worker. OPFS persistence permits one open
Worker for a database name; it is an exclusive writer rather than a
PostgreSQL-style set of concurrent sessions. There is no MVCC session model,
isolation-level selection, user-controlled savepoints, lock manager, or
deadlock detection. Different OPFS names are independent databases and do not
synchronize with one another.

## PostgreSQL facilities that are not present

TinyJoin has no PostgreSQL wire protocol, SQLSTATE-compatible error protocol,
server process, roles or grants, system catalogs, extensions, stored
procedures, triggers, notifications, WAL, replication, point-in-time recovery,
or PostgreSQL file-format compatibility. Rows and parameters are
JSON-compatible JavaScript values. Result fields use the closest stable
PostgreSQL OID as metadata: boolean `16`, integer `20`, text `25`, JSON `114`,
and float `701`. This mapping does not add PostgreSQL storage widths,
coercions, operators, parsers, or wire semantics.

Persistence is TinyJoin's own page format in memory or one browser OPFS file.
It is not a PostgreSQL data directory.

JavaScript prepared statements are Worker-owned parsed statements, not
PostgreSQL named prepared statements, server plan-cache entries, protocol
objects, or persistent database objects.

## Hard limits

Limits are part of the runtime contract: oversized work fails explicitly
rather than growing without bound.

| Resource | Current limit |
| --- | ---: |
| Physical database | 65,536 4 KiB pages (256 MiB) |
| Tables / indexes | 4,096 each |
| Columns per table or projection | 256 |
| Catalog name | 1,023 UTF-8 bytes |
| Complete encoded storage key | 1,024 bytes |
| Encoded logical row data | 1,048,568 bytes |
| Complete paged row / individual encoded JSON value | 1,048,576 bytes |
| JSON nesting | 64 levels |
| SQL text / tokens / parameters | 64 KiB / 4,096 / 1,024 |
| Expanded bound parameter values | 16 MiB per statement |
| Open prepared statements / retained prepared state | 128 / 8 MiB per open database |
| exec() script text / statements | 1 MiB / 256 |
| exec() row, index, scan, and join operations | 1,000,000 across the script |
| exec() retained result work | 16 MiB across the script |
| Predicate nodes / nesting / `IN` values | 256 / 32 / 1,024 |
| Expression terms / nesting | 256 / 32 |
| Subqueries in a statement / rows a subquery returns | 16 / 1,024 |
| Rows in one `INSERT ... VALUES` | 4,096 |
| Explicit `LIMIT` / `OFFSET` / `OFFSET + LIMIT` | 100,000 / 4,294,967,295 / 4,294,967,295 |
| Rows scanned / returned by a query | 1,000,000 / 100,000 |
| Rows changed by one `UPDATE` or `DELETE` | 100,000 |
| Ordered matching rows | 100,000 |
| Transaction overlay | 100,000 keys and 16 MiB |
| Table sources in one joined `SELECT` | 8 total (one base plus seven `JOIN` clauses) |
| `ON` equalities in one joined `SELECT` | 32 across the chain |
| Join candidate row extensions / returned rows | 1,000,000 across the chain / 100,000 |
| Join retained build rows | 100,000 across the chain |
| Join working state / result data | 16 MiB / 16 MiB across the chain |
| Aggregate groups or single-table `DISTINCT` rows / aggregate calls | 100,000 / 64 |
| Aggregate cells (groups times aggregate calls) | 1,000,000 |
| Query, DML result, join, aggregate, or mutation working set | 16 MiB per operation-specific bound |

An ordered query can reach its materialization limit before applying a small
`LIMIT`, unless it is ordered by the primary key: ascending by its leading
columns, or descending by all of them. Such a query reads rows in key order and
stops at its `LIMIT`, so the ordered-row limit does not apply to it, except
when it descends through a secondary index or reads a table with staged
changes inside a transaction. Join candidate-extension and retained-row bounds
apply to the complete left-deep chain, not separately to each step and not just
to returned rows. The candidate limit is enforced by a runtime counter as
candidates are found, including those that later fail another `ON` equality or
the `WHERE` clause. The 1,024-byte secondary-index key limit covers the complete encoded indexed tuple
and primary-key tuple together, not each component independently. A boolean
component takes 1 byte, an integer or float 8, and text its UTF-8 bytes plus 2,
and 1 more for each zero byte. An individual JSON value remains subject to the
smaller budget for the row that contains it.

Every occurrence of a parameter in a statement counts toward the expanded
binding budget. Repeating one large `$1` value many times can therefore fail
with `RESOURCE_LIMIT` even when the supplied parameter array is small. This
check runs before the values are copied into the statement, including prepared
executions and queries with `LIMIT 0`. It is an independent allocation bound,
not a limit on the total memory used by the browser or WebAssembly instance.
