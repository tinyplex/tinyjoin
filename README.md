# TinyGres

TinyGres is an experimental, worker-first relational database for browser apps.
It provides a deliberately bounded PostgreSQL-shaped SQL surface in a small
Rust/WebAssembly engine kept off the browser's main thread.

> [!IMPORTANT] TinyGres is an early database prototype, not PostgreSQL compiled
> to WebAssembly. It intentionally implements only the documented SQL and type
> subset. It is a standalone database engine: the package contains no hosted
> service adapter, network client, or replication protocol.

The first proof of concept deliberately does a small number of things:

- owns an in-memory or opt-in persistent database inside a dedicated Web Worker;
- evaluates a documented subset of PostgreSQL-shaped SQL in Rust/WASM;
- supports typed tables, atomic DDL/DML, and staged transactions;
- persists the same page-native format in memory or one OPFS file;
- applies explicit table replacements and row-change batches atomically; and
- emits table-level invalidations so an application can re-query.

## Building from source

The repository root is a private development package. A build creates a clean,
standalone npm package in `dist/`, including its public `package.json`, compiled
TypeScript, worker entry, and WASM runtime:

```sh
npm install
npm run build
```

Run `npm pack ./dist` to inspect the publishable tarball. Publishing will also
be done from `dist/`, never from the private repository root. Generated starter
apps and demos belong in a separate `create-tinygres` package built on
`tinycreate`; this repository keeps only minimal fixtures used by automated
browser and package tests.

Application developers consume the prebuilt worker and WASM from the npm
package; Rust is not required in consuming projects. Building this repository
from source requires `rustup`. The checked-in toolchain file selects the Rust
version and `wasm32-unknown-unknown` target, while the npm development
dependency provides `wasm-pack`. The build stages wasm-bindgen output in a
temporary directory and copies only the page-native runtime JavaScript and
`.wasm` file into `dist/wasm`, so generated package metadata never appears under
`src/`. There is one engine artifact for both memory and OPFS storage.
Cargo's compiler cache lives under `node_modules/.cache/tinygres` when using
the project scripts rather than creating a top-level `target/` directory. Use
`npm run cargo -- <arguments>` for other Cargo commands with the same behavior.

If `rustc` comes from Homebrew, install rustup alongside it and activate the
rustup proxies in the current shell:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- --default-toolchain none -y
. "$HOME/.cargo/env"
npm run build:wasm
```

There is no need to uninstall Homebrew Rust. In persistent shell configuration,
load `$HOME/.cargo/env` after Homebrew's `brew shellenv` so the rustup-managed
`rustc` and `cargo` take precedence for direct Cargo commands. The WASM build
resolves the repository toolchain through rustup even when another compiler
appears first in `PATH`, then checks for the target before invoking `wasm-pack`.

## Browser API

```ts
import { create } from "tinygres";

type Task = {
  id: number;
  title: string;
  done: boolean;
};

const db = create({
  storage: { kind: "opfs", name: "my-app-v1" },
});

await db.exec(`
  CREATE TABLE tasks (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false,
    metadata JSONB
  )
`);

await db.exec("CREATE INDEX tasks_done ON tasks (done)");
await db.exec("CREATE UNIQUE INDEX tasks_title ON tasks (title)");
await db.exec(
  "ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
);

const inserted = await db.exec<Task>(
  `INSERT INTO tasks (id, title, metadata)
   VALUES ($1, $2, $3)
   RETURNING *`,
  [1, "Ship TinyGres", { priority: "high" }],
);

await db.transaction(async (tx) => {
  await tx.exec("UPDATE tasks SET done = true WHERE id = $1", [1]);
  await tx.exec("INSERT INTO tasks (id, title) VALUES ($1, $2)", [
    2,
    "Survives the same atomic commit",
  ]);
  // Queries inside the callback see staged rows. Other state is published only
  // after the callback and its outstanding operations complete successfully.
  console.log((await tx.query<Task>("SELECT * FROM tasks")).rows);
});

const summary = await db.query<{
  done: boolean;
  task_count: number;
}>(`
  SELECT done, COUNT(*) AS task_count
  FROM tasks
  GROUP BY done
  ORDER BY task_count DESC
`);

const unsubscribe = db.subscribe({ tables: ["tasks"] }, async (event) => {
  console.log("Tables changed", event.tables, event.revision);
  render((await db.query<Task>("SELECT * FROM tasks")).rows);
});

