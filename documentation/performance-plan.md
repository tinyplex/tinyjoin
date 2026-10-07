# Performance plan

TinyJoin v0.3.0 is the smallest download and the quickest first open in the
[comparative benchmarks](../site/guides/9_benchmarks.md), but most reads and
writes are one to three orders of magnitude slower than SQLite. This plan
records where that time goes and the order in which to remove it. Reads come
first, and the largest read fix is a new storage format.

The evidence comes from four sources, gathered in September 2026 on an Apple
M2:

- the published browser results in `site/data/benchmarks.json`;
- native profiles of `tinyjoin-core` driven directly, without a Worker or
  WASM;
- engine-only WASM runs in Node, using the shipped `-Oz` pipeline;
- scratch prototypes of the first fixes, all passing the core test suite.

Wall times in the native and Node figures were noisy, because the machine was
under heavy load. Instruction counts, profile shares and A/B ratios are the
reliable figures.

## Progress

As of 28 September 2026, `main` carries this plan through most of its steps,
one commit per step, each gated on the Rust, TypeScript, browser and size
checks. The ratios below are to the faster of SQLite and PGlite in the same
run, so machine load largely cancels out. The later columns are runs published
in `site/data/benchmarks.json`: 26 September with five samples, two on 27
September with nine, on commits `64ce856` and `23fe19c`, and two on 28
September with nine, on `5a3e1d7` and `a9e4b08`. Three earlier runs on 27
September were discarded because background load inflated every engine's
times; the published ones were made with macOS media analysis paused, and their
SQLite times match the quiet 26 September run's. The `23fe19c` column adds
planning inserted rows straight into records and the warm-up Worker, the
`5a3e1d7` column the per-statement work and size reductions of 28 September,
measured while other applications kept the machine busier, the `a9e4b08`
column that evening's bulk-write, checksum, scan, and size work, and the
`b3ebbaf` column, published on 29 September, the scan, delete, update, index
build, and size work of that night. SQLite's times in those two runs are
within 2% of each other on average. The `82ec602` column, published the next
morning after two noisier runs were discarded, adds that night's
per-statement work: short parameters copied rather than encoded, results that
committed nothing passed through the Worker as text, changed keys ordered by
their encoded keys and kept as rows of values, and a catalog in sorted
vectors. The `73042cc` column, published that afternoon after two runs spoiled
by heat and other applications were discarded, adds parameters read directly,
the lone upsert's record path, each superblock carrying the start of the
allocation bitmap, and commits whose cost no longer grows with the file or the
page cache. The `e6ed5eb` column, published that evening after a run in which
TinyJoin's single-insert commits alone ran more than twice as slowly for ten
minutes was discarded, adds the owner tab's statements served as they arrive,
lookups by primary key encoded from the predicate's values, and records
encoded into one buffer. The `2515773` column, published that night, adds
XXH64 page checksums and entry fingerprints, and an open that reads only the
catalog, with the full check moved to check().

| Workload | v0.3.0 | 26 Sep | `64ce856` | `23fe19c` | `5a3e1d7` | `a9e4b08` | `b3ebbaf` | `82ec602` | `73042cc` | `e6ed5eb` | `2515773` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `cold-open` | 0.7× | 0.8× | 0.71× | 0.59× | 0.60× | 0.59× | 0.61× | 0.68× | 0.59× | 0.59× | 0.59× |
| `reopen` | 3.5× | 1.2× | 1.2× | 1.1× | 1.2× | 1.1× | 1.05× | 1.1× | 1.1× | 1.1× | 0.57× |
| `insert-autocommit` | 3.3× | 2.1× | 1.1× | 1.1× | 0.98× | 0.93× | 0.90× | 1.1× | 0.90× | 0.90× | 0.88× |
| `insert-transaction` | 9.9× | 2.5× | 1.7× | 1.7× | 1.6× | 1.5× | 1.6× | 1.4× | 1.4× | 1.35× | 1.3× |
| `insert-indexed` | 15× | 2.3× | 1.8× | 1.6× | 1.5× | 1.6× | 1.6× | 1.3× | 1.3× | 1.3× | 1.3× |
| `insert-batch` | 33× | 3.4× | 2.0× | 1.6× | 1.6× | 1.4× | 1.5× | 1.1× | 1.1× | 1.04× | 1.03× |
| `select-pk` | 4.2× | 1.6× | 1.2× | 1.1× | 1.1× | 1.0× | 1.0× | 0.99× | 1.0× | 0.99× | 1.00× |
| `select-scan` | 105× | 1.75× | 1.7× | 1.6× | 1.6× | 1.5× | 1.1× | 1.1× | 1.05× | 1.06× | 1.06× |
| `select-like` | 39× | 0.9× | 0.95× | 0.94× | 0.92× | 0.92× | 0.86× | 0.79× | 0.79× | 0.80× | 0.80× |
| `select-indexed` | 1,406× | 2.4× | 1.6× | 1.2× | 1.1× | 1.1× | 1.0× | 1.0× | 1.0× | 0.97× | 1.00× |
| `select-all` | 2.3× | 0.6× | 0.28× | 0.23× | 0.24× | 0.25× | 0.25× | 0.25× | 0.23× | 0.24× | 0.23× |
| `group-by` | 22× | 0.95× | 1.0× | 0.86× | 0.87× | 0.89× | 0.77× | 0.72× | 0.72× | 0.72× | 0.75× |
| `join` | 175× | 1.4× | 1.1× | 0.88× | 0.86× | 0.88× | 0.82× | 0.82× | 0.82× | 0.82× | 0.81× |
| `update-pk` | 1,077× | 2.3× | 1.8× | 1.8× | 1.8× | 1.7× | 1.6× | 1.2× | 1.5× | 1.4× | 1.35× |
| `update-scan` | 179× | 2.2× | 2.1× | 2.1× | 2.1× | 2.0× | 1.5× | 1.2× | 1.3× | 1.3× | 1.3× |
| `upsert` | 1,362× | 2.5× | 1.8× | 1.8× | 1.9× | 1.7× | 1.7× | 1.5× | 1.6× | 1.5× | 1.5× |
| `delete-pk` | 1,105× | 2.6× | 1.8× | 1.8× | 1.7× | 1.6× | 1.7× | 1.3× | 1.5× | 1.5× | 1.5× |
| `delete-like` | 95× | 3.7× | 2.7× | 1.9× | 2.1× | 1.6× | 1.6× | 1.7× | 1.3× | 1.4× | 1.3× |
| `delete-range` | 249× | 3.7× | 2.5× | 2.3× | 2.2× | 1.6× | 1.4× | 1.4× | 1.3× | 1.3× | 1.2× |
| `create-index` | 142× | 1.3× | 1.1× | 1.0× | 1.1× | 1.1× | 1.0× | 0.77× | 0.96× | 0.95× | 0.94× |

The compressed download grew from 296 KiB to 334 KiB, and is now 302 KiB.
Between two runs on the same build, the ratios moved by up to a tenth, and by
more under load, so read them to one significant figure. In the `23fe19c`
column, TinyJoin's own times fell by up to a third, most on the workloads whose
code the warm-up Worker compiles ahead of them: indexed ranges, bulk inserts,
joins, grouping, and deletes. SQLite was about 7% faster in the `5a3e1d7` run,
so single-statement writes barely moved against it, and about 8% slower in the
`a9e4b08` run, in which bulk deletes fell to 1.6 times PGlite's, quicker than
SQLite's. A full rerun on `1d9e6e7`, after the scan fast paths, put range
aggregates at 1.2 times SQLite's and a transaction's range `UPDATE`s at 1.8
times, but ran under more load, with SQLite about 15% slower than in the
`a9e4b08` run, so it was not published. In the `b3ebbaf` run, range aggregates
took 1.1 times SQLite's time and a transaction's range `UPDATE`s 1.5 times, the
largest ratio left fell from 2.0 to 1.7, for upserts, and creating indexes and
indexed range aggregates matched the faster engine. In memory, range
aggregates fell from 1.5 times SQLite's to 1.07, and range `UPDATE`s from 2.3
to 1.6.
The `82ec602` run's SQLite log-sum matched the `b3ebbaf` run's, but it hid a
shift: SQLite's reads ran about a sixth faster, and its OPFS writes a tenth to
a half slower, so the write ratios in that column fell partly because SQLite's
writes did. TinyJoin's own times fell by 2-29%, its point writes by 11-17%,
while engine-only in Node the same commits cut its point writes by 19-21% and
left bulk deletes and scans unchanged. In memory, where SQLite ran about a
sixth faster than in the `b3ebbaf` run, point writes fell from 2.0-2.2 times
SQLite's to 1.9-2.2, and 200-row inserts from 1.7 to 1.4.
The `73042cc` run is the quietest yet, with SQLite log-sums of 75.27 with OPFS
and 61.77 in memory, but SQLite's times shifted again: against the `82ec602`
run, its updates and deletes by key ran a seventh to a fifth faster, its bulk
deletes a quarter faster, and its commits a fifth slower. TinyJoin's point
writes therefore rose to 1.5-1.6 times SQLite's although its own times held,
as a head-to-head run of the builds from before and after that day's engine
work confirmed, within a few percent. Committing each insert alone took every
engine longer than in that run, PGlite most, so TinyJoin's commits, at 0.90
times PGlite's, gained on it partly for that reason; engine-only, carrying the
bitmap in the superblock cut a commit's work by 6%. `LIKE` and range deletes
fell to 1.3 times PGlite's, and creating indexes, though a sixth quicker than
before, rose to 0.96 times PGlite's, whose index builds ran a third faster.
In memory, committing each insert alone fell from 94 microseconds to 87, point
writes from 1.9-2.2 times SQLite's to 1.7-1.9, and inserts in a transaction
from 1.4-1.5 to 1.3-1.4.
The `e6ed5eb` run was as quiet, with SQLite log-sums of 75.10 with OPFS and
61.75 in memory, and SQLite's times held within a few percent of the
`73042cc` run's. With OPFS, TinyJoin's updates by key took 6% less time,
inserts in a transaction 5%, 200-row inserts 7%, reads by key 4%, and upserts
and deletes by key 2-3%, so no workload now takes more than 1.5 times the
faster engine's time, and reads by key match SQLite's. Scans, bulk deletes and
index builds held; the `LIKE` delete's rise from 1.3 times PGlite's to 1.4 is
within the spread of both runs' samples. In memory, where the owner tab's lane
does not apply, point statements took 1-3% less time.
The `2515773` run was as quiet, with SQLite log-sums of 75.07 with OPFS and
61.71 in memory; a first in-memory run, at 62.81, was discarded. With OPFS,
reopening fell from 45 ms to 22, 0.57 times SQLite's, since opening reads only
the catalog; the `LIKE` delete fell 13%, creating indexes 8%, and updates and
deletes by key 3-4%, while the rest held within a few percent. TinyJoin was
the fastest engine in 10 of the 20 workloads and second in the rest. In
memory, committing each insert alone fell from 90 microseconds to 83, creating
indexes by 12%, and the `LIKE` delete by 10%.

