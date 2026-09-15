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

## Multiple tabs share one writer

Clients using the same OPFS name automatically share one database-owning
Worker. Election, routing, prepared statement restoration, and cross-tab
notifications are internal. Different names and browser storage partitions
remain independent.

This is one serialized engine. An open transaction holds other Clients until
its callback finishes, and a frozen live owner or transaction holder can delay
other tabs until it resumes or closes. Keep callbacks short. Closing or losing
the owner triggers automatic election, but requests already sent to it fail
with `LEADER_CHANGED` and are never silently replayed. Incompatible releases
fail with `DATABASE_VERSION_MISMATCH`; old versions that do not coordinate
can still hold the underlying OPFS lock. See
[tab handover](/guides/storage-and-lifecycle/#tab-handover-and-subscriptions).

## It needs a modern browser

TinyJoin requires WebAssembly and dedicated module Workers, and persistence
additionally requires a secure context, OPFS synchronous access handles, Web
Locks, and BroadcastChannel. It
deliberately does not use `SharedArrayBuffer`, so a page does not need
cross-origin isolation headers.

The automated browser suite currently runs on Chromium only. Firefox and WebKit
are not verified, and a successful TypeScript or Vite build says nothing about
them. Test the browsers an application actually targets.

There is also no fallback to IndexedDB or memory when persistent storage is
unavailable, locked, corrupt, or out of quota. create() rejects instead.

TinyJoin does not automatically restore a Client closed during page teardown.
If the browser restores that page from its back/forward cache, the application
must reopen its Client and recreate statements and subscriptions, or reload
the page. The [lifecycle guide](/guides/storage-and-lifecycle/#opening-and-closing)
describes this boundary.

## Browser storage is not durable storage

OPFS is browser-managed. A user can clear it, and a browser may evict
best-effort storage under pressure. A `navigator.storage.persist()` request is
a request, not a guarantee, and TinyJoin does not make that product decision
during startup.

Treat a TinyJoin database as reconstructable local state. Data that has to
survive needs a copy the application controls.

A storage or commit-result failure can leave a write's outcome uncertain.
`RECOVERY_REQUIRED`, `STORAGE_COMMIT_OUTCOME_UNKNOWN`, and
`STORAGE_ENGINE_POISONED` require closing and reopening the Client. Reconcile
the recovered rows with stable operation identifiers before any replay;
`retryable` does not guarantee that replay is safe. The
[recovery guide](/guides/storage-and-lifecycle/#recovering-after-an-uncertain-write)
explains how to preserve that distinction.

## Transactions need bounded callbacks

Nested callback transactions are unsupported: awaiting another transaction()
on the same Client inside its callback deadlocks. Pass the active Transaction
to helpers. There is no AbortSignal or built-in timeout; Promise.race() stops
waiting without cancelling work, which can still commit. Read the
[transaction guide](/guides/transactions-and-changes/#composing-transaction-helpers)
before composing asynchronous application work.

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

Multi-tab routing also bounds each Client's pending requests and the owner's
waiting queue to 256 requests and approximately 8 MiB each, with a small
reserved allowance for transaction cleanup. Hitting these bounds rejects with
`RESOURCE_LIMIT`; await work or batch related writes. Prepared engine limits
are shared across connected Clients; at most 128 Clients can attach to an owner.

That boundedness is deliberate, but it does mean TinyJoin is sized for
application state rather than for analytics over a large dataset.

## Performance depends on the workload

Running SQL off the main thread keeps engine work out of the page's rendering
loop. It does not guarantee low latency or throughput on every browser and
device. Measure the application's schema, query shapes, and storage mode.

Transactions that only append new primary keys use incremental statement
validation. Mixed writes still validate the complete staged write set after
each statement and can have quadratic staging cost. Multi-row statements can
reduce that overhead; see [inserting many rows](/guides/transactions-and-changes/#inserting-many-rows).

Aggregates scan their input table, and transaction queries currently do not
use secondary indexes for lookup acceleration. Opening a persistent database
validates its stored trees, so startup cost grows with the stored data. Commit
and browser storage costs are separate from statement staging.

A local before/after check of incremental insertion used 1,000 awaited
prepared inserts in one transaction, with an integer primary key and a short
text value. Three-run medians on an Apple M2 in Chromium, using the packaged
default Worker, were:

| Storage | Staging before / after | Whole transaction before / after |
| --- | --- | --- |
| Memory | 1,324 / 62 ms | 1,557 / 304 ms |
| OPFS | 1,327 / 56 ms | 1,571 / 310 ms |

Commit time remained about 230–250 ms. These are diagnostic measurements of
one insertion workload, not a controlled comparison with other databases or
a browser support/performance guarantee. The repository retains the samples
and runtime hashes under `benchmarks/`; after `npm run build`, reproduce the
browser workload with `node scripts/benchmark-browser-inserts.mjs` and the
separate engine/unique-index workload with `node scripts/benchmark-staging.mjs`.

After the checksum, primary-key lookup, and startup optimizations, a five-sample
Chromium/M2 check used a primary key, a unique title
index, and 64-byte payloads. Mixed transactions of 25, 100, and 250 operations
took median totals of about 24 ms, 296 ms, and 2.2 seconds respectively.
Reopening a 5,000-row database with 1-KiB payloads took about 2.2 seconds;
materializing its 5.34-MB result took about 1.1 seconds. Owner and follower
measurements, distributions, runtime hashes, and reproduction commands are
retained in the [workload report](https://github.com/tinyplex/tinyjoin/tree/main/benchmarks).

For that measured shape, keep mixed batches in the tens of operations and
bound returned rows and bytes. Larger batches can hold every Client using the
same OPFS name for seconds. These samples cover one desktop and Chromium
version; they do not establish a phone, low-end device, or cross-browser
performance envelope. Measure the application's actual schema and devices
before choosing its batch and result sizes.

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