unsubscribe();
await db.close();
```

Every standalone SQL statement is atomic. `transaction()` stages `INSERT`,
`UPDATE`, and `DELETE` statements against an isolated candidate database,
rolls them back when the callback rejects (including for an uncaught statement
error), persists one complete commit to OPFS, and then emits one table-level
invalidation. A caught statement error does not abort the transaction or erase
earlier staged writes. Run DDL such as `CREATE`, `ALTER`, and `DROP` as
standalone atomic statements. The transaction object must not escape its
callback.

For callers that already hold complete JSON rows, `replaceTable(schema, rows)`
atomically replaces one table and `applyBatch({changes})` atomically applies
explicit `upsert` and `delete` operations. These are local database operations;
they perform no I/O beyond the configured database storage.

`create` starts opening the database in a dedicated module worker and returns
the client immediately. `exec()`, `query()`, and the other async operations
wait for initialization automatically. Call `ready()` only when an application
needs to observe opening or initialization separately. It is safe
to import during server rendering; the worker is only constructed when the
function is called in a browser.

`query()` returns `{revision, rows}`. `exec()` accepts exactly one supported
read or write statement and returns
`{command, revision, rowCount, rows, tables}`, including any `SELECT` or
`RETURNING` rows. These are TinyGres result objects rather than PostgreSQL wire
results; see the [SQL compatibility contract](./docs/sql.md) for the exact
dialect, type semantics, and limits.

For an application-owned worker, pass `worker`, `workerFactory`, or `workerUrl`:

```ts
const db = create({
  workerFactory: () =>
    new Worker(new URL("./tinygres.worker.ts", import.meta.url), {
      name: "tinygres",
      type: "module",
    }),
  schemas: [{ name: "posts", primaryKey: ["id"] }],
});
```

An application-owned worker starts the same standalone engine:

```ts
// tinygres.worker.ts
import { startWorker } from "tinygres/worker";

startWorker();
```

## OPFS persistence

Memory remains the default. Opt into persistent browser storage by assigning a
stable name to the database:

```ts
const db = create({
  schemas: [{ name: "posts", primaryKey: ["id"] }],
  storage: { kind: "opfs", name: "my-project-public-posts-v1" },
});

await db.ready();
```

Names must contain 1–64 ASCII letters, numbers, dots, underscores, or hyphens,
and start with a letter or number.

`ready()` opens the page database and validates or initializes its schemas. The
page engine uses 4 KiB copy-on-write pages:
candidate data and catalog pages are flushed before one checksummed metadata
publication makes the new generation visible. A known pre-publication failure
leaves the previous generation authoritative. An uncertain final publication
poisons the open engine and requires a reopen, which deterministically selects
the complete old or new generation.

Each logical name maps to the single OPFS file
`tinygres-pages-v1/db-<name>/database.pages`. Its synchronous access handle is
both the page device and the exclusive database lock; there is no sidecar
snapshot, journal, authority marker, or second persistence engine.

OPFS persistence is deliberately single-writer. A second Worker opening the
same name fails rather than risking concurrent mutation; closing or terminating
the owning Worker releases the lock. Different names are independent. Include
the application, dataset, and schema version in the name. A name is a namespace,
not an encryption or access-control boundary.

Synchronous OPFS access requires a secure context and a dedicated Worker. It is
not available in a `SharedWorker`. There is no silent fallback to memory when
OPFS is requested but unavailable, locked, corrupt, or out of quota.

Browser storage is still reconstructable cache data: users can clear it and a
browser may evict best-effort storage under pressure. Applications that need
stronger retention can make an explicit, user-appropriate
`navigator.storage.persist()` request; TinyGres does not make that policy
decision during startup.

The physical database is capped at 65,536 pages (256 MiB). Its default page
cache is 16 MiB, and mutation, query-result, join, and aggregate working sets
retain their independent 16 MiB logical bounds. Ordinary queries and writes
therefore do not deserialize the complete database into WASM memory or copy it
through `postMessage`.

TinyGres writes only its page-native format. Earlier experimental
checkpoint/journal and staged-migration layouts were never released and are not
recognized or migrated. Use a new logical storage name, or clear experimental
OPFS data, when moving a development app to this format.

## Fluent query builder

The initial builder intentionally exposes only the implemented surface:

```ts
const { data, error } = await db
  .from<Task>("tasks")
  .select("id, title, done")
  .eq("done", true)
  .gte("priority", 2)
  .order("id", { ascending: false })
  .range(0, 19);
