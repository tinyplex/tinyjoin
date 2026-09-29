# Benchmarks

TinyJoin is measured against the two databases most often chosen for the same
job: SQLite's official WebAssembly build and PGlite. All three run in a
dedicated Worker, store their data in the origin private file system (OPFS),
and are driven from the page by the same workloads in the same Chromium.

These numbers are published to track progress, not to win an argument.
TinyJoin's engine is young. It is the smallest download and the quickest to
open or reopen a database, and it reads every row, runs `LIKE` scans, groups,
joins, builds indexes, and commits single inserts more quickly than either
alternative. With OPFS storage, it is the fastest of the three in half of the
workloads, and second in the other half, taking one to 1.5 times as long as the
faster engine: it is never the slowest. Closing the remaining gaps is ongoing
work, and the suite is designed to be rerun after every optimization.

{{benchmarks.environment}}

Every chart has a linear axis from zero, so bar lengths compare directly, and
every bar is labeled with its value. In each group, the best value is bold.

## Measured results

### Download and startup

{{benchmarks.startup}}

Download size counts every file a page fetched to open a database: JavaScript
for the page and the Worker, WebAssembly, and PGlite's file system image. It is
shown uncompressed, compressed with gzip at level 9, and compressed with Brotli
at quality 11, since servers send either. First open starts before the engine
is fetched and ends when the first query on a new, empty database returns.
Reopen runs in a new browser session with a warm HTTP cache: it loads the engine
again, opens an existing database, and counts its rows.

Every sample starts in a new browser profile, so each engine's WebAssembly is
compiled afresh, one function at a time as each is first called. As it does in
any page, TinyJoin's create() also starts a short-lived second Worker that
compiles the engine's common statements on a scratch database, so its timed
statements mostly run already compiled. SQLite and PGlite compile theirs as
they go. See [custom Workers](/guides/custom-workers/).

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

In the results above, TinyJoin is the smallest download and the quickest to
create a new database, where PGlite initializes a new PostgreSQL cluster, and
to reopen one. It reads all 10,000 rows in order four times as fast as either
engine, runs `LIKE` scans, `GROUP BY`, and joins faster than either, builds
indexes and commits single inserts faster than either, and deletes rows in
bulk faster than SQLite. Indexed range aggregates and point reads take as long
as in the faster engine. The rest is slower, but no longer by orders of
magnitude: in v0.3.0, most workloads took 10 to 1,400 times as long as the
fastest engine, and none now takes as much as twice as long. The remaining
gaps point at the work ahead.

- **Single statements** cost about 30 to 36 microseconds each, which is 1.0
  to 1.5 times SQLite's cost for a point read, or an update, upsert, or delete
  by primary key. About 14 microseconds of every round trip is the message
  between the page and the Worker, which every engine pays. A point read costs
  as much as SQLite's, but a write in a transaction costs 9 to 11
  microseconds more: in TinyJoin's Worker, in planning, checking, and staging
  the row, and in its share of the commit.
- **Scans** take under a tenth longer than SQLite's. TinyJoin reads each
  column in place from the stored row, and each leaf's rows from its own copy
  of the leaf, and compares an integer column with integer bounds directly. A
  range `UPDATE` inside a transaction also passes over the rows the
  transaction has already changed, finding them leaf by leaf, and takes about
  1.3 times as long as SQLite's.
- **Inserts** one at a time take about 1.3 times as long as SQLite's, for
  the same reasons as single statements, and 200 at a time about 3% longer.
- **Bulk deletes** take about 1.2 to 1.3 times as long as PGlite's, which, like
  PostgreSQL, only marks deleted rows and leaves reclaiming their space to a
  later vacuum. TinyJoin removes each row and its index entries at once, and
  rewrites every page they occupied, and is still quicker than SQLite at both.
- **Committing each insert alone** is dominated by the storage flush, one per
  commit, and writes only the pages the insert changed and the superblock. It
  takes an eighth less time than PGlite's commits, and a quarter as long as
  SQLite's.
