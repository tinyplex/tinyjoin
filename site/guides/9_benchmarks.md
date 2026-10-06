# Benchmarks

TinyJoin is measured against the two databases most often chosen for the same
job: SQLite's official WebAssembly build and PGlite. All three run in a
dedicated Worker, store their data in the origin private file system (OPFS),
and are driven from the page by the same workloads in the same Chromium.

These numbers are published to track progress, not to win an argument.
TinyJoin's engine is young. It is the smallest download and the quickest to
open or reopen a database, and it reads every row, reads rows by key, scans,
runs `LIKE` scans, groups, joins, updates rows by range, builds indexes,
inserts 200 rows at a time, and commits single inserts more quickly than
either alternative, and runs indexed range aggregates as quickly as SQLite.
With OPFS storage, it is the fastest of the three in 13 of the 20 workloads,
and second in the other seven, taking at most 1.4 times as long as the faster
engine: it is never the slowest. Closing the remaining gaps is ongoing work,
and the suite is designed to be rerun after every optimization.

{{benchmarks.environment}}

Each workload lists the engines fastest first, and is drawn to its own linear
scale from zero, on which its slowest run reaches the full width, so a quick
workload's bars are as legible as a slow one's. A bar's length and value are
the median of an engine's runs, and a faint bracket at its end spans that
engine's fastest run to its slowest.

## Download

{{benchmarks.download}}

Download size counts every file a page fetched to open a database: JavaScript
for the page and the Worker, WebAssembly, and PGlite's file system image. It is
shown uncompressed, compressed with gzip at level 9, and compressed with Brotli
at quality 11, since servers send either.

## Startup

{{benchmarks.startup}}

First open starts before the engine is fetched and ends when the first query on
a new, empty database returns. Reopen runs in a new browser session with a warm
HTTP cache: it loads the engine again, opens an existing database, and counts
its rows.

Every sample starts in a new browser profile, so each engine's WebAssembly is
compiled afresh, one function at a time as each is first called. As it does in
any page, TinyJoin's create() also starts a short-lived second Worker that
compiles the engine's common statements on a scratch database, so its timed
statements mostly run already compiled. SQLite and PGlite compile theirs as
they go.

Compiled is not yet optimized. A browser optimizes a function only once it has
run many times, and every timed workload is the first run of its statements in
its page, so every engine's statements run faster when run again: a thousand
point reads by key take TinyJoin about 21 ms and SQLite about 25 ms by the
fifth run in one page, against 27 and 29 the first time. See [custom Workers](/guides/custom-workers/).

## Create

{{benchmarks.create}}

## Read

{{benchmarks.read}}

## Update

{{benchmarks.update}}

## Delete

{{benchmarks.delete}}

## Schema

{{benchmarks.schema}}

## What the results show

In the results above, TinyJoin is the smallest download and the quickest to
create a new database, where PGlite initializes a new PostgreSQL cluster, and
to reopen one. It reads all 10,000 rows in order seven times as fast as either
engine, reads rows by key in about 6% less time than SQLite, runs indexed range
aggregates as quickly, runs `LIKE` scans, `GROUP BY`, and joins faster than
either, builds indexes and commits single inserts faster than either, and
deletes rows in bulk faster than SQLite. Across the 20 timed workloads, it
places like this, where engines that tie share a place:

{{benchmarks.placings}}

Where it comes second, it takes at most 1.4 times as long as the faster
engine.

- **Single statements** cost about 27 to 31 microseconds each. About 12
  microseconds of every round trip is the message between the page and the
  Worker, which every engine pays. A point read by primary key takes about 8%
  less time than SQLite's, but an update, upsert, or delete by primary key
  takes 1.3 to 1.4 times as long: a write in a transaction costs about 7
  microseconds more. Warm, the difference is about 5 microseconds, in the
  engine's planning, staging, and result, and in the page's reading of the
  result. The benchmark's 1,000 statements run mostly WebAssembly that V8 has
  not yet optimized, which runs TinyJoin's many small functions almost twice
  as slowly as optimized code, where SQLite's few large functions are
  optimized almost at once.
- **Scans** take about three-fifths of SQLite's time. A predicate's simple
  terms are tested against each leaf's rows in one pass over the leaf's bytes,
  and only the rows they accept are decoded. A range `UPDATE` inside a
  transaction also passes over the rows the transaction has already changed,
  finding them leaf by leaf, and takes about five-sixths of SQLite's time.
- **Inserts** one at a time take about 1.2 to 1.3 times as long as SQLite's,
  for the same reasons as single statements, and 200 at a time about as long.
- **Bulk deletes** take about 1.2 times as long as PGlite's, which, like
  PostgreSQL, only marks deleted rows and leaves reclaiming their space to a
  later vacuum. TinyJoin removes each row and its index entries at once, and
  rewrites every page they occupied, and is still quicker than SQLite at both.
- **Committing each insert alone** is dominated by the storage flush, one per
  commit, and writes only the pages the insert changed and the superblock. It
  takes about a tenth less time than PGlite's commits, and about a quarter as
  long as SQLite's. Its time is mostly the flushes, so it varies most of any
  workload with the state of the disk.
- **Reopening** reads the database's catalog rather than every row, so a
  populated database reopens in under three-fifths of SQLite's time, and a
  large one as quickly as a small one. The full check of every row and index
  entry runs only when an application calls check().

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
| Subqueries, CTEs, set operations | Uncorrelated `IN (SELECT ...)`, and nested queries that gather rows into JSON | Yes | Yes |
| Expressions, casts, scalar functions | Arithmetic and `\|\|`, without casts or functions | Yes | Yes |
| `HAVING`, window functions | No | Yes | Yes |
| Views, triggers | No | Yes | Yes |
| Joins | Up to eight sources, evaluated as written, through key lookups, indexes, or hash tables | Query planner, with indexes | Query planner, with indexes |
| `INSERT ... SELECT`, `UPDATE ... FROM` | No | Yes | Yes |
| Upserts | `ON CONFLICT`, assigning expressions of the stored and proposed rows | Yes | Yes |
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
Engines take turns within each round of samples, and each round starts with
the next engine, so gradual changes in machine load affect them equally. Before
each round, the runner times a short computation, and waits until it runs
within 5% of its time at the start: a laptop slows as it heats over a long run,
and background work takes its cores, and either would otherwise weigh on
whichever workloads ran then.

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
counts, and replace the forms TinyJoin could not run when they were written:
arithmetic in `UPDATE ... SET` becomes a literal assignment, and the
`INSERT ... SELECT` tests, which it still cannot run, are omitted.

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
npm run build:docs
```

Use the Node.js version in `.node-version`, since compressed sizes depend on
its zlib. A full run of nine samples takes about ten minutes on the machine
above. Close other applications, pause background work such as photo library
analysis, and avoid concurrent builds or tests while it runs: a busy machine
slows every engine, and a burst of load can land on some workloads and not
others. The runner prints how long its CPU probe took at the start and at its
slowest, and how long it waited for the CPU to recover.

`--publish` requires the full suite and writes every sample, the environment,
and the list of downloaded files to `site/data/benchmarks.json`, from which the
documentation build renders these charts.

When working on TinyJoin's performance, run a subset instead. The runner prints
each engine's median and TinyJoin's ratio to the fastest:

```sh
npm run build
npm run bench:compare -- --engines tinyjoin,sqlite --workloads update-pk,delete-pk --samples 3
```

`--out` saves a report to compare against a later build, and `--help` lists
every option.
