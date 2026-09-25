# Benchmarks

TinyJoin is measured against the two databases most often chosen for the same
job: SQLite's official WebAssembly build and PGlite. All three run in a
dedicated Worker, store their data in the origin private file system (OPFS),
and are driven from the page by the same workloads in the same Chromium.

These numbers are published to track progress, not to win an argument.
TinyJoin's engine is young. It is already small and quick to open, but most
reads and writes are currently much slower than in either alternative, and
closing that gap is ongoing work. The suite is designed to be rerun after
every optimization.

{{benchmarks.environment}} The shortest time in each row is bold, and each bar
is scaled to the slowest engine in its row.

## Measured results

### Download and startup

{{benchmarks.startup}}

Download size counts every file a page fetched to open a database, compressed
with gzip at level 9: JavaScript for the page and the Worker, WebAssembly, and
PGlite's file system image. First open starts before the engine is fetched and
ends when the first query on a new, empty database returns. Reopen runs in a new
browser session with a warm HTTP cache: it loads the engine again, opens an
existing database, and counts its rows.

### Create

{{benchmarks.create}}

### Read

{{benchmarks.read}}

### Update

{{benchmarks.update}}

### Delete

{{benchmarks.delete}}

### Schema

{{benchmarks.schema}}

## What the results show

In the v0.3.0 results above, TinyJoin is the smallest download and the quickest
to create a new database: PGlite initializes a new PostgreSQL cluster on first
open. Nearly everything after that is slower in TinyJoin, and the gaps point
directly at the work ahead.

- **Updates, deletes, and upserts inside a transaction** are the largest gap,
  at three orders of magnitude. Once a transaction contains anything but
  appended rows, TinyJoin validates the complete write set after every
  statement, so the cost of each statement grows with the transaction.
  Append-only inserts already validate incrementally.
- **Range predicates do not use secondary indexes.** Only equality does, so the
  indexed range aggregates take as long as the unindexed ones, while SQLite
  and PGlite answer them from the index.
- **Scans are slow.** Every unindexed query, `LIKE` predicate, `GROUP BY`, and
  bulk `DELETE` reads all 10,000 rows, and TinyJoin reads rows far more slowly
  than either engine. Creating an index over those rows is also slow.
- **Joins run as nested loops** without index lookups, repeating that scan
  cost for every row of the outer table.
- **Multi-row inserts are no faster than single-row inserts.** In SQLite and
  PGlite, batching rows into fewer statements is the quickest way to load
  data; in TinyJoin, it currently saves nothing.
- **Reopening validates the stored trees**, so a populated database takes
  longer to reopen in TinyJoin than in SQLite.

The closest results are the ones dominated by messages between the page and
the Worker: point reads by primary key, reading every row, and committing
single-row writes one at a time. There, TinyJoin is within a small factor of
both engines, and reads single rows faster than PGlite.

## Features are not equivalent

The workloads use only SQL that all three engines accept, which is TinyJoin's
bounded dialect. That favors TinyJoin: SQLite and PGlite do far more, and an
application that needs what they add should use them. The
[caveats](/guides/caveats/) and the
[SQL compatibility contract](/guides/sql-compatibility/) describe TinyJoin's
boundaries in full.

| | TinyJoin | SQLite | PGlite |
| --- | --- | --- | --- |
| SQL dialect | A bounded, PostgreSQL-shaped subset | SQLite | PostgreSQL |
| Subqueries, CTEs, set operations | No | Yes | Yes |
| Expressions, casts, scalar functions | No | Yes | Yes |
| `HAVING`, window functions | No | Yes | Yes |
| Views, triggers | No | Yes | Yes |
| Joins | Up to eight sources, evaluated as written, without index lookups | Query planner, with indexes | Query planner, with indexes |
| `INSERT ... SELECT`, `UPDATE ... FROM` | No | Yes | Yes |
| Upserts | `ON CONFLICT`, assigning values from `EXCLUDED` | Yes | Yes |
| Types | Boolean, safe integer, float, text, JSON | Dynamic: integer, real, text, blob | The PostgreSQL type system |
| JSON | Stored, returned, and compared for equality | JSON functions | `json` and `jsonb` operators and functions |
| Full-text search | No | FTS5 | `tsvector` |
| Extensions | No | Compiled in only | PostgreSQL contrib extensions |
| Transactions | A callback API | SQL, with savepoints | SQL, with savepoints |
| Worker | create() owns it | The Worker1 API, or the application's own | `PGliteWorker`, or the application's own |
| Tabs sharing a database | Automatic, with owner handover | Not with `opfs-sahpool`, which is exclusive | `PGliteWorker` leader election |
| Change notifications | Changed tables and primary keys | Update hooks in the C API | Live queries, `LISTEN`/`NOTIFY` |
| Node.js | In memory | In memory | In memory or on disk |
| Maturity | Experimental, v0.x | Decades of production use | The PostgreSQL engine, in a v0.x package |

