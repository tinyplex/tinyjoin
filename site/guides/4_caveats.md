# Caveats

TinyJoin is small on purpose, and being small has costs. This page collects
what an application signs up for, in one place, so that the decision gets made
before the schema does.

If any of these are unacceptable, there are
[more mature projects](#if-tinyjoin-is-not-the-right-fit) that solve the same
problem with different trade-offs.

## It is experimental

TinyJoin is at v0.0.6. The JavaScript API, the SQL dialect, the error codes,
and the page format can all change between releases, and a release may require
an application to recreate its persistent database rather than migrate it.

Version the OPFS name alongside the schema generation, as in
`opfs://my-app-v1`, so that a breaking change becomes a new database rather
than a broken one. The [release notes](/guides/releases/) record each release's
compatibility boundary.

## The SQL is a bounded subset

TinyJoin implements its own PostgreSQL-shaped dialect. It is not PostgreSQL
compiled to WebAssembly, and familiar syntax is implemented only where the
smaller runtime can give it clear and deterministic semantics. Unsupported
forms are rejected explicitly rather than silently reinterpreted.

The omissions most likely to matter are:

- No subqueries, CTEs, `UNION`/`INTERSECT`/`EXCEPT`, or `DISTINCT`.
- No arithmetic, concatenation, casts, or scalar functions. Values are not a
  general expression language.
- No `LIKE`/`ILIKE`, `BETWEEN`, `ANY`/`ALL`, or JSON path operators, so
  substring search and ranged text matching have to happen outside SQL.
- No `ON CONFLICT` upsert, `INSERT ... SELECT`, `MERGE`, or `UPDATE ... FROM`.
- No sequences, `SERIAL`, or generated identity. Generate text identifiers in
  the client.
- No `NUMERIC`/`DECIMAL`, date, time, interval, `UUID`, `BYTEA`, array, enum,
  or user-defined types. Five runtime types cover boolean, integer, float,
  text, and JSON.
- No `HAVING`, distinct or filtered aggregates, window functions,
  `RIGHT`/`FULL`/`CROSS` joins, views, or triggers.

The [SQL compatibility contract](/guides/sql-compatibility/) is the exact list.
Read it before designing a schema, not after.

## One writer, and effectively one tab

A persistent database opens with an exclusive OPFS synchronous access handle. A
second Worker for the same `opfs://` name fails to open rather than risking
concurrent mutation, and in practice a second Worker means a second tab of the
same application. There is no silent fallback to memory.

TinyJoin does not coordinate tabs. It uses no `SharedWorker`,
`BroadcastChannel`, or Web Locks, and a
[subscription](/guides/transactions-and-changes/) only reports changes made
through its own Client.

An application that needs a real multi-tab story has to build one:

- Catch the open failure and tell the user the application is already open in
  another tab. This is the smallest honest option.
- Or elect one writer tab and route queries to it over a channel you own.
- Or open a memory database in secondary tabs when they only need a snapshot.

## It needs a modern browser

TinyJoin requires WebAssembly and dedicated module Workers, and persistence
additionally requires a secure context and OPFS synchronous access handles. It
deliberately does not use `SharedArrayBuffer`, so a page does not need
cross-origin isolation headers.

The automated browser suite currently runs on Chromium only. Firefox and WebKit
are not verified, and a successful TypeScript or Vite build says nothing about
them. Test the browsers an application actually targets.

There is also no fallback to IndexedDB or memory when persistent storage is
unavailable, locked, corrupt, or out of quota. create() rejects instead.

## Browser storage is not durable storage

OPFS is browser-managed. A user can clear it, and a browser may evict
best-effort storage under pressure. A `navigator.storage.persist()` request is
a request, not a guarantee, and TinyJoin does not make that product decision
during startup.

Treat a TinyJoin database as reconstructable local state. Data that has to
survive needs a copy the application controls.

## There is no server, and no sync

TinyJoin has no PostgreSQL wire protocol, server process, roles or grants,
system catalogs, extensions, stored procedures, WAL, replication, or
point-in-time recovery. It does not synchronize with a remote database and does
not propagate offline writes. Its storage is TinyJoin's own page format, not a
PostgreSQL data directory.

## The limits are hard limits

Oversized work fails explicitly rather than growing until the tab dies. A
persistent database is bounded to 256 MiB, one query returns at most 100,000
rows, a join chains at most eight table sources, and SQL text, parameters,
prepared statements, and working memory each have a named bound. The complete
list is in [hard limits](/guides/sql-compatibility/#hard-limits).

That boundedness is deliberate, but it does mean TinyJoin is sized for
application state rather than for analytics over a large dataset.

## Performance is unmeasured

TinyJoin runs its engine off the main thread, which keeps a page responsive
while queries run, and its bounded planner has no room for the pathological
cases that come with a general optimizer. Neither of those is a throughput
claim: there are no published benchmarks yet. Measure with the schema and query
shapes an application actually uses.

## If TinyJoin is not the right fit

These projects are larger, more mature, or both, and are the better answer when
the caveats above are not acceptable:

- [PGlite](https://pglite.dev/) is real PostgreSQL compiled to WebAssembly,
  with the full dialect and extensions. Choose it when genuine PostgreSQL
  compatibility matters more than download size.
- [SQLite Wasm](https://sqlite.org/wasm/) is the official SQLite build for the
  browser, with an OPFS backend, decades of stability, and a much larger SQL
  surface.
- [wa-sqlite](https://github.com/rhashimoto/wa-sqlite) is SQLite for the
  browser with pluggable storage backends, including ones designed for
  concurrent tabs.
- [DuckDB-Wasm](https://github.com/duckdb/duckdb-wasm) is columnar analytics in
  the browser, for aggregate queries over large datasets.
- [Dexie](https://dexie.org/) is a typed IndexedDB wrapper, for when relational
  SQL is not the requirement and broad browser support is.
- [TinyBase](https://tinybase.org/) is a reactive data store with persistence
  and synchronization, for when the requirement is local-first sync rather than
  SQL.

None of that is a criticism of those projects. TinyJoin exists because a useful
subset of the same problem fits in a much smaller download.