The `f91bee22` run, published on 6 October after the night's work below,
with SQLite's log-sum at 74.55, among the lowest yet: TinyJoin was the
fastest engine in 11 of the 20 workloads and second in the rest, never the
slowest, at most 1.41 times the faster engine. Range aggregates without an
index fell from 1.06 times SQLite's time to 0.58, `LIKE` aggregates from 0.80
to 0.50, a transaction's range `UPDATE`s from 1.28 to 0.82, and 200-row
inserts from 1.10 to 1.06; updates, upserts and deletes by key stayed at 1.3
to 1.4, and inserts in a transaction at 1.25 to 1.3. Single committed inserts,
which a run on `6cb477fe` an hour earlier (SQLite's log-sum 74.43, the lowest
yet) had at 0.91 of PGlite's time, came out at 1.04 in this one: that
flush-bound workload moves by a tenth between runs. The compressed download
is 348 KiB.

The `c60f510c` run, published on 7 October after the shared table names and
buffers below: TinyJoin was the fastest engine in 11 of the 20 workloads and
second in the rest, never the slowest. The 8,000-row range delete fell from
1.20 times PGlite's time to 1.08, 200-row inserts from 1.06 times SQLite's to
1.00, and single committed inserts came out at 0.85 of PGlite's time; updates,
upserts and deletes by key stayed at 1.25 to 1.5, inserts in a transaction at
1.2 to 1.3, and the `LIKE` delete at 1.3 times a PGlite run faster than its
others. Two earlier runs on the same commit were discarded: for seconds at a
time, every engine's commits took two to three times as long, which the CPU
probe does not see, and the 1,000 single-insert commits fell in those
stretches for six of TinyJoin's nine samples. The run kept is the one with the
lowest sum of the logarithms of all three engines' medians, 231.1 against
231.6 to 232.3 for the four before it, measured with Docker's virtual machine,
Spotlight's importers and Activity Monitor paused. The compressed download is
352 KiB.

The `cbab3dd6` run, published on 7 October after the point insert's resolved
values and skipped staged search, with the same processes paused and a CPU
probe never more than 2.6 ms slow, the quietest of the night: TinyJoin was the
fastest engine in 13 of the 20 workloads and second in the rest, never the
slowest, at most 1.35 times the faster engine. 200-row inserts fell from 1.00
times SQLite's time to 0.94, inserts in a transaction from 1.28 to 1.23, and
indexed range aggregates came out at 0.97 of SQLite's time and single
committed inserts at 0.91 of PGlite's; updates, upserts and deletes by key
stayed at 1.30 to 1.35, inserts into an indexed table at 1.23, the `LIKE`
delete at 1.17 times PGlite's time and the range delete at 1.10. The
compressed download is 353 KiB.

The `66805a22` run, published on 7 October after the day's work on bulk DML
and the overlay, the first with the flush gate (which waited 110 s in all,
read 0.24-0.98 ms a flush, and left no sample unrecovered) and the balanced
engine order: TinyJoin was the fastest engine in 14 of the 20 workloads and
second in the rest, never the slowest, at most 1.34 times the faster engine.
The `LIKE` delete fell from 1.17 times PGlite's time to 0.95 and the range
delete from 1.10 to 0.98, 200-row inserts from 0.94 times SQLite's to 0.87,
inserts into an indexed table from 1.23 to 1.19, range `UPDATE`s from 0.82
to 0.78 and upserts from 1.31 to 1.26; updates and deletes by key stayed at
1.2 to 1.35 and inserts in a transaction at 1.23. A run an hour earlier on
the same commit was discarded: five of TinyJoin's nine single-insert commit
samples took two to three times as long while the flush probe read a quiet
disk before each of them, so the gate cannot see that condition, which the
next bullets investigate. The compressed download is 351 KiB.

The `8df67174` run, published on 8 October after the flat statement messages
and the day's engine steps: TinyJoin was the fastest engine in 18 of the 20
workloads and second in the other two, at most 1.07 times the faster engine.
Updates by key fell from 1.20 times SQLite's time to 0.93, upserts from 1.26
to 0.99, deletes by key from 1.34 to 1.02, inserts in a transaction from 1.23
to 0.97, inserts into an indexed table from 1.19 to 0.96, selects by key from
0.91 to 0.74, and indexed range aggregates from 1.06 to 0.80. The `LIKE`
delete read 1.07 times PGlite's time, 4.3 ms against 4.0 where the run before
read 4.1 against 4.3, and the range delete 1.00; interleaved, the two builds'
`LIKE` deletes read 4.20 and 4.25 ms, and the range delete 6.10 and 5.65.
Opening a new database read 57.5 ms against 53.9, and 51.7 at v0.5.0:
interleaved, the two builds read 53.5 and 55.3, so about 2 ms of it is this
work, which the larger client and Worker cost before their first statement,
and the rest is the run. A run fifty minutes earlier on the same commit was
discarded. Six of TinyJoin's nine single-insert commit samples took two to
three times as long, each of them the sample after a PGlite sample or the
workload's first, with the flush probe reading a quiet disk before every one,
and SQLite's samples after PGlite's were 0.1 to 0.9 s slower too; that run
waited on its gates 355 times where this one waited 130, and a run of PGlite
and TinyJoin alone between the two showed no slow sample. The nine rounds put
TinyJoin after PGlite five times and SQLite four, so when the disturbance is
there TinyJoin's median is a disturbed sample and SQLite's is not: a run of
twelve rounds through every arrangement twice would put each after PGlite six
times, and neither median would be spared. The compressed download is 353
KiB.

Done:

- Phase 0, the native benchmark.
- R1 to R7. D3 and D4 were adopted as recommended, and the SQL guide
  documents both.
- R8's typed `GROUP BY` keys and once-compiled `LIKE` patterns.
- R9, and D5 as recommended: rows travel as JSON text that only the page
  parses, and the Worker checks each result's header only.
- Page format 3: S1 to S4. Rows a transaction stages are kept as the
  records its commit writes, and each written row is checked and measured
  once. D1, D6 and D8 were decided as recommended.
- T1 and T2.
- C1 to C5. A commit writes each run of consecutive pages with one storage
  call and flushes once; its superblock carries a hash of the pages it wrote,
  and recovery returns to the previous root when they did not all reach
  storage.
- O1 follows from R1.
- An aggregate whose columns an index holds is answered from the index's
  entries without reading the table.
- The owning tab reaches its engine through direct calls rather than messages,
  and serves a statement at once when nothing is queued ahead of it.
- A statement's single change becomes its overlay entry directly, a
  statement's entries are kept in vectors rather than maps, and its effect on
  each table's changed-row count is applied once validated, rather than
  checked against a copy of every count. An insert allocates 33 blocks rather
  than 37, and 2.4 KB rather than 6.5 KB.
- Protocol messages are checked with `Object.keys`, a result is read into
  Results as its response arrives, and a ready client sends a direct statement
  at once, rather than after turns of the microtask queue.
- A statement's changed rows are kept in the order they arrive, and sorted
  only if a key arrives out of order, rather than inserted into maps; a table
  with more changes than a change event can name is found without collecting
  any keys. A range delete of 8,000 rows takes a fifth less engine time.
- Checksums pass runs of zero bytes 256 at a time. A commit counts allocated
  pages as they change, encodes its superblock once, and the in-memory device
  copies a page it holds in place. Committing one insert takes a third less
  engine time.
- Scans take a leaf's rows from the cursor's copy of it, checking the cursor's
  view once per leaf, and pass rows straight to their visitor when no work
  budget is charged. The cursor's step reads an inline cell itself, a column
  reader decodes an integer itself, and the pages a walk visits are hashed as
  the cache hashes them. Range aggregates take 29% less engine time, and a
  quarter less in Chromium.
- A stored row's size estimate starts from a total its table's layout works
  out once, and reads only its text and JSON columns.
- Several small collections moved from B-tree maps to vectors, and three maps
  stopped removing entries: the engine is 15 KiB smaller compressed.
