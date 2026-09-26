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
| --- | ---: | ---: |
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
fingerprint scheme. The v0.4.0 release notes already announce the break.

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
  at open is documented in the v0.3.0 notes.
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

## Decisions needed

| | Decision | Recommendation |
| --- | --- | --- |
| D1 | Validate each record once, when its leaf is loaded or written, and never on every read? | Yes. Page format 3 is built around it, with the full reference validator kept in tests |
| D2 | Should open keep checking every row and index entry, or accept fingerprints, lazy validation, or checking only trees changed since the last validated generation? | Keep it complete, but make it sequential (O2), and revisit if large databases still open slowly |
| D3 | Join budgets count candidate comparisons, which index and hash joins mostly avoid. The documented limits and the guides' wording must change. | Count work actually done, and update the SQL compatibility and benchmarks guides |
| D4 | Streaming in primary-key order relaxes the 100,000 ordered-row limit for those queries. | Accept, and document it |
| D5 | Array transport needs a protocol version bump, which makes mixed-version tabs a `DATABASE_VERSION_MISMATCH`. Object keys would follow column order instead of today's alphabetical order. | Accept for the next minor release |
| D6 | Page format 3 cannot read existing databases. Refuse them or migrate them on open? | **Decided:** refuse with `UNSUPPORTED_PAGE`, with no migration, as announced in the v0.4.0 release notes |
| D7 | Optimization level, trading size for speed. | Decide after page format 3 and C1 |
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