- **Reopening** reads the database's catalog rather than every row, so a
  populated database reopens in under three-fifths of SQLite's time, and a
  large one as quickly as a small one. The full check of every row and index
  entry runs only when an application calls check().

## In memory

The same workloads also run with each engine keeping its database in memory
rather than in OPFS: TinyJoin's `memory://`, SQLite's `:memory:`, and PGlite's
`memory://`. Nothing reaches storage, so these results measure each engine's
own work, and their difference from the results above is what its storage
costs. Reopening a database does not apply.

{{benchmarks.memory-environment}}

### Download and startup in memory

{{benchmarks.memory-startup}}

### Create in memory

{{benchmarks.memory-create}}

### Read in memory

{{benchmarks.memory-read}}

### Update in memory

{{benchmarks.memory-update}}

### Delete in memory

{{benchmarks.memory-delete}}

### Schema in memory

{{benchmarks.memory-schema}}

In memory, TinyJoin is again the quickest to open, and still reads every row,
runs `LIKE` scans, groups, and joins faster than either engine. Without
storage costs, though, it is the slowest at deleting rows in bulk and at
building indexes. Comparing the two sets of results shows what storage costs
each engine:

- **Committing each insert alone** takes TinyJoin about 83 microseconds in
  memory and 436 with OPFS, so most of an OPFS commit is the storage flush.
  SQLite commits in memory in about 28 microseconds. TinyJoin still writes,
  checksums, and records every page each commit changes, as it does for OPFS.
- **Bulk deletes and index builds** take TinyJoin about as long in memory as
  with OPFS, since it writes the pages they change in a few large calls, while
  SQLite's take a quarter to two-fifths as long in memory, and PGlite's about a
  quarter less. What remains is TinyJoin's engine, which takes about 1.7 to 1.9
  times as long as the faster engine to delete thousands of rows, and 1.4 times
  as long to build indexes.
- **Single statements and transactions** cost about the same either way: a
  transaction commits once, and a single statement's time is spent between the
  page, the Worker, and the engine.

The in-memory run followed the run above on the same machine, and each
engine's reads took about as long in both. Machine load still differs between
runs, so compare engines within one run rather than across the two.

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
| Joins | Up to eight sources, evaluated as written, through key lookups, indexes, or hash tables | Query planner, with indexes | Query planner, with indexes |
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

The join places 5,000 orders across 100 customers, and each query reads one
customer's orders through the index on `customer_id`.

A sample that runs longer than 60 seconds is abandoned and reported as such,
and that engine skips the workload's remaining samples.

### What this does not measure

- One desktop machine and one browser. Mobile devices, Firefox, and Safari
  are not measured.
- Assets are served over loopback HTTP, so download sizes are counted, but
  network transfer time is not.
- Other configurations can be faster or slower: SQLite's `opfs` file system,
  community builds such as wa-sqlite, and PGlite with IndexedDB or
  `relaxedDurability`.
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
npm run bench:compare -- --publish --samples 9
npm run bench:compare -- --publish --storage memory --samples 9
npm run build:docs
```

Use the Node.js version in `.node-version`, since compressed sizes depend on
its zlib. A full run of nine samples takes about ten minutes on the machine
above. Close other applications, pause background work such as photo library
analysis, and avoid concurrent builds or tests while it runs: a busy machine
slows every engine, and a burst of load can land on some workloads and not
others.

`--publish` requires the full suite and writes every sample, the environment,
and the list of downloaded files to `site/data/benchmarks.json`, or with
`--storage memory` to `site/data/benchmarks-memory.json`, from which the
documentation build renders these charts.

When working on TinyJoin's performance, run a subset instead. The runner prints
each engine's median and TinyJoin's ratio to the fastest:

```sh
npm run build
npm run bench:compare -- --engines tinyjoin,sqlite --workloads update-pk,delete-pk --samples 3
```

`--storage memory` runs the same workloads without OPFS, which separates the
engine's own cost from storage. `--out` saves a report to compare against a
later build, and `--help` lists every option.