- A scan inside a transaction that changed its table passes over the rows the
  transaction replaced by finding them in each leaf, by binary search from the
  last one found, rather than comparing every row with the next staged key
  through a closure (T3's merge). A key search reads only each cell's key.
  Filters compare an integer column with integer bounds as integers, through
  a range worked out once. Engine-only, a transaction's 100 range `UPDATE`s
  fell from 71 ms to 57, and 100 range aggregates from 48 ms to 44.
- A script's `DELETE` plans each stored row as a `Remove` of its encoded key,
  charged as the map of its key columns through a per-table estimate, rather
  than as that map; a transaction still plans maps, which its overlay
  measures. The 8,000-row range delete fell from 5.4 ms to 4.4 engine-only.
- An `UPDATE` that keeps each row's key, with no `RETURNING`, rewrites the
  stored record, keeping every unassigned column's bytes, and plans it as a
  `Put`. A row whose JSON text could pass the row limit, and a table keeping a
  JSON column the statement does not assign, still plan maps. 1,000 updates by
  key fell from 12.3 ms to 11.1 engine-only; the 2 KiB this was set aside for
  shrank to 1.2 KiB by sharing the map path's accounting.
- Writing a page stores each cell's header and slot as fixed-size stores, and
  an index key copies text whole: creating two indexes fell from 11.2 ms to
  10.3 engine-only.
- A commit writes its three allocation bitmap pages and its superblock
  straight into their page arrays, rather than through payload vectors copied
  into pages: 1,000 inserts each committed alone fell from 27 ms to 25
  engine-only. A transaction's lookup by key hands the key it encoded to the
  committed tree rather than encoding it again.
- Every hash map and set shares one multiplicative hasher: SipHash's keys come
  from memory addresses on `wasm32-unknown-unknown`, which has no random
  source, so it resisted nothing. With four sets that needed no hashing moved
  to vectors, the engine shrank by 3.1 KiB compressed, more than this
  session's speedups added.
- A statement's changed keys are sorted and deduplicated by their encoded keys,
  rather than inserted into B-tree maps keyed by their JSON text, and a
  commit's come straight from its staged entries, already in that order, and
  are decoded only for a table within the bound. Inserting 10,000 rows 200 at
  a time fell from 33.2 ms to 30.5 engine-only, and the engine shrank by
  2.1 KiB compressed. The Worker copies a short ASCII parameter into its
  request rather than calling `TextEncoder`, which costs a Chromium Worker
  0.26 µs a call: 10,000 inserts in a transaction fell by 5% engine-only in
  Chromium. Changed keys are then kept as each table's key columns, in key
  order, and a row of values for each key, rather than as a map for each key
  in a map of tables: writes by primary key fell 3% more, and the engine
  shrank a little.
- A statement's result that published nothing, a read or a statement inside a
  transaction, passes through the Worker as the text WASM wrote: its header
  and its rows, which only the page parses and checks, as protocol version 10.
  Point statements took 4-7% less time in the browser benchmark, with OPFS.
- The catalog keeps its tables and indexes in a `NameMap`: names and values in
  two sorted vectors, whose search one function serves for both, rather than
  in two B-tree maps with their own code each. The engine shrank by 2.5 KiB
  compressed.
- A single-row upsert no longer formats its row's canonical conflict key, the
  text that finds another row of the same statement with the same key, which
  a lone row can never meet: 1,000 upserts fell from 10.5 ms to 10.0
  engine-only. A scalar parameter whose JSON text cannot pass the bound is no
  longer measured exactly, which formatted a float and scanned a string: 1,000
  updates by key fell a further 3%.
- The Worker writes a request's parameters by reading them directly, rather
  than through each property's descriptor so that no getter could run: every
  request reaches it as a structured clone the protocol check accepted, which
  holds no accessors. Engine-only in a Chromium Worker, 10,000 inserts one at
  a time or 200 at a time fell by a tenth.
- A single-row upsert without `RETURNING` that meets a stored row by its
  primary key rewrites the row's record, as an `UPDATE` keeping its key does,
  rather than decoding the row into a map, applying `DO UPDATE SET`, and
  encoding it again. A row whose text could pass the row limit, a table keeping
  a JSON column the statement does not assign, and an assignment to a key
  column still plan maps. 1,000 upserts, half of them updates, fell from 9.0
  ms to 8.3 engine-only, for 1.3 KiB compressed, and a test checks that both
  plannings agree statement by statement.
- Each superblock carries the allocation bitmap of the first 31,488 pages,
  123 MB, in the rest of its page, and the rest of the bitmap is two chunks,
  each with two slots of its own, rather than three pages that every commit
  rewrote beside the superblock. A commit writes only the chunks it changes,
  in the slot beside the one its predecessor reads, and the superblock
  records each chunk's slot, generation and page checksum, so that a chunk an
  abandoned commit left in that slot at the same generation is refused. A
  commit to a database under 123 MB writes its data pages and its superblock
  and nothing else: 1,000 inserts each committed alone fell from 212 million
  instructions to 199 engine-only, and took about 5% less time in Chromium
  with OPFS, where each write call costs about 17 µs and the flush most of
  the rest. This is part of page format 3, so it breaks nothing released.
- Opening a database counts the rows each index should hold in the pass that
  validates the table's rows, rather than scanning the table again for every
  index that is not unique over required columns, and an index that claims
  entries without a tree root is refused, which the count alone had let
  through. Reopening a 10,000-row table with one index fell from 344 million
  instructions to 333 engine-only.
- A transaction finds a free page by testing 64 pages' bits at a time,
  rather than looking each page up in both bitmaps in turn from the start of
  the file, which every write transaction's first allocation did. The
  benchmark tables are too small to show it, but 1,000 inserts each committed
  alone into a 100,000-row table fell from 626 million instructions to 472
  engine-only, and from 76 ms to 54.
- A commit writes the pages its transaction allocated from the pager's list
  of them, in page order, and drops the committed pages it freed from the
  page cache by comparing the two bitmaps, rather than visiting every cached
  page three times: to write the candidate's, to check none was left dirty,
  and to look each committed one up in the new bitmap. A cache full of a
  large table's pages had made every commit slower: those 1,000 inserts into
  100,000 rows fell again, from 472 million instructions to 268, and from
  44 ms to 24.
- The owning tab's statement, when nothing waits ahead of it, is served as
  its Worker receives it, and its response posted straight to the page,
  rather than first joining the Worker's pending requests and reaching the
  owner as a routed message. Point statements took 2-3% less time in the
  browser benchmark with OPFS, for 91 bytes compressed.
- A lookup by primary key encodes its key straight from the values the
  predicate compares the key columns with, borrowed from the statement,
  rather than copying every equality the predicate holds into one map, the
  key's columns into another, and encoding that. Engine-only, reads by key
  took 8% less time and updates and deletes by key 7% less, for 121 bytes
  compressed.
- A record is encoded into one buffer, sized for its values and the largest
  header they could need, with the header written in front of the values
  once they are in, rather than into a growing buffer copied behind a header
  into another, from separate lists of the columns, their values, their
  ends, and which are null. A written row allocates five fewer blocks:
  engine-only, inserts took 7-9% fewer instructions, and updates by key and
  upserts 4% fewer, and the engine shrank a little.

Found along the way:

- Copying a slice of a length known only at run time compiles to `memcpy`,
  which WebAssembly runs as a `memory.copy` call into the runtime. Reading a
  record's offsets and integers that way cost scans about a quarter of their
  time.
- Collecting into a `BTreeMap`, and sorting with a closure, compiles a
  separate copy of the sort for every call site. Replacing nine such sites
  shrank the engine by 13 KiB compressed.
- Each `BTreeMap` key and value type compiles its own copy of the map's code,
  1-3 KB compressed, and removing entries compiles its rebalancing too, about
  5 KB more before compression. The prepared statement registry, the catalog's
  records while loading, a transaction's per-table counts and unique-value
  claims, and a write's new pages each had a map type of their own; replacing
  them with vectors, bitmaps, or maps the engine already had cut 15 KiB
  compressed, with no statement slower.
- Most of a page is zero bytes: a B-tree node's free space, the allocation
  bitmap's unused pages, the superblock's padding. Over zero bytes a CRC-32
  depends only on its state, so precomputed tables pass 256 of them in four
  lookups; committing one insert had spent a third of its time checksumming.
- Even sixteen bytes at a time, a CRC-32 looks up every byte in a table.
  Pages other than superblocks now carry XXH64, over the page's 32-byte
  stripes that are not all zero and a bitmap of which stripes those are, so
  zero bytes stay nearly free. Superblocks keep CRC-32: v0.1.0 through v0.3.0
  check a page's CRC-32 before its version, so a CRC-32 superblock is what
  lets them refuse a newer database as unsupported rather than corrupt.
- A constant table costs its full size compressed, since its bytes look
  random. The CRC-32 byte table had stayed in the engine's data section after
  its slicing tables moved out; building it on first use saved 1 KiB
  compressed.
- Allocating a page past the end of the file wrote a zero placeholder for it,
  so commits that grew the file wrote every new page twice.
- Each OPFS write call costs about 17 µs whatever its size, so a commit's
  page writes are worth batching even when the pages are few.
- The write-set budgets use two different row-size estimators, which share a
  name in different modules. The transaction's running totals are tested for
  exact equality with a whole-write-set oracle, which catches any mix-up.
- Chromium compiles WebAssembly lazily: loading the engine takes about a
  millisecond, and each function is compiled when first called, then
  optimized once hot. Every benchmark sample starts in a new browser profile,
  so a timed phase paid for compiling the code it reached: `DELETE ... LIKE`
  took 14 ms cold and 7 ms warm. Workers running the same module share
  compiled code, so a short-lived second Worker now warms the engine up.
- Formatting an `f64` with Rust's `Display`, even in one error message, links
  the Grisu and Dragon4 algorithms, about 10 KB compressed, and serde's derived
  deserializers link them too, through their error messages. Reading the
  catalog's models by hand, and writing that one number with zmij, took the
  engine from 316 KiB to 307.
- `Reflect.ownKeys` serves no cached key list in V8, and took 0.15 µs for a
  six-key object where `Object.keys` takes 0.01. Four checks of every
  statement's request and result used it: about 1 µs a statement.
- A recycling allocator in front of dlmalloc made point statements 5-7%
  faster while staging allocated kilobyte B-tree nodes for each row, but once
  it no longer did, the layer cost bulk reads and deletes 4-6% for 1-3% on
  point statements, and was removed. Removing the allocations served better
  than making them cheaper.
- Messages between the page and the Worker cost about the same whether a
  result crosses as an object or as JSON text the page parses. What JSON text
  saves is the Worker's own parse and check of the header, about a microsecond
  a statement, so a result that published nothing now crosses as text, and
  one that committed, which the Worker reads to announce its changes, as an
  object.
- `wasm-opt` inlines a function into its only caller even when it loops. The
  plain scan loop, folded into the transaction reader's larger function, ran
  5% slower; called from two places, it stays whole.
- V8 optimizes a WebAssembly function once it has run about 13 million bytes
  of its code, so a function called once per row is optimized only after tens
  of thousands of rows. A single statement over 10,000 rows therefore runs
  much of its per-row code unoptimized the first time: engine-only in Node, the
  8,000-row range delete takes 10 ms the first time and 5.6 once warm, and 7.3
  after the warm-up Worker's statements, which touch its paths too lightly to
  optimize them.

- A filter's simple terms, comparisons of an INTEGER or TEXT column with
  constants, `LIKE` patterns, and `IS NULL` tests, compile into record tests,
  which a table scan applies to each leaf's cells in one pass over the leaf's
  bytes, with the record's offsets, null bits and short integers read in
  place and literal `LIKE` segments matched as bytes, and only the accepted
  rows are decoded and presented. The scan charges the caller's row bound a
  leaf at a time. Engine-only, 100 range aggregates fell from 46 ms to 20, 100
  `LIKE` aggregates from 111 to 71, and 100 range `UPDATE`s from 58 to 31.
- A prepared statement of one of four shapes, a lone INSERT, an upsert on the
  primary key, an UPDATE or a DELETE of the row a primary-key equality names,
  or an INSERT of listed rows, keeps a point template, and inside a transaction
  is planned from the template and its parameters directly, without binding
  the syntax tree or the general planner's validation and scanning; a test
  stages the same statements both ways and compares the transactions.
  Engine-only, inserts in a transaction took a fifth less time, updates by key
  a fifth less, upserts a third less, and 200-row inserts a quarter less.
- The WebAssembly build provides `memcmp` and `bcmp` that compare eight bytes
  at a time; the builtins' byte loops had been 19% of a 200-row insert's time
  under baseline compilation.
- The client shapes results and requests without regular expressions or
  spreads, the Worker sizes scalar parameters by shape and writes them in one
  pass, and the validators count keys rather than walking them: warm in
  Chromium, an update by key spent 1.7 µs less in the Worker and 1.1 µs less
  on the page.
- The rows a statement changes share one `Rc<str>` for their table, hoisted
  once per planner, and the entries an index gains and loses for them are
  encoded into one buffer; a result's changed keys hold their table's schema
  rather than copies of its name and key columns; the write-set and DML size
  checks are inline additions with cold error paths; a record layout keeps
  its rows' JSON overhead; the write-set preflight resolves a statement's
  table once and the script path charges its operations once; and the Worker
  writes each response into the text kept from the last. An update by key
  makes 1,513 WebAssembly calls rather than 1,876. Engine-only, the 8,000-row
  range delete takes 12% less time optimized and 18% less under baseline
  compilation, the `LIKE` delete 6% less, and 10,000 inserts in a transaction
  6% less under baseline compilation, for 3.8 KiB more compressed code.
- A point template keeps each listed value as a literal or a parameter
  position, resolved when the statement is prepared rather than looked up in
  the value's marker object for every row of every execution, and an insert of
  a key planning read no row for, into a table none of whose staged entries is
  a delete, skips the search of the staged entries for one it would replace:
  the overlay's B-tree map searches had been a quarter of a 200-row insert's
  time under baseline compilation. Engine-only, 200-row inserts take 14% less
  time under baseline compilation and 9% less optimized, 10,000 inserts in a
  transaction 8% and 3%, and upserts 4%.
- A script's DELETE and UPDATE scan through the record tests as reads and
  transactions do: the script candidate had inherited the trait's default
  visit, which decoded every row and judged it with the whole predicate. A
  predicate term the record tests reject no longer has the row's other terms
  evaluated, so an expression that would fail on a rejected row (a division
  by zero, say) no longer fails the statement, as was already the case for
  reads and for statements inside a transaction; a test pins the three paths.
  Engine-only, the `LIKE` delete takes 21% less time under baseline
  compilation and 11% less optimized, the range delete 7% and 5%.
- A script's DELETE over a table with no index of any kind and no foreign key
  involving it keeps `HeldRow::Measured`, the row's estimated bytes, instead
  of a copy of its entry: the writer needs only that the row exists and what
  it is charged as. The per-row change charge and a scalar key's estimate are
  hoisted to the statement, and the write-set preflight takes a scalar key's
  estimate from the layout instead of opening an empty record. Per deleted
  row of the `LIKE` delete, 33 fewer WebAssembly calls and one allocation
  fewer; engine-only, 6% less time under baseline compilation and 15% less
  optimized, the range delete 2% and 4%, for 0.4 KiB more compressed code.
  The one observable change: a corrupt text or JSON column in such a row
  fails the statement while planning rather than while applying.
- A transaction's staged rows live in a `TableOverlay`: one vector of
  entries per table in arrival order, a `KeyIndex` of open-addressed u32
  slots from a multiplicative hash of the encoded key, the table's count of
  staged deletes, and a key order built lazily: positions that arrive above
  the last stay in order, and a scan or the commit sorts the tail that did
  not and merges it in by binary search, copying positions rather than
  cloning keys. The B-tree map's searches had been a quarter of a bulk
  insert's time under baseline compilation. A first version re-sorted the
  whole order with cloned keys at every scan, which slowed update-scan by 16%
  under baseline compilation; the merge brought it 5% below where it was.
  Engine-only, 10,000 inserts in a transaction take 11% less time under
  baseline compilation and 10% less optimized, 200-row inserts 16% and 15%,
  updates, upserts and deletes by key 3-5%, and the engine is 2.2 KiB smaller
  compressed, the B-tree map's instantiation gone and the index build and
  GROUP BY sharing one sort of borrowed keys.
- A statement and its result cross between the page and the Worker as flat
  arrays, as protocol version 12 and bridge version 6. The request is
  `[version, id, operation, target, transaction, arrayRows, ...parameters]`.
  The response to a statement that published nothing, a read or a statement
  inside a transaction, is `[version, id, command, revision, rowCount]`
  followed by a read's rows as JSON text, or by the one table a write changed
  with its key columns and each changed key's values. The engine writes that
  result as numbers into a buffer it owns, which the Worker reads in place
  through the module's memory, where it wrote a JSON header that the Worker
  decoded and the page parsed; a result of any other shape (`RETURNING` rows,
  several tables, a commit) still crosses as a `SqlResult` in a response
  object. The page counts a transaction's statements in flight through a hook
  its connection calls, rather than keeping each promise in a set, and a
  prepared statement's in a second, with a reaction on each to remove it; the
  coordinator and the host serve a flat statement at once when nothing is
  queued, and build the request object it stands for only when it must wait.
  Whole stack in Node with real structured clones, cold as a benchmark sample
  is, medians of nine interleaved pairs: update-pk -35%, upsert -37%,
  delete-pk -36%, insert-transaction -33%, insert-indexed -32%, select-pk
  -27%. In Chromium, twelve samples of each build interleaved, with SQLite in
  the same runs (its medians in brackets): update-pk 30.45 to 24.45 ms
  (24.0), upsert 29.50 to 22.85 (22.4), delete-pk 29.95 to 23.80 (21.5),
  insert-transaction 247.2 to 199.9 (199.1), insert-indexed 259.1 to 206.7
  (212.1), select-pk 27.65 to 22.05 (29.7), select-indexed 3.65 to 2.90
  (3.50), and insert-autocommit, whose statements commit and so cross as
  before, 364.9 to 359.9. The package is 3.4 KiB larger compressed: 1.2 in
  the client, 1.3 in the Worker and 0.8 in the engine.
- A statement of one change is staged without the bookkeeping a statement of
  many needs. Its patch is one vector of one struct per table, built in two
  vectors the transaction keeps between statements, the entries vector only
  while it is no larger than a first push makes one, so that a statement of
  thousands of rows leaves nothing behind; the claims a patch gives up and
  makes are sets made only when there is one to hold; and the place
  `NameMap::position` found for the table's overlay and the probe
  `KeyIndex::find` made for the key are carried to the install rather than
  repeated. A test stages the same changes both ways and compares the
  transactions, and 79,000 generated statements gave the same results and
  pages before and after. Instructions under baseline compilation: update-pk
  -3.9%, upsert -4.6%, delete-pk -4.4%, insert-transaction -5.2%,
  insert-indexed -4.7%, with staging's calls a third fewer (an update's 299
  to 205, its allocations there 5 to none); whole stack, cold, -1.6% to
  -3.7% by median; 33 bytes smaller compressed.