```

This syntax queries TinyGres directly and never makes a network request. Values
are represented as JSON-compatible values.

## SQL compatibility

TinyGres is intentionally much smaller than a full PostgreSQL implementation.
It has a custom parser and page engine, five JSON-compatible runtime types, and
no PostgreSQL server, wire protocol, catalogs, extensions, roles, or data-file
compatibility. Familiar SQL spelling does not imply support for an unlisted
PostgreSQL feature.

The authoritative [SQL compatibility contract](./docs/sql.md) provides a
statement-and-keyword matrix, predicate and type matrices, transaction and
concurrency differences, unsupported feature families, and hard operational
limits. The summary below describes the main implemented slice.

TinyGres currently accepts one statement at a time. `SELECT` supports:

- one unqualified or two-part table name;
- `*` or a list of simple column names;
- `=`, `<>`/`!=`, `<`, `<=`, `>`, and `>=` comparisons;
- `AND`, `OR`, `NOT`, parentheses, `IN`/`NOT IN`, and `IS [NOT] NULL`
  with SQL three-valued null logic;
- string, number, boolean, `NULL`, or PostgreSQL-style `$1` parameters; and
- simple multi-column `ORDER BY` with `ASC`/`DESC` and
  `NULLS FIRST`/`NULLS LAST`;
- optional non-negative `LIMIT` and `OFFSET`.

A two-part name such as `public.tasks` is stored as one flat TinyGres table
name. There is no PostgreSQL schema namespace or `search_path`, and the
unqualified name `tasks` does not resolve it.

Raw SQL also supports a bounded single-table aggregate form:

- `COUNT(*)`, `COUNT(column)`, `SUM`, `AVG`, `MIN`, and `MAX`;
- simple group columns through `GROUP BY`, with explicit `AS` aliases;
- the existing `WHERE` predicates before grouping; and
- `ORDER BY` projected output names or aliases, followed by `LIMIT`/`OFFSET`.

Aggregate queries require a typed catalog, so this first slice applies to
SQL-created tables rather than untyped schemas that expose only column names.

`COUNT(column)`, `SUM`, `AVG`, `MIN`, and `MAX` skip `NULL`. A global
aggregate over no matching rows produces one row (`COUNT` is zero and the
others are `NULL`), while a grouped empty input produces no rows. `SUM` and
`AVG` accept integer/float columns; `MIN` and `MAX` accept integer, float, or
text columns. Integer sums fail rather than silently crossing JavaScript's
safe-integer boundary.

Typed local tables can be joined with a deliberately bounded relational form:

```sql
SELECT post.id AS post_id, tag.name AS tag_name
FROM posts AS post
JOIN post_tags AS post_tag ON post.id = post_tag.post_id
JOIN tags AS tag ON post_tag.tag_id = tag.id
ORDER BY post_id, tag_name
```

- two to eight typed table sources in a left-deep chain, using `JOIN`/`INNER
  JOIN` or `LEFT [OUTER] JOIN` at each step;
- optional bare or `AS` table aliases and qualified column references;
- one or more column equalities in each `ON`, combined with `AND`, with at most
  32 across the query; every equality connects the incoming table to a table
  already in the chain;
- the existing predicates in `WHERE`, plus `ORDER BY`, `LIMIT`, and `OFFSET`;
  and
- explicit column projections with distinct output names, using `AS` where
  multiple tables contain the same column name.

Every source requires a typed catalog and a unique alias. Unqualified
references are accepted only when exactly one source contains the column;
ambiguous references are rejected. Join steps are evaluated in written,
left-to-right order. `NULL` join keys do not match, and a left join represents
columns from its unmatched incoming table as `NULL`; a later inner join can
therefore remove that null-extended row. Integer and float keys can be compared,
while JSON join keys are rejected. Without `ORDER BY`, joined row order is not
part of the contract. Quoted table aliases and column names remain
case-sensitive, but this join slice rejects literal dots inside them.

Many-to-many relationships use an ordinary bridge table, conventionally with
a composite primary key as in `post_tags` above. TinyGres does not implement
foreign keys, so it does not enforce that bridge rows reference existing rows.

This implementation uses a bounded left-deep nested-loop execution path without
a join optimizer. It rejects a ninth table source, `OR` or non-equality
expressions in `ON`, and `SELECT *`. Across the complete chain it permits at
most 1,000,000 candidate row extensions, 100,000 retained build rows, 100,000
result rows, and separate 16 MiB join working-state and result-data budgets.
Before reading rows, it also rejects a chain whose source row counts imply more
than 1,000,000 worst-case candidate extensions, without assuming that an `ON`
condition will be selective.
Aggregates over joins remain outside this slice.

Standalone writable databases additionally support:

- `CREATE TABLE` and `CREATE TABLE IF NOT EXISTS` with a required inline or
  table-level primary key;
- column types `BOOLEAN`, `SMALLINT`/`INTEGER`/`BIGINT`, `REAL`/`DOUBLE
PRECISION`, `TEXT`/`VARCHAR`, and `JSON`/`JSONB`;
- literal defaults, `NULL`/`NOT NULL`, and composite primary keys;
- `CREATE [UNIQUE] INDEX [IF NOT EXISTS]` over boolean, integer, and text
  columns, including composite indexes;
- `ALTER TABLE ... ADD [COLUMN] [IF NOT EXISTS]` with literal/default
  backfilling, plus `DROP TABLE [IF EXISTS]` and `DROP INDEX [IF EXISTS]`;
- multi-row `INSERT ... VALUES`, filtered `UPDATE`, and filtered `DELETE`;
- `$1` parameters, `DEFAULT`, and simple `RETURNING *`/column lists; and
- callback transactions through `db.transaction()`.

Complete primary-key equality uses direct row lookup. A complete equality match
for every column of a secondary index uses its maintained postings; partial
composite matches, ranges, `OR`, and `NOT` currently scan the table. Unique
indexes follow PostgreSQL's default behavior of allowing multiple keys that
contain `NULL`. Text comparison and ordering use deterministic Unicode code-point
ordering rather than PostgreSQL database collations.

Integers are restricted to JavaScript's exactly representable safe-integer
range. Type names are compatibility spellings over this smaller runtime type
set: for example, `BIGINT` does not provide 64-bit values and `JSONB` currently
uses JSON-compatible values. `NULL = NULL` does not match, following
SQL null semantics.

Aliases on ordinary non-aggregate, non-join projections, `HAVING`, aggregate
`DISTINCT`/`FILTER`/window forms, subqueries, general expressions, multiple or
non-equijoins, foreign keys, `ON CONFLICT`, sequences/generated IDs, type
modifiers, and SQL `BEGIN` tokens are rejected explicitly; the error code
depends on the form. `ALTER` is
currently limited to adding a column; renaming or removing columns is not
implemented. This is an explicit compatibility boundary, not an accidental
promise of full PostgreSQL behavior.

## Development and validation

```sh
npm run typecheck       # authored TypeScript source and unit tests
npm run test:ts         # client, RPC, and worker-host unit tests
npm run test:rust       # native Rust engine tests
npm run build           # assemble the complete publishable dist package
npm run test:browser    # real browser Worker/WASM flow
npm run test:package    # pack dist and install it in a clean Vite app
npm run check:size      # hard 700 KiB page-native WASM gate
```

The browser tests initialize the real module Worker, query, apply an atomic row
change, receive an invalidation, and re-query, plus exercise persistence through
a real dedicated Worker and OPFS restart. The writable proof creates a typed
table, inserts and updates inside a transaction, verifies rollback after a
constraint failure, closes the Worker, and reopens the committed state.

The packed-package test separately proves SSR-safe import, declarations, a
production Vite build, and real browser execution through both the packaged
default worker and an application-owned worker. It covers memory and fresh OPFS
write/reopen flows in both modes and installs the tarball rather than resolving
TinyGres through a workspace link.

The single page-native WASM artifact has a hard uncompressed limit of 700 KiB.
The check is intentionally independent of gzip size so compression cannot hide
startup and compilation cost.

The production page artifact enables WebAssembly SIMD. The current automated
browser compatibility proof is Chromium in a dedicated Worker; TinyGres does
not yet claim Firefox or WebKit support for this artifact, and it never silently
falls back to another engine when compilation is unavailable.

## Direction

The browser runtime uses the same bounded page-native engine for both memory and
OPFS storage. The next direction is streamed result cursors and cancellation so
even the bounded 16 MiB result model need not be materialized at once. The
initial bounded join slice establishes qualified column references
without making ambiguous row semantics part of the public contract. Full
PostgreSQL catalogs, extensions, server concurrency, and arbitrary wire
compatibility are not goals. External service integrations can be designed as
separate packages later, against concrete requirements and a stable engine API.

TinyGres is MIT licensed.