## How the benchmark works

### The engines

{{benchmarks.versions}}

SQLite uses `opfs-sahpool`, the SQLite project's fastest OPFS file system. Like
TinyJoin's storage, it holds its files exclusively, and it needs no
cross-origin isolation headers. PGlite uses `opfs-ahp`, its OPFS file system.
Every engine keeps its default durability: no relaxed flushing, no
`PRAGMA synchronous`, and no in-memory journal.

### One page, three Workers

A single Vite production build contains a small harness and one chunk per
engine, so a page downloads only the engine it opens. Each engine is reached
through the same five calls: exec() for a parameter-free script, query() for
one statement, prepare() and run() for a repeated statement, and
transaction() for a callback.

- TinyJoin's adapter maps those calls onto create() and the Client API, which
  owns its Worker.
- SQLite and PGlite each run in a Worker of a few dozen lines, answering one
  message per statement. The SQLite Worker caches prepared statements by SQL
  text; PGlite receives the SQL text with every statement.
- A transaction is one round trip per statement for every engine: SQLite and
  PGlite send `BEGIN` and `COMMIT` as statements, and TinyJoin sends each
  statement inside transaction().

Every query result is delivered to the page as an array of row objects, so
timings include the message from the Worker, as an application would
experience it.

### Isolated samples

Every sample launches Chromium with a fresh profile in a new temporary
directory. The profile is on disk, not an incognito context, whose storage would
be held in memory. No engine ever sees another's files or warm caches, and a
sample that exceeds the time limit can be killed without affecting the next.
Engines take turns within each round of samples, so gradual changes in machine
load affect them equally.

In each sample, the harness opens a new database, runs the workload's untimed
setup, and then times only the workload itself with performance.now(). Setup
loads rows as multi-row `INSERT` statements in one transaction.

### Checked results

Every workload ends with an untimed check, such as a row count, a column sum,
or a total accumulated from the query results. The runner compares the checks
of all three engines, and fails if any disagree, so every engine is known to
have done the same work.

### Workloads

All tables have an integer primary key. The main table has 10,000 rows with a
sequential integer, a random integer below 100,000, that number spelled out in
English words, and one of 100 group numbers. The data is generated from a fixed
seed, so it is identical for every engine and every run.

{{benchmarks.workloads}}

Several workloads are adapted from the classic SQLite database speed
comparison, which [PGlite](https://pglite.dev/benchmarks) also publishes
results for. The adaptations add a primary key to every table, use smaller
counts, and replace the forms TinyJoin cannot run: arithmetic in `UPDATE ...
SET` becomes a literal assignment, and the `INSERT ... SELECT` tests are
omitted.

The join places 5,000 orders across 100 customers. With 10,000 orders, the
query would exceed TinyJoin's 1,000,000-comparison
[join budget](/guides/sql-compatibility/#join-work-budgets) and fail, because
TinyJoin's nested loop does not use the index on `customer_id`.

A sample that runs longer than 60 seconds is abandoned and reported as such,
and that engine skips the workload's remaining samples.

### What this does not measure

- One desktop machine and one browser. Mobile devices, Firefox, and Safari
  are not measured.
- Assets are served over loopback HTTP, so download sizes are counted, but
  network transfer time is not.
- Other configurations can be faster or slower: SQLite's `opfs` file system,
  community builds such as wa-sqlite, PGlite with IndexedDB or
  `relaxedDurability`, and TinyJoin with in-memory storage.
- Concurrency across tabs, very large databases, and long-lived storage
  fragmentation.

## Reproducing the results

The benchmark lives in `benchmarks/compare` in the
[repository](https://github.com/tinyplex/tinyjoin). SQLite and PGlite are
installed there, pinned by its own lockfile, rather than in the project's
dependencies. The runner installs them on first use.

```sh
npm ci
npm run build
npm run bench:compare -- --publish
npm run build:docs
```

Use the Node.js version in `.node-version`, since compressed sizes depend on
its zlib. A full run takes about half an hour on the machine above. Close
other applications and avoid concurrent builds or tests while it runs.

`--publish` requires the full suite and writes every sample, the environment,
and the list of downloaded files to `site/data/benchmarks.json`, from which the
documentation build renders these tables.

When working on TinyJoin's performance, run a subset instead. The runner prints
each engine's median and TinyJoin's ratio to the fastest:

```sh
npm run build
npm run bench:compare -- --engines tinyjoin,sqlite --workloads update-pk,delete-pk --samples 3
```

`--storage memory` runs the same workloads without OPFS, which separates the
engine's own cost from storage. `--out` saves a report to compare against a
later build, and `--help` lists every option.