- A cached page is fetched with no call but the cache's lookup.
  `Pager::read_page_in_place` tests in one place what a read of a cached page
  needs (the pager is usable, the ID is a data page the active root
  allocates, no page is reserved, the cache holds it), marks the entry
  referenced and reads the envelope's type, length and ID at fixed offsets;
  any other read goes whole to `read_page_checked`, which makes each check in
  turn and reports the first that fails. The route had been 17 calls a page,
  three pages for a lookup in the benchmark's table. Ten tests hold the new
  route to the old one: every page ID against the old bitmap indexing, every
  type byte, and `decode_whole` against `decode_verified` under every
  single-bit flip of the envelope. Instructions under baseline compilation:
  update-pk -2.0%, upsert -1.4%, delete-pk -1.9%, select-pk -2.3%,
  select-scan -3.4%, update-scan -2.1%, and about half as much once
  optimized; 15 bytes larger compressed.
- The row a prepared UPDATE, DELETE or upsert by key changes is copied once,
  found by the key the statement encoded. The statement read it through a
  visit: the view encoded the key from its values, the lookup copied the
  record out of its leaf, the visitor copied key and record again into the
  entry the writer keeps, and the statement copied the key a third time for
  its change. It now encodes the key once and asks the view for the held
  row by it (`StorageReader::held_encoded_key`, which only a transaction's
  view answers): a staged row is copied from the overlay, and a committed
  one by `Btree::get_entry`, which writes the key and then the value into
  one allocation of exactly their length, the entry itself. The read makes
  the tests the visit made in their order (the transaction's revision, one
  work charge, the table, the overlay, the record's header), and a test
  keeps the old route as its oracle over generated rows, in leaves and in
  overflow pages, staged and committed. Instructions under baseline
  compilation, medians of nine interleaved pairs: upsert -2.9%, update-pk
  -1.4%, delete-pk -0.9%, select-pk -0.5%, with 38, 62 and 23 fewer calls a
  statement for an update, an upsert and a delete; 17 bytes larger
  compressed.
- The batch writer makes no call for a cell's fields. A leaf cell's three
  header fields and its slot are stored by `put_u16` and `put_u32`, indexed
  byte stores compiled into their callers, last byte and last field first so
  that one bounds test covers the header, where `put` took a slice and made
  an array of it, two calls a field. `new_value` keeps its two length tests
  and is compiled into the three places that make a cell, with the storing
  of a large value cold and out of line; a replaced entry hashes its key
  once for the fingerprint of the value it held and of the one it takes;
  `write_leaf_cell` copies an inline value only when it has bytes, which no
  index entry's has; and `validate_key` is compiled into its callers with
  its refusal built out of line. Page bytes, fingerprints, errors and the
  order of allocation are unchanged: thirteen workloads made the same device
  writes byte for byte before and after, and a test pins entry fingerprints
  to values computed outside the engine. Calls in the commit of 10,000
  inserts: 550,151 to 459,315, and with an index 1,778,495 to 1,576,920.
  Instructions under baseline compilation: insert-indexed -1.4%,
  insert-transaction -0.8%, update-pk -0.8%, upsert -0.7%, delete-pk -0.5%;
  commit time there 3.87 to 3.08 ms for the inserts and 11.29 to 10.25 with
  an index. Giving the cell vector of a tree with no root its capacity at
  once showed nothing when reverted alone, and was left out. The compressed
  engine is 168 bytes larger on this base, with 31 fewer bytes before
  compression.
- A write's batch for an index, and the catalog's, are ordered through
  `sort_keyed`, as (key, position) pairs, and built from that order, as an
  index build's batch already was; `BatchChange::sort`, a second
  instantiation of the standard sort, is gone, with 14 functions of the
  dump. Both sorts order distinct keys by the same comparison of bytes, and
  `Btree::apply` refuses a batch that is not strictly increasing before it
  reads or writes anything, a guard that had no test and now has one. In the
  same edit `apply_row_changes` keeps an index's entries without closures:
  a row's old and new entries are compared only when both are there, and
  each one present is pushed, ten calls fewer for a row inserted or deleted.
  Nineteen workloads make the same device writes byte for byte. The package
  is 1.42 KiB smaller compressed; instructions under baseline compilation:
  delete-range -2.3%, insert-indexed -0.2%, create-index +0.3% (its batch's
  push is now a function shared with these two batches).

Found along the way, 6 and 7 October:

- Warm in Chromium (measured with a page-and-Worker profiler harness), an
  update by key in a transaction costs TinyJoin about 22 µs against SQLite's
  17.5, of which about 12 µs in each is the message exchange itself: one
  `postMessage` each way, their serialization and dispatch, and 4-6 µs in
  which neither thread runs. TinyJoin's WebAssembly takes 5.3 µs against
  SQLite's 1.4, and its page-side JavaScript 3.5 against 1.2; SQLite pays
  1.6 µs per statement writing its rollback journal, which TinyJoin defers to
  the commit. Only sending fewer messages per statement can remove the floor:
  a shared-memory channel measured 13.6 → 8.7 µs per round trip, but needs
  cross-origin isolation.
- The benchmark's 1,000-statement phases run mostly baseline-compiled
  WebAssembly: V8 optimizes a function only once about 13 MB of its code has
  executed, so TinyJoin's many small functions stay in Liftoff for the whole
  phase, where they run 1.7-1.9 times slower than optimized, while SQLite's
  few large functions cross the budget almost at once. Running thousands of
  point statements in the warm-up Worker first did not change the browser
  numbers.
- Binaryen's `--log-execution` pass, with the logging import answered from the
  harness, counts every function's calls: an update by key made 1,876 calls,
  122 of them to a two-byte reader, 102 to `memcmp`, 57 to `Vec::reserve`,
  39 to `push_str`, 56 to `Option::ok_or_else`, and 28 `malloc`/`free`
  pairs. Removing 19% of the calls took 5% off the time under baseline
  compilation, so the calls themselves are a smaller part of the tax than
  their count suggests. Compiling the two- and four-byte readers into their
  callers cost 2.6 KiB compressed for about 1%, and was dropped.
- Compiling the engine at opt-level `s` takes 23% off the baseline-compiled
  time of statements by key and 8% off the optimized time, for 39 KiB more
  compressed code; an explicit LLVM inline threshold of 30 at opt-level `z`
  about 10% for 17 KiB, and a threshold of 15 nothing reliable for 3.8 KiB.
  `wasm-opt -Os` makes no difference to `-Oz`. The first measurement after a
  build runs 20-40% slow on this passively cooled machine, and once mistook the
  threshold of 15 for a fifth off: compare builds interleaved, after a
  warm-up block.
- A commit's page writes: the 1,000 spread updates, the 200-row inserts and
  the `LIKE` delete each rewrite all 178 of the table's pages in two or three
  runs, since the rows they change lie in every leaf; the range delete frees
  whole leaves and writes 10 pages; committing one insert writes about four
  pages and flushes, and an OPFS flush costs 0.35-0.4 ms, which is nearly all
  of that workload's 431 µs per statement.
- Carrying a held row's estimated bytes inside its `StoredEntry` (so the
  script writer stops re-opening the record to charge it), with an existence
  probe that copies no row, was built, reviewed and measured, then dropped:
  the range delete gained 3-5% under baseline compilation and nothing
  optimized, updates and upserts were flat to 3% slower, and the engine grew
  0.8 KiB compressed, since the entry grew by four bytes on wasm32 and the
  shared leaf descent added an indirect call to every point lookup. The
  patch is kept in the session scratchpad as held-estimate-dropped.patch.
