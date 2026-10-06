# Releases

This is a reverse chronological summary of TinyJoin releases and their public
compatibility boundaries. Every entry states what upgrading to it requires, so
check the entries between the version in use and the target before upgrading.
A release that needs no action says so explicitly.

## v0.6.0 (unreleased)

This release makes the engine faster across the
[comparative benchmarks](/guides/benchmarks/), with no change to the API or to
the storage format. With OPFS storage, TinyJoin is the fastest of the three
engines in 11 of the 20 timed workloads and second in the other nine, never
the slowest. Range aggregates take about half the time they did, `LIKE` scans
and a transaction's range `UPDATE`s a third less, the 8,000-row range delete
a tenth less, and 200-row inserts about what SQLite's take.

### Scanning rows

A `WHERE` clause's simple terms, those that compare an `INTEGER` or `TEXT`
column with constants, match a `LIKE` pattern, or test for `NULL`, are
compiled into tests of the stored record, and a table scan applies them to
each leaf's rows in one pass over the leaf's bytes, decoding and presenting
only the rows they accept. A `LIKE` pattern of literal segments is matched as
bytes. In the benchmark, 100 range aggregates without an index fell from 48 ms
to 25, 100 `LIKE` aggregates from 113 ms to 70, and a transaction's 100 range
`UPDATE`s from 62 ms to 41.

### Writing rows by key

A prepared `INSERT`, including one of many listed rows, a lone upsert on the
primary key, and an `UPDATE` or `DELETE` of the row a primary-key equality
names, run inside a transaction, is planned straight from its template and
parameters: its key is encoded from them, the row looked up once, and the
change built as the record a commit writes, rather than binding the
statement's syntax tree and planning it as any statement is. A test stages
the same statements both ways and checks that the transactions, the results,
and the committed databases agree. Staging then looks a statement's table up
once rather than for every row, a listed row's values are resolved to literals
or parameter positions once, when the statement is prepared, rather than for
every row of every execution, and an insert into a table none of whose staged
rows is a delete skips the search of the staged rows for an entry it would
replace, which planning would have read. Engine-only, 10,000 inserts in a
transaction take a quarter less time, updates by key a fifth less, upserts a
third less, and 200-row inserts a third less.

### Changing many rows

The rows a statement changes share one name for their table, kept once per
statement rather than copied for every row, and the entries an index gains
and loses for them are encoded into one buffer rather than a vector each. A
statement's work is charged once rather than for every row, a row's estimated
bytes are added and checked in place rather than through calls, a table's
record layout keeps the JSON overhead of its rows, which planning a statement
by key measured for every statement, and a result's changed keys share their
table's schema rather than copying its name and key columns. The Worker
writes each response into the text kept from the last. In Node's V8,
deleting 8,000 rows by an indexed range takes 12% less time optimized and
18% less before optimization, a `LIKE` delete 6% less, and 10,000 inserts in
a transaction 6% less before optimization.

### Comparing memory

The WebAssembly build compares memory eight bytes at a time, in place of the
byte loops the compiler provides, which nearly every comparison of keys in a
B-tree search, a sort, or a transaction's staged rows ran. It matters most
before V8 has optimized the code, when a statement over many rows runs the
loop for every one of them: under baseline compilation, 10,000 inserts in a
transaction take 14% less time, and creating two indexes 18% less.

### The client and the Worker

A result's command is one of a few fixed words, kept as a static string and
compared rather than tested against regular expressions; the client builds a
prepared execution's request in its exact shape; the Worker sizes a
statement's scalar parameters by their shape and writes them into its request
in one pass; and a response's keys are counted rather than walked where each
is checked by name. Warm in Chromium, an update by key in a transaction spends
about 2 µs less in the Worker's JavaScript and 1 µs less in the page's.

### Download size

The compressed download is now {{sizes.total.gzip}}, up from 339 KiB in
v0.5.0, for the record tests, the point planner, and the word-wise
comparison.

### Upgrading from v0.5.0

Nothing is required: the API and the storage format are unchanged.

## v0.5.0

This release connects TinyJoin to the [Drizzle ORM](https://orm.drizzle.team)
and the [Kysely](https://kysely.dev) query builder, and adds what they need: a
typed schema API, foreign keys, fuller `ALTER TABLE`, and more of the
[SQL dialect](/guides/sql-compatibility/) that hand-written statements and
query builders use. It also fixes two v0.4.0 bugs, and reads rows faster.

### Fixes since v0.4.0

**A database with a column declared `DEFAULT NULL` opens again.** v0.4.0 and
earlier wrote such a column's default into a persistent database correctly, but
read it back as no default at all, so the catalog no longer matched what had
been written, and opening the database failed with `PAGED_STORAGE_CORRUPT`.
v0.5.0 reads the default as it was written, so an affected v0.4.0 database
opens with its data intact. In-memory databases, which are never reopened, were
not affected.

**Adding a `NOT NULL` column without a default to a table with rows fails
again.** v0.4.0 stopped writing every row again when `ALTER TABLE ... ADD
COLUMN` adds a column, and with that stopped checking that the rows could hold
it, so a `NOT NULL` column without a default could be added to a table with
rows, which then held `NULL` in it: queries returned the `NULL`, and check()
reported the table as corrupt. v0.5.0 refuses the statement with
`CONSTRAINT_VIOLATION`, as v0.3.0 and PostgreSQL do. To repair a table that
v0.4.0 left this way, give the column a value in every row, as in
`UPDATE t SET c = 0 WHERE c IS NULL`, or drop its `NOT NULL`.

### ORM support

The new `tinyjoin/drizzle` entry point connects the
[Drizzle ORM](https://orm.drizzle.team) to a TinyJoin Client:
`drizzle(client, {schema})` returns a Drizzle database whose queries run in
TinyJoin's dialect, whose transactions are TinyJoin's callback transactions,
and whose JSON columns store JSON values rather than JSON text. The application
installs `drizzle-orm` 0.45 itself; TinyJoin declares it as an optional peer
dependency and never bundles it, so the download sizes are unchanged.

The entry point's migrate() function applies the migrations that
`drizzle-kit generate` writes, from SQL the application bundles, each
atomically with the row that records it, so every tab can call it as it
starts. Drizzle Kit's `push` and Drizzle's own migrators are not supported.
See the [Drizzle guide](/guides/drizzle/).

The new `tinyjoin/kysely` entry point connects the [Kysely](https://kysely.dev)
query builder to a TinyJoin Client. Its TinyJoinDialect, constructed with
`{client}`, is a Kysely dialect whose queries run in TinyJoin's dialect, whose
transactions are TinyJoin's callback transactions, whose introspector reads
getSchema(), and under which Kysely's Migrator runs, one tab at a time. The
application installs `kysely` 0.28 or later itself; TinyJoin declares it as an
optional peer dependency and never bundles it. See the
[Kysely guide](/guides/kysely/).

[create-tinyjoin](https://github.com/tinyplex/create-tinyjoin) v0.5.0 asks how
its starter app writes its queries: in SQL, or with the Drizzle ORM or the
Kysely query builder. Its Drizzle app pushes a Drizzle schema as each tab
starts, and its Kysely app creates its table in a migration that Kysely's
Migrator runs.

### Reading and setting the schema

The new getSchema() method on the Client reads the database's tables, with
their columns, primary keys, indexes, and foreign keys, as typed objects rather
than through SQL catalog tables, which TinyJoin does not have. Each column
carries its runtime type, whether it is nullable, any literal default, and any
`VARCHAR` length. See
[reading the schema](/guides/storage-and-lifecycle/#reading-the-schema).

The new setSchema() method makes the database's tables match a schema in the
shape getSchema() returns, in one atomic change: it creates what is missing,
renames tables and columns from the names they had, changes defaults,
nullability, and `VARCHAR` lengths, and makes each table's indexes the
schema's, keeping every row, and drops what the schema leaves out only when
asked. A schema the database already has changes nothing, and a schema version
keeps a tab running older code from undoing newer changes. `tinyjoin/drizzle`
adds push(), which sets a Drizzle schema this way, in place of migrations. See
[setting the schema](/guides/storage-and-lifecycle/#setting-the-schema).

### Foreign keys

Foreign keys are now enforced. A column's `REFERENCES`, or a table's
`FOREIGN KEY ... REFERENCES`, names the table and columns its values must be
found in, unless one is `NULL`, and each key is checked as its statement ends,
inside transactions too. Deleting or updating a referenced row does what the
key's `ON DELETE` and `ON UPDATE` actions say: `NO ACTION` and `RESTRICT`
refuse it, `CASCADE` deletes or updates the referencing rows in the same
statement, and `SET NULL` and `SET DEFAULT` change their references. `ALTER
TABLE` adds a key, checking every stored row, and drops one; renames follow
into the keys; and dropping a table, column, or unique index a key needs takes
`CASCADE`, which drops the key. getSchema() and setSchema() include each
table's keys, and Drizzle's `.references()` pushes as one. See
[foreign keys](/guides/sql-compatibility/#foreign-keys).

### Creating and altering tables

`CREATE TABLE` accepts the constraints and spellings that schema tools write:
a named primary key, as in `CONSTRAINT tags_pk PRIMARY KEY (post_id, tag)`;
`UNIQUE` on a column or as a table constraint, which creates a unique index
named as PostgreSQL would name it; a JSON default written as `'{}'::jsonb`; and
`USING btree` on an index. `ALTER TABLE` can add and drop a unique constraint,
and accepts `DISABLE ROW LEVEL SECURITY`, which changes nothing. `DROP TABLE`
and `DROP INDEX` accept `CASCADE` and `RESTRICT`. Together these run the SQL
that `drizzle-kit generate` writes for new tables.
See [constraints and indexes](/guides/sql-compatibility/#constraints-and-indexes).

`ALTER TABLE` can now also rename a table or a column, drop a column, set or
drop a column's default, and set or drop `NOT NULL`, and accepts a column type
that restates the runtime type it has, as `bigint` does for `integer`. Existing
rows keep their values: dropping a column, changing a default, and setting
`NOT NULL` rewrite the table's rows, and `SET NOT NULL` fails, changing nothing,
while a row holds `NULL`. A dropped column takes every index on it with it, as
in PostgreSQL, and indexes follow renamed columns and tables.

A `VARCHAR(n)` or `CHARACTER VARYING(n)` column holds at most `n` characters,
and a longer value fails with `CONSTRAINT_VIOLATION`, as in PostgreSQL, except
that TinyJoin also refuses a value whose excess is spaces, which PostgreSQL
would trim. `n` was previously rejected, so schemas that ORMs and migration
tools write with lengths, such as Drizzle's `varchar({length})` and Kysely's
migration tables, now run. getSchema() reports the length as `maxLength`.

### Naming tables and columns

A statement over one table can qualify its columns with the table's name, as a
join already could: `SELECT tasks.id FROM tasks WHERE tasks.done = false ORDER
BY tasks.title`, or `tasks.*` for every column. `UPDATE` and `DELETE` accept
the same in `WHERE` and `RETURNING`, and `INSERT` in `RETURNING`. A qualified
statement is planned exactly as its plain spelling is, so it uses the same key
lookups and index ranges. In `ORDER BY`, a qualified name is always the table's
column, where a plain name matches an output alias first. See
[qualified columns](/guides/sql-compatibility/#qualified-columns).

A single-table `SELECT`, `UPDATE`, or `DELETE` also accepts a table alias, with
or without `AS`, as each table of a join already did: `SELECT t.id FROM tasks
AS t WHERE t.done = false`, or `DELETE FROM tasks t WHERE t.id = $1`. As in
PostgreSQL, the alias replaces the table's name as the qualifier. Results,
changed tables, and subscriptions still name the table.

A `SELECT` read with `rowMode: "array"` may now return several fields of one
name, such as the `id` of each table in a join, because an array row holds its
values by position. Object rows still require distinct output names and reject
the same statement with `INVALID_QUERY`, as before. Query builders that map
rows by position, as Drizzle does, depend on this. See
[repeated output names](/guides/sql-compatibility/#repeated-output-names).

A join's select list can use `*` for every column of every table and `table.*`
for every column of one, as in `SELECT p.*, u.name FROM posts p JOIN users u
ON u.id = p.user_id`, and a single table's can put `*` beside other outputs, as
in `SELECT *, price * quantity AS total FROM items`. Object rows still need
distinct names, so a `*` over tables that share a column name needs array rows.

### Expressions

An `UPDATE` or `ON CONFLICT DO UPDATE` assignment can now work its value out
from the row it updates, so a counter no longer needs a read before its write:
`UPDATE counters SET hits = hits + 1 WHERE id = $1`. An expression combines
columns, literals, and parameters with `+`, `-`, `*`, `/`, `%`, unary `-`, and
`||`. Integers stay within the JavaScript-safe range and divide by truncating,
floats must stay finite and take no `%`, and there are no implicit casts. In an
upsert, the stored row's columns are named with the table's name and the
proposed row's with `EXCLUDED`, as in `SET hits = counters.hits +
EXCLUDED.hits`. Types are checked before any row is read.

Either side of a `WHERE` comparison can now be an expression too, so a query
can compare two columns, as in `WHERE updated > created`, or a value worked out
from the row, as in `WHERE price * quantity > $1`. A comparison of a column with
a value worked out from literals and parameters, such as
`created > $1 - 86400`, still reads only a range of an index or the primary
key.

A single-table or join query can also return expressions, as in
`SELECT id, price * quantity AS total FROM items ORDER BY total DESC`, and
`ORDER BY` can name an expression's alias. An expression without an alias is
named `?column?`, as in PostgreSQL. Aggregate and `SELECT DISTINCT` queries
still return only columns and aggregates. See
[expressions](/guides/sql-compatibility/#expressions).

### Subqueries and nested queries

`IN` and `NOT IN` can take the values of a subquery, as in
`WHERE user_id IN (SELECT id FROM users WHERE team = $1)`, in any statement with
a `WHERE` clause. The subquery runs once, before the statement reads any row,
must return one column and at most 1,024 rows, and cannot read the statement
around it. See [subqueries](/guides/sql-compatibility/#subqueries).

A query can nest queries that read each of its rows, as ORMs write them to
load related rows: a `(SELECT ...)` in the select list that returns one value,
as in `(SELECT count(*) FROM posts p WHERE p.user_id = u.id)`, and a
`LEFT JOIN LATERAL (SELECT ...) alias ON true`. Their rows can be gathered into
JSON with `json_agg`, `json_build_array`, `to_json`, and `coalesce(..., '[]')`,
including from a query in `FROM (SELECT ...) alias`. Each nested query runs
once for every row around it, up to 100,000 in a statement. Drizzle's
relational queries with `with`, and Kysely's `jsonArrayFrom` and
`jsonObjectFrom`, run this way. See
[nested queries](/guides/sql-compatibility/#nested-queries).

### Speed and download size

The warm-up Worker that create() starts runs its loops three-quarters as long,
and so finishes sooner. It was still running while a database ran its first
statements, and slowed them: in the comparative benchmark, timed workloads now
run about 5% faster overall, point reads about 8% faster, and indexed range
aggregates about a fifth faster.

A single-table `SELECT` whose rows arrive in key order no longer builds each
row as an object of its values under their names before the Worker writes the
row out. Reading all 10,000 rows of the comparative benchmark takes two-fifths
less time, and a point read by key about 3% less, for 1.2 KiB more of
compressed engine.

The compressed download is now 339 KiB, up from 301 KiB in v0.4.0, for the
engine's new SQL, foreign keys, and schema API. The Drizzle and Kysely
entry points, and the libraries they connect, load only in an application that
imports them.

### Upgrading from v0.4.0

**No action is needed.** Install, rebuild, and redeploy. The page format stays
at format 3, so an existing database opens with no migration and no OPFS
namespace change. A database that holds a `VARCHAR(n)` column, a schema version
from setSchema(), or a foreign key cannot be opened by v0.4.0, which reports
its catalog as corrupt, so do not roll back past v0.5.0 once a database holds
any of them.

The Worker protocol moves from version 10 to 11 for the schema request. Client
and Worker ship together and are upgraded together by a normal install, so this
affects only a deployment that pins or caches a Worker file independently of
the client bundle, where a mismatched pair fails cleanly with
`PROTOCOL_MISMATCH`.

The new syntax was previously rejected, so statements that already worked keep
their meaning, with one exception: where a table has a column whose quoted name
begins with the table's own name and a dot, such as `"extra.value"` in a table
named `extra`, an unquoted `extra.value` in `WHERE` used to read that column
and now reads the table's `value` column. Quote the name to read it.

## v0.4.0

**Persistent storage breaks compatibility with v0.3.0 and earlier.** v0.4.0
introduces page format 3, which stores each row as packed binary columns
rather than as JSON text, and encodes keys so that they sort in the same order
SQL compares their values. Rows take less space, a query can read the columns
it needs without parsing the rest of the row, and `ALTER TABLE ... ADD COLUMN`
no longer rewrites every existing row.

v0.1.0 through v0.3.0 use page format 2. Opening a page format 2 database with
v0.4.0 fails with `UNSUPPORTED_PAGE`, and so does opening a page format 3
database with an earlier release. There is no automatic migration, and changing
the OPFS name does not copy existing data.

- For data that can be reconstructed, use a new OPFS namespace, for example
  change `opfs://my-app-v2` to `opfs://my-app-v3`, and rebuild the database.
  The old namespace remains intact.
- To retain existing data, export the application's known tables using a
  client pinned to `tinyjoin@0.3.0` before upgrading. Create the schema in a
  new namespace with v0.4.0, import rows with parameterized statements, and
  verify the data before retiring the old database. TinyJoin does not yet
  provide a general database export or migration API.
- New apps generated by create-tinyjoin v0.4.0 target `tinyjoin@^0.4.0` and
  use a `-db-v3` storage name. Updating the generator does not migrate
  applications it generated previously.

In-memory databases, including every `tinyjoin/node` database, are unaffected.

**Object rows now list their columns in field order.** A row's keys follow the
result's `fields`, as in PostgreSQL, so `SELECT title, id` returns rows whose
first key is `title`, and a changed key lists a composite primary key's columns
in the order the key declares them. Earlier releases sorted both alphabetically.
Code that reads a row or key by column name is unaffected; only code that
depends on key order, such as `Object.keys(row)` or `JSON.stringify(row)`, sees
a difference.

**The Worker protocol version moves from 8 to 10.** Client and Worker ship
together and are upgraded together by a normal install, so this affects only a
deployment that pins or caches a Worker file independently of the client
bundle. A mismatched pair fails cleanly with `PROTOCOL_MISMATCH`, and tabs
running different releases fail with `DATABASE_VERSION_MISMATCH`, rather than
misreading each other's results.

**Numbers keep every bit.** Numbers in JSON values and SQL text are now read
with correct rounding. Earlier releases read about one in ten numbers that
need all 17 significant digits, such as many results of `Math.random()`, as a
neighboring value. In v0.3.0 and earlier, a row holding one could then not be
read at all: reading it failed with `PAGED_STORAGE_CORRUPT`, because the row
no longer matched its stored text.

**Opening a database checks its catalog, not every row.** Earlier releases
read every row and index entry when opening a database, so opening took time
in proportion to its size, and refused a database that failed. Each page is
still checked when it is read, and the new check() reads the whole database on
demand, rejecting with the first problem it finds. A database whose damage no
statement reads now opens, and only check() reports it. See
[checking a database](/guides/storage-and-lifecycle/#checking-a-database).

This release is also much faster. In the
[comparative benchmarks](/guides/benchmarks/), most workloads in v0.3.0 took 10
to 1,400 times as long as the faster of SQLite and PGlite. None now takes as
much as twice as long, and reopening a database, reading every row, `LIKE`
scans, `GROUP BY`, joins, building indexes, and committing a single insert are
quicker than in either.

- Updates, upserts, and deletes inside a transaction no longer slow down as the
  transaction grows. Each statement is checked against running totals rather
  than the whole write set, so a thousand of them take about 50 ms rather than
  more than half a minute.
- Comparisons, `BETWEEN`, and `LIKE` patterns that start with literal
  characters read a range of an index or of the primary key, rather than every
  row. An aggregate that reads only the index's columns is answered from the
  index alone. See [constraints and indexes](/guides/sql-compatibility/#constraints-and-indexes)
  for when a range is used.
- Joins find each table's matching rows by key lookup, through an index, or in
  a hash table, rather than comparing every pair of rows. The join budget now
  counts the rows each lookup actually returns, so a join is never rejected
  because the Cartesian product of its tables is large.
- A query ordered by the leading primary-key columns reads rows in key order
  and stops at its `LIMIT`, rather than collecting and sorting every match, so
  it is no longer held to the limit on ordered matching rows. Inside a
  transaction that has changed the table, it still sorts.
- Scans read only the columns a statement uses, straight from the stored row,
  and so does a statement that finds its row by primary key.
- Each row a statement writes is planned, checked, and measured once. A
  transaction keeps the rows it stages as the encoded records its commit
  writes, rather than as maps it copies and encodes again at commit, and reads
  them in place, as it does stored rows.
- Writes apply each statement's rows to each B-tree in one pass, and index
  builds sort their entries first. A commit writes each page once, a run of
  consecutive pages with a single storage call, and makes them durable with one
  flush rather than three. Its superblock records a fingerprint of the pages it
  wrote, so reopening after an interrupted commit returns to the last complete
  one.
- Rows travel from the engine to the page as JSON text, which only the page
  parses, rather than as objects built one property at a time and then copied
  between threads. A point query takes about a quarter less time, and reading
  10,000 rows well under half. A statement that publishes nothing, such as a
  read or a statement inside a transaction, passes the rest of its result
  through the Worker as text too, and point statements take 4 to 7% less
  time.
- On the tab that owns a database, statements reach the engine as calls rather
  than as messages checked again at every layer.
- A plain `INSERT` plans each row straight into the record its commit writes,
  rather than into a map that staging encodes again. So does an `UPDATE` that
  leaves each row's key in place: it rewrites the stored record, keeping the
  bytes of every column it does not assign, and 1,000 updates by key take about
  a tenth less of the engine's time. A single-row upsert rewrites the stored
  row it conflicts with the same way.
- Staging a single-row statement no longer copies the transaction's
  bookkeeping, and each statement's request and result are checked, and its
  result read, with less work on both sides of the Worker. Point inserts,
  updates, and deletes in a transaction take 5 to 10% less time.
- Scans take each leaf's rows straight from the copy of the leaf they hold,
  checking the tree only as they move between leaves, and read an inline cell
  and an integer column without the general decoders. A range aggregate over
  10,000 rows takes about a quarter less time in the browser.
- A statement's changed rows are kept in the order its scan finds them, rather
  than sorted into maps as they arrive, so deleting 8,000 rows takes about a
  fifth less time. A `DELETE` outside a transaction also plans each row by its
  stored key, rather than as a map of its key columns, which takes a further
  fifth off.
- A result's and a commit's changed keys are ordered and deduplicated by their
  encoded keys, rather than by the JSON text of each, so each table's keys are
  listed in primary-key order: earlier releases listed `{"id": 10}` before
  `{"id": 9}`. They are kept as rows of values rather than a map for each key.
  Inserting 10,000 rows 200 at a time takes a tenth less of the engine's time,
  and a write by primary key 3% less.
- Inside a transaction that has changed a table, a scan finds the rows the
  transaction replaced leaf by leaf, rather than comparing every row's key with
  the next staged one, and an integer column is compared with integer bounds
  as integers. A transaction's 100 range `UPDATE`s take a fifth less of the
  engine's time, and 100 range aggregates about a tenth less.
- Writing a page stores each cell's fixed-size header and slot directly, rather
  than copying them a few bytes at a time, and an index key copies text whole.
  Creating two indexes over 10,000 rows takes 8% less time.
- Stored entries are fingerprinted with XXH64, which reads eight bytes at a
  time, rather than with FNV-1a, which hashed them a byte at a time. Every leaf
  a write creates fingerprints its entries, an index's included, so creating
  two indexes takes 9% less of the engine's time.
- A commit checksums each page with XXH64, which reads eight bytes at a time,
  rather than with CRC-32, which looks each byte up in a table, and passes over
  the zero bytes that fill most of each page. It encodes less besides, writing
  its allocation bitmap and superblock straight into their pages. Committing a
  single insert takes over a third less of the engine's time, and pages read
  back from storage are checked faster too.
- Each superblock carries the allocation bitmap for the first 123 MB of the
  database in the rest of its page, and a commit writes the bitmap's other
  pages only when it changes them, where it rewrote three bitmap pages every
  time. A commit to a smaller database writes its data pages and its
  superblock and nothing else: committing a single insert takes about 5% less
  time with OPFS.
- A write transaction looks for free pages 64 at a time, rather than one at a
  time from the start of the file, and a commit visits only the cached pages
  it wrote or freed, rather than every page in the cache. A commit's cost now
  grows much more slowly with the size of the database: committing single
  inserts into a 100,000-row table takes under half the engine's time it
  did.
- Opening a database reads its catalog rather than every row and index entry,
  so it no longer slows down as the database grows. Reopening the benchmark's
  10,000-row database takes about half as long, and a 100,000-row database
  with one index opens in 2 ms of the engine's time rather than 270.
- Several of the engine's small collections are kept in vectors rather than
  B-tree maps, whose code is compiled anew for each type they hold, which took
  about 15 KiB off the compressed engine. Its hash maps and sets share one
  quick hash rather than SipHash, whose resistance to colliding keys needs
  random keys that WebAssembly without a host source of randomness never gave
  it, and four sets that needed no hashing became vectors: 3 KiB more. The
  catalog's tables and indexes, and the keys a write reports, took 5 KiB
  more, and building the superblock's CRC-32 table when it is first needed,
  rather than shipping it, 1 KiB more.
- Once per page, create() also starts a short-lived second Worker that runs
  the engine's common statements on a scratch in-memory database for about a
  tenth of a second, and then exits. Chromium compiles WebAssembly one function
  at a time, as each is first called, and Workers running the same module
  share what either compiles, so a database's first statements of each kind no
  longer wait for their code to compile. See
  [custom Workers](/guides/custom-workers/).

The compressed download is now 301 KiB, up from 295 KiB in v0.3.0.

## v0.3.0

This release closes some of the most commonly encountered gaps in the
[SQL dialect](/guides/sql-compatibility/). Each form is new syntax that was
previously rejected, so an existing statement keeps its meaning.

`BETWEEN` and `NOT BETWEEN` are accepted wherever a `WHERE` clause is, including
in aggregates, joins, `UPDATE`, `DELETE`, and prepared statements.
`price BETWEEN $1 AND $2` means exactly `price >= $1 AND price <= $2`, with the
same type checking and `NULL` behavior as those two comparisons, and it is
inclusive at both ends. There is no `SYMMETRIC` form, so a range whose bounds
are reversed matches nothing rather than being swapped.

A single-table `SELECT` can now rename its columns with `AS`, as aggregate and
join queries already could: `SELECT id AS task_id, title FROM tasks`. The alias
becomes the result field name, `ORDER BY` can refer to it, and one column may be
returned under several names. As in PostgreSQL, an `ORDER BY` name matches an
output alias before a source column of the same name. Output names must still be
distinct.

`SELECT DISTINCT` removes duplicate rows from a single-table or join projection,
which is especially useful for collapsing the rows a many-to-many join
multiplies, as in
`SELECT DISTINCT tag.name FROM post_tags JOIN tags AS tag ON post_tags.tag_id = tag.id`.
`NULL`s compare as equal to one another, and `LIMIT` and `OFFSET` count distinct
rows. As in PostgreSQL, `ORDER BY` must name projected columns. The projection
must list its columns explicitly and cannot contain JSON columns or aggregates,
and `DISTINCT ON` and aggregate `DISTINCT`, such as `COUNT(DISTINCT column)`,
remain unsupported. A column named `distinct` remains usable.

`INSERT ... ON CONFLICT` makes an upsert one atomic statement rather than an
`UPDATE` followed by a conditional `INSERT` inside a transaction, which held
every tab sharing the database for the duration. `ON CONFLICT (id) DO NOTHING`
skips conflicting rows, and
`ON CONFLICT (id) DO UPDATE SET value = EXCLUDED.value` rewrites the stored
row. The target is the primary key or the columns of one unique index, and
`DO NOTHING` may omit it to treat every unique constraint as an arbiter. Rows
are handled one at a time as in PostgreSQL, so a statement that would update
the same row twice fails with `CONSTRAINT_VIOLATION`. `SET` accepts literals,
parameters, `DEFAULT`, and `EXCLUDED` columns, but cannot read the existing row
or change its primary key, and `DO UPDATE ... WHERE` is not supported.
`RETURNING`, the row count, and changed keys include only inserted and updated
rows, so a statement that skips every row reports no change. See
[upserts](/guides/sql-compatibility/#upserts).

`LIKE`, `ILIKE`, and their `NOT` forms match a text column against a pattern in
which `%` matches any run of characters and `_` exactly one, so a search box no
longer has to filter rows in JavaScript: `WHERE title ILIKE $1` with
`'%' + term + '%'`. Backslash escapes a literal `%` or `_` unless an `ESCAPE`
clause names another character, and a pattern that ends in its escape
character is rejected. A search term from user input should have its own `%`,
`_`, and backslash characters escaped before being wrapped in wildcards.
`ILIKE` folds only ASCII letters, as PostgreSQL does under the C locale, which
is consistent with TinyJoin's code-point ordering, so `'É' ILIKE 'é'` is false.
Pattern matching always scans candidate rows rather than using an index.

**Upgrading from v0.2.0 needs no action.** Install, rebuild, and redeploy. The
Worker protocol stays at version 8 and the page format stays at format 2, so an
existing database opens with no migration and no OPFS namespace change. The new
syntax was previously rejected, so statements that already worked keep their
results.

The engine is now built with Rust 1.98.1 rather than 1.91.1. That more than
offsets the new SQL, so the compressed WebAssembly engine is about 4 KiB
smaller than in v0.2.0.

## v0.2.0

Subscriptions and statement results now report **which rows changed**, not only
which tables. A [`TablesChangedEvent`](/api/tinyjoin/interfaces/subscriptions/tableschangedevent/)
and every [`Results`](/api/tinyjoin/interfaces/query-results/results/) carry a
`keys` map of table name to the primary keys that changed, so a listener can
refresh individual rows instead of re-reading a table. Keys are reported for
writes from any connected Client, which makes a change made in another tab
resolvable to individual rows. See
[refreshing individual rows](/guides/transactions-and-changes/#refreshing-individual-rows).

Reporting is a bounded best effort, and the changed-table list stays
authoritative. A table appears in `keys` only when every key it changed fits
the bound of 1,000 keys per table; a table that changed more rows is absent
rather than partially listed, so an absent table means its rows cannot be named
and must be re-queried. `keys` is always empty alongside `reset: true`. Code
that assumes keys are present will silently miss large writes, so handle the
absent case.

**The Worker protocol version moves from 7 to 8.** Client and Worker ship
together and are upgraded together by a normal install, so this affects only a
deployment that pins or caches a Worker file independently of the client
bundle. A mismatched pair fails cleanly with `PROTOCOL_MISMATCH` rather than
misreading a result. Applications using
[`tinyjoinOffline()`](/api/vite/functions/offline/tinyjoinoffline/) are unaffected: the
plugin revisions both files by content, so a rebuild replaces them together.

**Upgrading from v0.1.0 needs no action.** Install, rebuild, and redeploy.
Persistent storage is unchanged: the page format remains format 2, so an
existing database opens with no migration and no OPFS namespace change. The
`keys` field is additive, so application code that ignores it behaves exactly
as it did before.

Grouped and aggregate queries now use a secondary index when their predicate
supplies every column of one, instead of always scanning the table. Results are
unchanged, because the predicate is still evaluated for each candidate row.
Because the scan limit counts candidate rows, a grouped query that previously
exceeded `QUERY_WORK_LIMIT_EXCEEDED` on a large table may now succeed.

`tinyjoin/node` no longer fails to start when the parent process runs with a
flag that Node rejects for a worker thread, such as the `--stack-trace-limit`
that several test runners set. Inherited flags are dropped rather than the
database failing to open.

## v0.1.0

The new [Node entry point](/guides/node/), `tinyjoin/node`, opens in-memory
databases in Node.js 22 or later. It constructs a Worker thread and loads WASM
automatically, returning the same Client API without additional dependencies.
Each Client owns an independent database; filesystem persistence is not included.

**Persistent storage breaks compatibility with v0.0.5.** The npm v0.0.5
release uses page format 1; v0.1.0 uses page format 2. Opening a v0.0.5
database with v0.1.0 fails with `UNSUPPORTED_PAGE`. There is no automatic
migration, and changing the OPFS name does not copy existing data.

- For data that can be reconstructed, use a new OPFS namespace, for example
  change `opfs://my-app-v1` to `opfs://my-app-v2`, and rebuild the database.
  The old namespace remains intact.
- To retain existing data, export the application's known tables using a
  client pinned to `tinyjoin@0.0.5` before upgrading. Create the schema in a
  new namespace with v0.1.0, import rows with parameterized statements, and
  verify the data before retiring the old database. TinyJoin does not yet
  provide a general database export or migration API.
- New apps generated by create-tinyjoin v0.1.0 target `tinyjoin@^0.1.0` and
  use a `-db-v2` storage name. Updating the generator does not migrate
  applications it generated previously.

This release also makes predicate and assignment type validation consistent
across reads and writes, compares heterogeneous JSON values consistently,
and bounds shared parameter-graph traversal and expanded SQL bindings before
copying values. Append-only transactions now validate new row statements
incrementally, including unique-index checks; mixed writes retain complete
staged write-set validation.

Selective multi-table joins now use their actual comparison count rather than
a worst-case Cartesian estimate. Scan, build-row, result, memory, and shared
script-work limits remain in force.

Page checksum calculation is faster while preserving the stored checksum
values, storage format, and corruption checks. This improves mixed writes and
populated startup; the repository's [workload measurements](https://github.com/tinyplex/tinyjoin/tree/main/benchmarks)
retain the before/after samples and runtime hashes.

`UPDATE` and `DELETE` now use direct primary-key lookup for exact complete key
predicates, including staged transaction rows. Other predicates retain their
scan path, and statement validation and constraint checks remain in force.

Persistent startup avoids a redundant table scan for unique indexes whose
columns are all `NOT NULL`. Table validation and complete index-entry checks
remain in force; nullable indexes and indexes without uniqueness retain the
existing count scan.

Duplicate output names in simple projections and `RETURNING` now fail
consistently with `INVALID_QUERY` before execution. `LIMIT` and `OFFSET`
reject nonnumeric parameter values consistently in ordinary, aggregate, and
join queries, including JSON objects shaped like internal prepared placeholders.

Persistent Clients now coordinate automatically across tabs and within a page.
One elected Worker owns the database; operations, prepared statements, and
notifications follow it through handover. Callback transactions exclude other
Clients for their duration. In-flight operations interrupted by owner loss
reject without automatic replay, and incompatible releases reject explicitly.

The optional `tinyjoin/vite` build plugin precaches a complete production
application, including lazy Worker, OPFS, and WASM assets. New starter apps
enable it by default. Updates wait for old controlled tabs to close; existing
service workers can use its generated manifest and helper instead. See the
[offline guide](/guides/offline/).

## v0.0.5

This release establishes TinyJoin as a standalone, SQL-first browser database
package.

- create() opens the packaged dedicated Worker and WebAssembly engine with no
  application Worker boilerplate.
- Memory and persistent OPFS databases use one bounded page-native engine.
- The JavaScript client provides parameterized queries, atomic scripts,
  callback transactions, reusable prepared statements, and table-level change
  subscriptions.
- Typed tables support primary keys, maintained secondary and unique indexes,
  bounded schema additions, aggregates, and left-deep inner and left joins.
- Results use a familiar `rows`, `fields`, `affectedRows`, `command`, and
  `rowCount` shape, with TinyJoin revision and changed-table metadata.
- The published package includes its runtime, Worker, WASM, declarations,
  compatibility guide, README, release notes, and agent guidance.

TinyJoin remains experimental and deliberately smaller than PostgreSQL. This
release does not include a PostgreSQL server or wire protocol, hosted service,
remote replication, or offline-write synchronization.