- The slow single-insert commit samples, examined over eight publication runs
  and two isolated ones by three readers and their skeptics: TinyJoin's file
  does not grow at each commit (the freed pages are reused, so 20 of 1,000
  commits append a page), Chromium's sync access handle adds no per-write
  work, and the disturbance is not TinyJoin's alone. It is a bounded stretch
  of slower flushes that follows a large teardown, mostly a PGlite profile of
  more than a thousand files and a renderer near a gigabyte, and lands on
  whichever sample's flush-dense window it overlaps: TinyJoin's 0.4 s window
  of a thousand flushes fits inside it and reads as two to three times slower,
  SQLite's and PGlite's longer windows as a few hundred milliseconds more,
  and PGlite following PGlite was hit the same way in an isolated run. It
  needs a busy machine: the two worst runs had Docker's VM and a 12 GB model
  server resident, and the run discarded today had the media and photo
  analysis daemons started again by launchd during it, and a Time Machine helper,
  where the kept run half an hour later had them paused. The pre-sample flush
  probe does move before slow samples (0.30-0.54 ms against 0.25-0.31) but
  within its tolerance. Taken: the runner's profiles and probe files now live
  in a `.noindex` directory, and the quiet-run script pauses again any daemon
  launchd starts during a run. Left for a later run: a per-statement timeline inside the
  workload, so a slow sample shows whether it was uniformly slow or stalled,
  and a gate that reads the system's idle time rather than one thread's speed.
- The slow `insert-autocommit` samples in the two runs discarded on 7
  October were the ones that followed the deletion of a PGlite profile. Within
  a round the engines ran in a fixed rotation of TinyJoin, SQLite, PGlite, so
  TinyJoin followed PGlite in six rounds of nine, and in both runs those
  twelve samples took 909-1,150 ms, bar one in each run at about 500, against
  372-414 ms for the four that followed SQLite across a round boundary within
  the workload; the first sample of each run, which followed the previous
  workload's last SQLite sample, was slow too (798 and 1,026 ms), so the
  deletion explains the pattern within the workload but not the workload's
  first sample. SQLite's two slow samples in each run (2,705 and 1,824 ms,
  then 2,360 and 1,698) were among the three that followed PGlite across a
  boundary, and PGlite, which never followed itself, kept its medians (464
  and 459 ms), though it had slow samples of its own. A two-engine run fitted
  the same rule in eleven samples of twelve. PGlite's `opfs-ahp` creates a
  pool of 1,000 OPFS files when it initializes, so removing its profile is by
  far the heaviest deletion in the suite, and the next sample's flushes pay
  for it for a few seconds. Those two runs were not quiet otherwise either:
  their CPU probe reached 96 and 85.6 ms against a baseline of 18, where the
  kept runs reached 22-26. The runner now waits on a flush probe before each
  sample and uses every arrangement of the engines across rounds, as the
  benchmarks guide describes. The flush it times is the one the engines pay
  for, read in Chromium's source on 7 October (its main branch, not the
  harness's version; the change dates from 2019, https://crrev.com/c/1400159,
  so Chromium 153 has it):
  `FileSystemSyncAccessHandle::flush()` calls `file_delegate()->Flush()`,
  `FileSystemAccessRegularFileDelegate::Flush()` is `backing_file_.Flush()`,
  and `base::File::Flush()` is `fcntl(F_BARRIERFSYNC)` with `fsync` as the
  fallback on Apple platforms, and `fdatasync` on Linux. Node's `fs.fsync`
  cannot be asked for the barrier: `uv__fs_fsync` in `src/unix/fs.c` of
  libuv 1.52.1, the libuv Node 24.21.0 bundles, tries `F_FULLFSYNC`, then the
  barrier, then `fsync`; the macOS SDK's `sys/fcntl.h` defines `F_FULLFSYNC`
  as 51 and `F_BARRIERFSYNC` as 85, and python's `fcntl` module names only the
  first, so the probe's helper passes 85. Timed here with a 16 KiB write, a
  barrier takes about 0.3-0.6 ms, a full flush and Node's `fsync` alike about
  3 ms, and a plain `fsync` 0.03 ms. PGlite's `opfs-ahp` flushes by calling
  flush() on each of its access handles, read in its `dist/fs/opfs-ahp.js`;
  SQLite's `opfs-sahpool` xSync was not read. Creating 1,000 files of 40 KiB
  on the same volume and deleting them, whether unflushed within a tenth of a
  second or each flushed with the barrier and deleted three seconds later, did
  not move the barrier probe from its 0.35-0.5 ms, so file churn alone does
  not reproduce what a PGlite profile's deletion did to the sample after it;
  the readings the runner keeps beside each sample will show whether the
  probe sees it.

Found along the way, 8 October:

- Half of what a point statement cost beside SQLite was JavaScript and the
  shape of its messages, not the engine. Every benchmark sample is a fresh
  page, so its 1,000 timed statements run the page's and the Worker's
  JavaScript in V8's interpreter and baseline tiers, which inline nothing:
  each small helper (`isRecord`, `isUndefined`, `objHasOwn`) is a call, and
  the client's 0.9 µs a statement once optimized was 3.7 µs there. A
  structured clone costs by the shape of what it copies. Timed in Node, the
  request object took 1.17 µs to clone and a flat array of the same values
  0.52; the response object 0.82 and a flat array 0.58, where a compact array
  nested inside the old envelope took 1.10, more than the object it would
  have replaced. JavaScript and clones together were 7-8 µs of a statement,
  nearly as much as the engine.
- V8 spends a WebAssembly function's tier-up budget in proportion to the size
  of its baseline code, so the engine's largest functions are optimized
  before a benchmark's timed phase begins, by the warm-up Worker's statements
  and the workload's own setup, while its small ones stay in baseline code
  throughout. A change that removes work from a statement keeps its value in
  the browser; one that only folds small functions into a large one mostly
  does not. Each engine step is therefore measured twice: as instructions
  under baseline compilation alone, and as whole-stack time under V8's own
  tiering.
- Instructions retired (`/usr/bin/time -l`), for a process that runs a
  workload a fixed number of times less a process that only sets it up,
  repeat to 0.2-0.4% on a machine whose timings of the same builds vary by
  several percent. Under baseline compilation an update by key retires about
  173,000 instructions, a call between two WebAssembly functions costs about
  48 of them, and an allocation with its free about 800.

Remaining, in order of expected value:

1. Per-statement cost of writes. As of the `8df67174` run a statement in a
   transaction costs about 22 µs, within 7% of SQLite's either way: an update
   by key 0.93 times its time, an upsert 0.99, an insert 0.97, and a delete
   by key 1.02, the one still behind. Interleaved in Chromium, the flat
   messages took 19% to 23% off each and the day's engine steps a further 4%
   to 8% off updates, upserts and deletes by key and 2% off inserts, so work
   in the engine still shows in the browser. The steps designed and reviewed
   on 7 October and not yet built are, for lookups, a node searched in one
   loop that reads its keys in place; for staging, a delete staged from its
   encoded key; for commits, a page built once in the page the cache keeps,
   and a touched leaf rewritten by runs of the cells it keeps; and for
   planning, a template's arguments and positions resolved when it is
   prepared, and a one-change plan staged and reported without vectors.
   What follows is the history of this item. Measured warm in Chromium, a
   prepared point
   read costs 26 µs against SQLite's 24, but an insert in a transaction costs
   about 28 µs against 16, and an update about 34 against 17, before their
   commits. A round trip between the page and a Worker costs about 12 µs by
   itself, and the engine's share of a write is now 4–7 µs in Node's V8 and
   about half that in Chromium, spent in many small allocations and lookups
   as a statement is bound, planned, staged, and reported. Staging and the
   protocol's checks took 5-10% off each point statement in the browser on
   28 September; what remains is spread thinly: the response header's JSON,
   written in WASM and, for a result that committed, parsed in the Worker,
   and binding a statement by cloning its template. An `UPDATE` that keeps
   each row's key now writes the row's new record from its old one, a tenth
   off 1,000 updates by key engine-only, and a single-row upsert that meets a
   stored row does too. Profiled engine-only on 29 September, excluding its
   commit, such an `UPDATE` spends about 18% building its result's JSON
   header in WASM and parsing it in the Worker, 6% encoding its parameters,
   and a third planning, where the key lookup costs most: a key map built
   from the predicate and then encoded (4%), and the B-tree descent, which
   reads each probed cell's key (10%). The rest is the Worker's JavaScript, a
   few microseconds more than SQLite's: the layers each request passes
   through, and copying requests and results between threads as object
   graphs. Parameters now cross into WASM as bytes the Worker writes while it
   checks them, rather than through `serde_wasm_bindgen`, which cost about
   0.7 µs a statement.
2. Scans take 1.05 times as long as SQLite's in the `73042cc` run, and a
   transaction's range `UPDATE`s 1.3 times, down from 1.5 and 2.0 before the
   `b3ebbaf` run. What
   remains is spent mostly in the cursor, the per-row visitor call, and
   reading the column a predicate tests. Forcing a small fast path inline
   where every scanned row passes (`#[inline(always)]` on the fast path, not
   on the general function) paid best: inlining a whole general function grew
   the engine by a kilobyte or more for less.
3. Bulk deletes take 1.3 times as long as PGlite's in the `73042cc` run, and
   less than SQLite's. PGlite, like PostgreSQL, marks deleted rows and reclaims them
   later; TinyJoin removes each row and its index entries at once. Deleting a
   contiguous key or index range could drop whole subtrees, using their
   fingerprints and counts, rather than visiting every entry.
4. [D7](#decisions-needed), remeasured under
   [build settings](#build-settings): keep `z`.
5. O2 and O4. Reopening is within 1.1× of SQLite, but checking an index on
   reopen looks up its table row for every entry: engine-only, a 10,000-row
   table's index took 26 ms to check, where its rows take 15. After the scan
   and lookup work of 29 September, an index on its text column takes 14.
   Checking entries without those lookups would take memory in proportion to
   the index, or a weaker check than [D2](#decisions-needed) keeps, and on
   29 September was judged not worth either while reopening stays close to
   SQLite's.
6. Size. Each B-tree map type still compiles its own code: a transaction's
   claims could live in vectors, as the catalog's tables and indexes, and the
   keys a write reports, now do. The hash sets now share one hasher, but each key
   type still compiles its own table code.
7. Cold code. A single statement over thousands of rows runs much of its
   per-row code unoptimized the first time (see above), and every benchmark
   sample is a first time. Warming those paths would take a warm-up of tens of
   thousands of row operations, a CPU cost at every page load that is a
   product decision. The engine is compiled from a streamed fetch, whose
   optimized code Chromium can cache for later visits, which fresh-profile
   samples never make; whether it does for this module is unmeasured.

## Where the time goes

The engine, not the Worker, dominates. A scan of 10,000 rows costs about
65 ms natively with the shipped optimization level, which is the same as the
browser's 69 ms per query. Each scanned row costs about 65,000 instructions
and 48 heap allocations, where SQLite spends about 60 ns per row.
Predicate evaluation and aggregation together take about 1% of scan time;
nearly everything else is storage-layer overhead.

| Area | Gap to SQLite | Root cause |
| --- | --- | --- |
| Scans: range, LIKE, GROUP BY, count | 25–105× | Every read re-validates every row (55–66%), and every page access re-checks its CRC and re-decodes the node (about 30%) |
| Indexed ranges | 1,400× | The planner uses an index only for equality, so ranges scan the table |
| Joins | 175× | The inner table is rescanned and cloned on every query, every pair is compared, and `WHERE` is not pushed down |
| Point reads | 4× | Page CRC per access (about 33%), a whole-cache hash rebuild after every statement (about 25%), and object building |
| Reading all rows | 2.3× | Row validation, four to six size walks per row, per-cell key conversion, and TypeScript re-validation of the result |
| Updates, deletes, upserts in a transaction | 1,000–1,400× | Each statement re-validates the entire write set, so a transaction is O(n²) |
| Bulk inserts, index builds, bulk deletes | 10–250× | Commit applies each row separately, re-encoding and re-checksumming its whole root-to-leaf path |
| Reopen | 3.5×, plus about 1 s per non-unique index at 10,000 rows | Open re-validates every row, and checks every index entry with a random table lookup |
| Row storage | 10,000 small rows take 573 pages (2.3 MB) | Rows are JSON text carrying every column name, with numbers as text and the primary key stored twice, in leaves that are only half full |
| All of the above | 1.3–1.7× | `opt-level = "z"` slows the hot byte loops |

## Ground rules

- Remove wasted work before changing contracts. Anything that changes
  documented behavior is listed under [decisions](#decisions-needed) and waits
  for one. The storage format is the exception: it changes once, in one
  release, as [page format 3](#page-format-3).
- Keep every replaced validator as a test oracle. New code must agree with
  it on randomized inputs before the old path leaves the hot path.
- Gate every step on `npm run test:rust`, which includes the recovery and
  semantic property suites, `npm run test:browser`, and `npm run check:size`.
- Measure every step twice: first with the native harness below, for quick
  and deterministic instruction counts, then with a subset of the browser
  suite, for example
  `npm run bench:compare -- --engines tinyjoin,sqlite --workloads select-scan,select-pk`.
  Publish the full suite at milestones.

## Phase 0: a faster measuring loop

- Add a small native benchmark crate, for example `benchmarks/engine`, that
  links `tinyjoin-core` by path. It should report instructions retired and
  allocations for each workload. The browser suite takes half an hour, and its
  wall times are noisy.
- Add the workloads the current suite misses:
  - `ORDER BY id LIMIT 10`;
  - `count(*)`;
  - reopening a database with a non-unique index;
  - update transactions at several sizes, to expose growth.

## Reads

### R1. Check each page once, when it is loaded

**Risk: low.** Today every page access copies 4 KiB, recomputes a byte-wise
CRC-32 and copies the payload, even on a cache hit (`Page::decode`, via
`pager::decode_expected_page`).

- Verify the checksum and envelope when a page enters the cache from the
  device, and mark the cache entry as verified.
- Trust pages the engine encoded itself, since encoding computes their CRC.
- Remove the page copies.
- Keep decoded parent nodes on the cursor path instead of re-decoding them on
  every leaf change.
- Stop building a `HashSet` of children on every internal-node decode.

On-disk corruption is still detected on every device read. Scans gain about
1.5×. Commits, index builds, deletes and reopen gain 1.7–2.5×, because they
walk the same path.

### R2. Run read-only statements without a write candidate

**Risk: low.** Every standalone statement opens a pager write candidate, and
aborting it calls `PageCache::rebuild_lookup`, which re-inserts every cached
page into a SipHash map. That is 93% of a `LIMIT 0` statement, and its cost
grows with the cache, up to 4,096 pages.

- Run autocommit reads against the committed view.
- Maintain the cache index incrementally, and skip the rebuild when nothing
  was removed. Commit does the same full rebuild.
- Try a cheaper hasher for page-keyed maps.
- Stop cloning the catalog for each read.

R1 and R2 together cut point-select engine CPU 3.2× in a prototype. They also
removed the growth with cache size.

### R3. Count bytes without formatting, once per row

**Risk: low.** Size estimates format every number with `to_string` just to
measure it, and a returned row is estimated four to six times: in
`normalize_row`, three times in `execute_ordered`, and in `retain_result`.

- Compute encoded lengths arithmetically.
- Carry each stored row's byte length.
- Estimate each row once.

The formula stays the same, so every budget boundary stays exact. A prototype
cut `SELECT *` by 1.7×.

### R4. Stop validating every row on every read

**Superseded by [page format 3](#page-format-3).** `validated_row`
parses each row's canonical JSON, re-encodes it to compare bytes, clones and
normalizes it with limit checks, and re-encodes its primary key, on every
read. Page format 3 validates each record once, when its page is loaded, so this
step only matters if the format change slips. In that case, trust committed
JSON rows on the hot path, which alone measured 2.3–2.6× on scans.

For reference, five prototype patches together (R1–R4, plus lighter cursors)
cut engine-only WASM time:

| Workload | Before | After |
| --- | ---: | ---: | ---: |
| Range aggregate scan | 88 ms | 17 ms |
| `SELECT * ORDER BY id` | 150 ms | 26 ms |
| `GROUP BY` | 95 ms | 22 ms |
| Point lookup | 0.13 ms | 0.03 ms |

### R5. Use indexes for ranges

**Risk: medium.** `visit_predicate_candidates` takes a primary-key or index
path only from `collect_guaranteed_equalities`. `<`, `<=`, `>`, `>=`,
`BETWEEN`, `IN` and `OR` all scan. `Btree::cursor_from` can already seek to a
lower bound.

- Extract bounds from `AND` conjuncts on the leading column of an integer or
  boolean index, or on an integer primary key.
  - Use ceiling and floor for float parameters.
  - Keep evaluating the full predicate, so bounds may be conservative.
- Add a bounded range visit that stops at the encoded upper bound.
- Sort candidate primary keys before fetching rows. That keeps float `SUM`
  and `AVG` bit-identical and lets one cursor serve every fetch, where today
  each index entry re-descends from the root.
- Use the same planning for `UPDATE` and `DELETE`, which take a primary-key
  equality only.

Expected: `select-indexed` improves by about 150×.

Text keys currently sort by length first. Once S2 makes them order-preserving,
extend the same bounds to text columns and to prefix patterns such as
`LIKE 'abc%'`.

### R6. Stream rows in primary-key order

**Risk: low; needs [D4](#decisions-needed).** `execute_ordered` always
collects, clones and sorts, so `ORDER BY id LIMIT 1` costs a whole-table read.

When `ORDER BY` is exactly an ascending integer primary key and the
transaction overlay holds no rows for the table, stream through the table tree
and stop at the limit. `LIMIT k` then costs O(k), and `SELECT * ORDER BY id`
skips the collect, clone and sort.

### R7. Join with indexes and hashes

**Risk: medium–high; needs [D3](#decisions-needed).** `join.rs` scans and
clones every relation after the first on every query, compares every pair
with string-keyed map lookups, and flattens rows into maps with `format!`-built
keys before applying `WHERE`. The benchmark join examines 500,000 pairs to
return about 50 rows.

1. Push conjuncts that reference only the first relation into its scan. In
   the benchmark, `c.id = $1` then becomes a primary-key lookup.
2. Use an index nested loop when the incoming relation has its primary key or
   an index on the `ON` columns. Otherwise use a hash join whose buckets keep
   build order.
3. Resolve column references once, when the plan is built.

These must be preserved:

- left-major output order;
- `LEFT JOIN` null extension;
- `NULL` never matching;
- integer and float keys comparing numerically;
- never pushing a `WHERE` term on a nullable side below its join.

Expected: about 30× on the benchmark join, and about 100× once R1–R4 land.

### R8. Smaller executor fixes

**Risk: low.** Row representation moves to [page format 3](#page-format-3)
(S3). What remains:

- use typed hash keys for `GROUP BY`;
- compile each `LIKE` pattern once per statement instead of once per row;
- pass owned rows to visitors instead of cloning them.

### R9. Lighter result transport

**Risk: medium; needs [D5](#decisions-needed).**

- The WASM bridge builds a null-prototype object per row and converts every
  cell's key with `JsValue::from_str` and `Reflect::set`.
- The Worker then walks and re-validates the whole result in TypeScript:
  16.5 ms for 10,000 rows.
- `rowMode: 'array'` saves nothing today, because arrays are made from objects
  on the main thread.

Instead:

- Return array or columnar rows from WASM, and build objects from `fields` on
  the main thread, in column order.
- Validate trusted WASM output by header only in production. Keep the deep
  check in tests.
- Validate each request once, at the untrusted boundary, instead of four
  times on its way to WASM. That saves about 10–20 µs per call.

Expected: reading 10,000 rows drops by about 25–35 ms, and array rows
genuinely become cheaper.

## Page format 3

Page format 2, used since v0.1.0, stores every row as a canonical JSON object,
after an 8-byte header holding a format version and the body length:

```json
{"a":17,"b":8493,"c":"eight thousand four hundred ninety three","g":94,"id":18}
```

In the 10,000-row benchmark table:

- about a quarter of the row bytes are repeated column names;
- numbers are stored as decimal text;
- the primary key is stored twice, once in the B-tree key and again in the
  JSON;
- leaves are only half full, so 838 KB of row text occupies 573 pages, or
  2.3 MB.

Every read parses that text, and today also re-encodes it to prove it is
canonical. In memory, every row becomes a `BTreeMap` with an owned `String`
for each column name.

The encoding must be canonical, because B-tree fingerprints hash each entry's
key and value bytes, and equal logical rows must hash equally on every
replica. Any canonical encoding meets that; it does not need to be JSON.

Page format 3 replaces the row record and the key encoding in one break, and
reserves room for sync metadata. It keeps the slotted pages and the
fingerprint scheme. The v0.4.0 release notes already announce the break. The
same break moves the start of the allocation bitmap into each superblock and
gives the rest of it slots chunk by chunk, so that a commit rewrites only what
it changed: see [progress](#progress).

### S1. Packed row records

A table entry's value holds only the non-key columns, in schema order:

| Part | Size | Content |
| --- | --- | --- |
| Flags | 1 byte | Offset width, whether a null bitmap follows, and reserved sync bits. Unknown bits are rejected. The page format versions the layout, so records carry no version byte. |
| Column count | 1 byte | The number of stored columns: at most 255, since every table has a key column |
| Sync metadata | 0 bytes until sync ships | Present only when a sync flag is set (S5) |
| Null bitmap | ⌈n / 8⌉ bytes, or none | Present only when a stored column is `NULL` |
| End offsets | n − 1 entries of 1, 2 or 4 bytes | Where each column's bytes end. The last column ends with the record, and the width is the smallest that fits. |
| Values | The rest | Packed back to back, with no names, tags or padding |

Values are encoded by column type:

| Type | Encoding |
| --- | --- |
| `INTEGER` | Minimal little-endian two's complement, 0–7 bytes; zero takes no bytes |
| `FLOAT` | The 8 IEEE-754 bytes, keeping the sign of zero as today |
| `BOOLEAN` | One byte, 0 or 1 |
| `TEXT` | The UTF-8 bytes |
| `JSON` | Canonical JSON text, as today; only `JSON` columns pay for JSON |
| `NULL` | A set bitmap bit and no bytes |

Reading column *i* takes two offset loads and a slice. There is no parsing and
no allocation, and a text value is a `&str` borrowed straight from the page.
The benchmark row shrinks from 87 bytes to 49, and its key from 11 bytes to 8.

Each value has exactly one valid encoding, so records are canonical by
construction. Checking a record means checking its bounds and minimal forms,
not re-encoding it.

### S2. Order-preserving keys

Key components drop today's type tag and length prefix, since the schema
knows each type. Every component compares correctly as raw bytes:

- integers keep today's 8-byte big-endian encoding with the sign bit flipped;
- floats keep today's sortable IEEE-754 bits, with zero normalized;
- booleans stay one byte;
- text is its UTF-8 bytes, with `0x00` escaped and a two-byte terminator. Byte
  order is then code-point order, which is the order the SQL guide documents.

Secondary-index entries stay `indexed tuple ‖ primary-key tuple`. This unblocks
what text keys prevent today: range scans and prefix `LIKE` on text (R5), and
`ORDER BY` on indexed text.

### S3. Rows in memory

- Replace `Row = Map<String, Value>` inside the engine with schema-ordered
  rows.
- Evaluate predicates, aggregates and join keys against the encoded record,
  decoding only the columns a statement references.
- Build named objects only at the API boundary, or not at all once results
  travel as arrays (R9).

This is the largest part of the change. `Row` runs through the executors, the
transaction overlay, changed keys and the WASM bridge.

### S4. Search pages in place

B-tree pages already have a slot array.

- Binary-search it on the raw page for point lookups.
- Iterate cells in place during scans, instead of decoding every cell into
  owned vectors on each visit (`Node::decode`).

Fuller splits for appends (C2) roughly halve the pages a scan reads.

### S5. Room for sync

The sync groundwork plans:

- one packed 8-byte HLC per row, stamped per transaction;
- a short exception list for columns a later transaction changed;
- tombstones, deferred until a sync protocol exists.

Page format 3 reserves a flag bit for each: a row HLC, a column exception list,
and a tombstone record with no column data. Each is absent, and free, until
sync ships.

Fingerprints stay FNV-1a over each entry's key and value bytes, which become
smaller and cheaper to hash. Whether an entry's fingerprint covers its HLC is
for the sync protocol to decide: the hasher can include or skip that section.

### Validation and migration

- **Validate each record once:** when its leaf is first loaded for its table,
  or when it is written. Cache that fact with the page (R1), and never
  validate on every read. The checks are structural and cheap. This is D1.
- **Open-time validation** (O2) runs the same record checks while merging
  index and table cursors.
- **Omit trailing columns that equal their defaults.** `ALTER TABLE ... ADD
  COLUMN` then no longer rewrites every row, and the encoding stays canonical,
  because a column's `DEFAULT` is a literal that no later statement can
  change. This is D8, decided.
- **Page format 3 does not read page format 2.** Opening an older database
  fails with `UNSUPPORTED_PAGE`, as in the v0.1.0 break, and there is no
  migration. This is D6, decided.

**Expected (estimates).**

- Even with R1–R4 applied, the scan prototype still spent about half its time
  parsing JSON into maps and dropping them, and 14% decoding nodes. S1–S4
  remove most of both. That should make scans a further 2–3× faster than the
  prototype's 17 ms.
- With C2, the benchmark table should shrink from 573 pages to about 170–200.

## Transactions with updates, deletes and upserts

Natively, 100 updates in one transaction take 0.39 s and 1,000 take 22.8 s.
One autocommit update takes 0.28 ms. After any non-append statement,
`PagedTransaction::stage` calls `validate_row_write_set` over every staged key.
That is 95% of the time, mostly `Btree::get` for base rows the overlay already
holds.

### T1. Reuse known base rows

**Risk: low.** Pass each overlay entry's base row to the validator instead of
looking it up again; the revision is pinned. This measured 2–3× faster, but
remains quadratic.

### T2. Validate incrementally

**Risk: medium.**

- Keep each staged entry's contribution to every counter, so that replacing
  an entry subtracts its old contribution and adds its new one.
- Keep a map from value to primary key for each unique index.
- For each statement, check only its own claims against the map and the
  committed owners, using today's rule for released and moved values.
- Install the new state only when the statement succeeds.
- Keep the full validator as an oracle for randomized mixed insert, update,
  delete and upsert sequences over unique indexes. The append-path tests
  already follow this pattern.

A prototype of this ceiling staged 1,000 updates natively in 0.1 s. The
benchmark's update, delete and upsert transactions should drop from 34–39 s to
under a second.

### T3. Use indexes inside transactions

**Risk: medium.** `visit_index` returns `None` inside a transaction, so range
updates and `ON CONFLICT` on a unique-index target scan the table on every
statement. Make index visits aware of the overlay.

## Bulk writes, commits and schema changes

Commit is 80–90% of an insert workload's engine time. `apply_changes` runs one
`Btree::upsert` per row per tree. Each upsert decodes, re-encodes, hashes and
checksums every node on its path.

- **C1. Apply changes in batches.** Changes arrive sorted per table.
  - Decode each affected leaf once, apply all its changes, and split once.
  - Encode and checksum each dirty page once, when the commit finishes.
  - Pass the old rows found during planning into the apply step instead of
    looking every row up again.

  Expected: 5–10× on insert commits and bulk deletes.
- **C2. Split for appends.** `choose_leaf_split` always splits in half, so
  ascending inserts leave leaves about half full. That doubles the number of
  pages every later scan reads. Split near the right edge for appends, and
  remove the split's O(n²) clones. The format does not change.
- **C3. Build indexes bottom-up.** Sort the keys, check uniqueness between
  neighbors, and write each page once. Apply the same approach to rewriting
  rows for `ADD COLUMN`. Expected: about 10× on `create-index`.
- **C4. Cut commit I/O.**
  - Cache the file length in the JavaScript page device. Today it calls
    `getSize()` on every read and write: 8 times per commit, 572 times per
    reopen.
  - Write only the bitmap chunks that changed.
  - Skip catalog records that did not change.
  - Stop writing zero-filled placeholder pages.
  - Merge the data-page flush with the bitmap flush, going from three barriers
    per commit to two, but only once the recovery property tests cover that
    ordering.
- **C5. Small fixes.**
  - Deduplicate `changed_keys` with a set instead of `Vec::contains`.
  - Stop cloning schemas and the catalog per statement.
  - Stop cloning the whole overlay for `exec` inside a transaction.

## Open and startup

`load_and_validate_catalog` scans every table with `validated_row`. For each
index, it scans the whole index, doing a random `Btree::get` into the table
per entry. Each non-unique index then adds another full table scan. At 10,000
rows, one non-unique index adds about a second to reopen.

- **O1.** R1 alone makes reopen with an index about 1.7× faster.
- **O2. Keep the same guarantee, but cheaper.**
  - Count every index's expected entries in one table pass.
  - Validate each index by merging its cursor with the table's, instead of a
    random lookup per entry.

  Expected (an estimate): roughly 10× on reopen with indexes.
- **O3. Decide the guarantee ([D2](#decisions-needed)).** Complete validation
  at open was documented in the v0.3.0 notes. Decided: the full check moved to
  check(), and open reads only the catalog.
- **O4. Start faster.**
  - Begin `WebAssembly.compileStreaming` when the Worker starts, overlapping
    lock and OPFS setup.
  - Import the OPFS runtime and the WASM glue concurrently.

## Build settings

`opt-level = "z"` with `wasm-opt -Oz` costs:

- natively, 1.4–1.7× in instructions;
- in V8 WASM, about 1.15× on point lookups and 1.3–1.7× on scans.

`opt-level = 3` grows the WASM by 47%, which lands at the 1 MiB `check:size`
gate: just under it in one build and just over it in another. `s` grows it by
10%, with no clear gain yet.

The fixes above take most of the sensitive loops off the hot path: CRC,
serde, string-keyed maps and allocator calls. So re-measure after R1–R4 and
C1, and decide [D7](#decisions-needed) with data.

Remeasured on 26 September 2026, raising the optimization level of
`tinyjoin-core` alone, with the other crates left at `z`. Times are
engine-only V8 runs of the benchmark workloads in Node, the best of three, in
milliseconds:

| `tinyjoin-core` | `wasm-opt` | Engine gzip | `insert-transaction` | `insert-batch` | `select-scan` | `group-by` | `update-pk` | `delete-range` | `create-index` |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `z` (shipped) | `-Oz` | 301 KiB | 116 | 76 | 85 | 16.9 | 17.2 | 12.6 | 11.7 |
| `s` | `-Oz` | 326 KiB | 114 | 75 | 80 | 17.0 | 17.6 | 12.9 | 11.4 |
| `2` | `-Oz` | 371 KiB | 102 | 63 | 83 | 15.6 | 14.5 | 9.6 | 9.2 |
| `2` | `-O2` | 374 KiB | 93 | 61 | 83 | 15.7 | 15.2 | 9.5 | 9.2 |
| `3` | `-Oz` | 379 KiB | 96 | 58 | 81 | 15.7 | 14.0 | 9.5 | 9.2 |
| `3` | `-O3` | 380 KiB | 92 | 59 | 81 | 15.7 | 13.7 | 9.7 | 9.2 |

Levels 2 and 3 cut writes by 15–25% but barely move scans, which the fixes
above already made cheap, for 70–80 KiB more compressed, about a quarter of
the engine. Every build stays under the 1 MiB gate: level 3 is 963 KiB
uncompressed. `s` costs 24 KiB for a few percent.

Remeasured on 27 September 2026, on commit `ee2de97`, in the same way:

| `tinyjoin-core` | Engine gzip | `insert-transaction` | `insert-batch` | `select-scan` | `group-by` | `update-pk` | `delete-range` | `create-index` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `z` (shipped) | 308 KiB | 78 | 51 | 70 | 16.5 | 12.1 | 8.8 | 11.5 |
| `s` | +24 KiB | 76 | 51 | 69 | 16.3 | 11.4 | 8.8 | 10.6 |
| `1` | larger | 80 | 53 | 70 | 18.7 | 12.1 | 9.2 | 10.7 |
| `2` | +69 KiB | 65 | 41 | 62 | 15.0 | 9.8 | 7.4 | 9.0 |
| `3` | +69 KiB | 65 | 41 | 62 | 14.9 | 10.0 | 7.7 | 9.1 |

Level 2 still buys 10–25% on the engine alone, for about a fifth of the whole
download, and the engine is only part of each statement's time in the browser.
`s` buys 2–7% for 24 KiB. Keep `z`.

## Decisions needed

| | Decision | Recommendation |
| --- | --- | --- |
| D1 | Validate each record once, when its leaf is loaded or written, and never on every read? | Yes. Page format 3 is built around it, with the full reference validator kept in tests |
| D2 | Should open keep checking every row and index entry, or accept fingerprints, lazy validation, or checking only trees changed since the last validated generation? | **Decided:** open checks only the catalog, and pages and rows are checked as they are read; the full check runs on demand, as check(), like SQLite's `integrity_check` and PostgreSQL's `amcheck`. Large databases opened slowly: 150 ms natively at 100,000 rows, 270 ms with one index. Nothing repairs a database that fails the check yet |
| D3 | Join budgets count candidate comparisons, which index and hash joins mostly avoid. The documented limits and the guides' wording must change. | Count work actually done, and update the SQL compatibility and benchmarks guides |
| D4 | Streaming in primary-key order relaxes the 100,000 ordered-row limit for those queries. | Accept, and document it |
| D5 | Array transport needs a protocol version bump, which makes mixed-version tabs a `DATABASE_VERSION_MISMATCH`. Object keys would follow column order instead of today's alphabetical order. | **Decided:** accepted for v0.4.0, as protocol version 9, with object rows' keys in field order, as its release notes announce |
| D6 | Page format 3 cannot read existing databases. Refuse them or migrate them on open? | **Decided:** refuse with `UNSUPPORTED_PAGE`, with no migration, as announced in the v0.4.0 release notes |
| D7 | Optimization level, trading size for speed. | Keep `z`: remeasured on 27 September, level 2 buys 10–25% on the engine alone for 69 KiB, about a fifth of the download, while the larger per-statement costs lie outside the optimizer's reach |
| D8 | Omit trailing columns that equal their defaults, so `ADD COLUMN` stops rewriting every row? | **Decided:** yes |

## Order of work

1. Phase 0.
2. R1, R2 and R3. They carry low risk and don't depend on the format. They also
   speed up commits, index builds, deletes and reopen.
3. Page format 3, shipping in v0.4.0: S1 and S2 first, then S3 and S4. D1 is
   decided as part of it, and D6 and D8 are already decided.
4. R5 and R6, including text ranges and prefix `LIKE`.
5. R7.
6. T1 and T2. They are independent of the read work and could run alongside
   it.
7. C1, C2 and C3.
8. O2 and O4.
9. R8 and R9.
10. C4, C5, T3 and the build decision.

After step 5, point reads, indexed ranges and joins should be within a few
times SQLite's times, and unindexed scans within about 10× (both estimates).
Closing the rest is ordinary engine tuning.
