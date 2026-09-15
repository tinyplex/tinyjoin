# Workload measurements

## Prelaunch performance improvements

The quick profile compares the same 250-operation mixed transaction and
5,000-row database with 1,024-byte payloads across three samples per build.
It uses the same schema, isolated Chromium contexts, row-model checks, and
reopen/owner/follower checks as the full workload suite below. Run builds and
benchmarks sequentially, without concurrent tests or compilation:

```sh
npm run build
node scripts/benchmark-workloads.mjs /tmp/launch-performance.json --quick
```

On the same Apple M2 and Chromium 151.0.7922.34, measured medians were:

| Build | 250 mixed writes, whole transaction | Populated reopen | First full read |
| --- | ---: | ---: | ---: |
| Baseline | 7,563.1 ms | 4,274.8 ms | 1,353.8 ms |
| Lookup-table CRC-32 | 2,412.8 ms | 3,129.2 ms | 1,091.4 ms |
| CRC-32 + primary-key UPDATE/DELETE | 2,454.6 ms | 3,093.2 ms | 1,106.6 ms |
| All three improvements (five samples) | 2,197.6 ms | 2,152.1 ms | 1,086.8 ms |

`launch-performance-before.json` and `launch-performance-checksum.json` retain
the samples, environment, and runtime hashes. The baseline used an isolated
prototype of the quick profile; the checksum run used the committed runner and
rebuilt package. The baseline run completed first. These are local diagnostics,
not controlled device guarantees.

The checksum optimization preserves the stored checksum values and all page
validation. Its WASM is 738,134 bytes raw / 278,546 bytes gzip, compared with
737,145 / 277,336 before: an increase of 989 raw and 1,210 gzip bytes.
It reduces calculation cost without removing the repeated full validation
performed by mixed transactions.

`launch-performance-primary-key.json` records the next build. Direct primary-key
lookup does not materially improve the 250-row/250-operation shape: complete
write-set validation still dominates it. A complementary workload keeps the
transaction at 25 operations while increasing the existing table to 5,000 rows
with 64-byte payloads. It uses the same insert/update/delete cycle and checks
the final row model and persisted contents:

```sh
node scripts/benchmark-workloads.mjs /tmp/point-writes.json --point-writes
```

| 25 mixed writes against 5,000 rows | Staging median | Whole transaction median |
| --- | ---: | ---: |
| CRC-32, before primary-key lookup | 446.4 ms | 461.3 ms |
| CRC-32 + primary-key lookup | 33.2 ms | 47.7 ms |

The three samples per build are in `point-writes-before.json` and
`point-writes-after.json`. The baseline runtime was reconstructed from the
checksum commit and byte-checked against its recorded hashes; every other
runtime artifact was identical. Runs were sequential. The benefit is avoiding
full-table scans for point mutations as the existing table grows, while retaining
the current complete mixed-transaction validation.

The final build also avoids a redundant count scan for unique indexes whose
columns are all `NOT NULL`. Table validation and full per-entry index checks
remain. Its complete five-sample workload matrix is retained in
`workload-envelope-optimized.json`, and is summarized below. The earlier quick
profiles use three samples; compare these as local distributions rather than
precise guarantees for an isolated optimization.

The final runtime is 793,722 bytes raw / 298,152 bytes gzip. Compared with the
prelaunch baseline, all three improvements together add 1,683 raw bytes and
269 gzip bytes. The WASM is 738,828 raw / 277,605 gzip bytes.

Validation for this build: `TINYJOIN_BROWSER_PORT=4184 npm run check:release`
passed, including 286 native Rust tests, 127 TypeScript tests, 10 real-WASM
contracts, 16 documentation and 19 runtime Chromium tests, packed consumers,
size checks, and restrictive-CSP offline lifecycle checks. The release rebuild
matched every recorded runtime hash. The starter's `npm run test:candidate`
also passed all four generated builds and 15 Chromium lifecycle tests against
an immutable tarball of this candidate.

## Current workload envelope

`workload-envelope-optimized.json` retains five samples for each of nine OPFS
workloads on an Apple M2 (8 logical CPUs, 16 GiB RAM), macOS (Darwin 25.5.0), headless Chromium
151.0.7922.34, and the packaged default Worker. These are local diagnostics,
not a timing gate, a competitor comparison, or a supported device limit. All
runtime JavaScript/WASM byte lengths and SHA-256 hashes are in the artifact.

Practical guidance for this measured shape:

- Keep mixed transactions short. Tens of sequential mixed writes are a sensible
  starting point to measure: 25 operations took about 24 ms, 100 about 296 ms,
  and 250 about 2.2 seconds. All same-name Clients wait for that transaction.
  Multi-row statements can reduce repeated validation; append-only insertion
  has a separate incremental path and is measured below.
- Bound result size. Materializing 100 short rows took a few milliseconds; 5,000
  1-KiB payload rows took about 1.1 seconds. Paging a UI does not help if it still
  requests every row from the database. These timings include execution and
  delivery, not rendering.
- Budget populated startup separately from the first empty open. The empty open
  was normally about 20 ms; reopening the largest measured database took about
  2.2 seconds, followed by another 1.1-second first read. Opening validates the
  stored trees. A warm HTTP cache does not eliminate that work.
- Batch setup/imports deliberately. One transaction containing 5,000 rows with
  1,024-byte payloads exceeded the 16-MiB retained batch limit. Five 1,000-row
  transactions seeded that dataset successfully. Raw payload size is not retained
  transaction memory; this is not a universal safe batch size.

### Mixed staging and commit

The initial table has the same number of rows as operations below, a 64-byte
ASCII payload, an integer primary key, and a UNIQUE text title index. Operations
cycle through inserting a new key, updating an existing title, and deleting an
existing key. Each statement is prepared before timing and awaited inside one
transaction. The exact row model is checked after commit and after reopen.

All times are milliseconds. Each median is computed independently. Staging
includes client/Worker round trips; commit spans callback completion through
transaction resolution, including persistence and coordination.

| Initial rows / operations | Staging median (min–max) | Commit median | Whole transaction median |
| --- | --- | --- | --- |
| 25 | 18.4 (17.8–20.2) | 5.9 | 24.2 |
| 100 | 271.1 (270.5–273.9) | 25.4 | 296.2 |
| 250 | 2,123.6 (2,116.1–2,165.4) | 73.3 | 2,197.6 |

The pre-optimization `workload-envelope.json` remains available for historical
comparison. `workload-pilot.json` retains three earlier 500-operation samples:
staging 29.73–29.92 seconds, commit 411–414 ms. That pilot was stopped before the planned
1,000-operation shape; there is no inferred result for that shape. The repeatable
mixed suite uses 25/100/250 operations to keep it useful for local iteration.

### Populated reopen and result delivery

Result workloads use the same schema/indexes, with all rows present. The initial
sole client opens and seeds the database, closes, and reopens before a second tab
joins as follower. Both routes warm one full read; measured order alternates
across repetitions. Owner and follower results must be identical. Byte counts
below are actual UTF-8 JSON result sizes, not raw string payload estimates.

| Rows | Payload bytes/row | JSON result bytes | OPFS bytes | Reopen median ms | First read median ms | Owner median ms | Follower median ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | 64 | 10,481 | 73,728 | 24.7 | 5.1 | 1.8 | 1.9 |
| 100 | 1,024 | 106,481 | 466,944 | 46.1 | 9.4 | 6.2 | 6.4 |
| 1,000 | 64 | 106,781 | 385,024 | 83.2 | 16.2 | 10.6 | 11.1 |
| 1,000 | 1,024 | 1,066,781 | 4,300,800 | 285.0 | 60.8 | 54.3 | 55.8 |
| 5,000 | 64 | 542,781 | 1,777,664 | 390.7 | 59.5 | 50.0 | 52.2 |
| 5,000 | 1,024 | 5,342,781 | 21,344,256 | 2,152.1 | 1,086.8 | 1,113.6 | 1,074.4 |

The largest payload shape is seeded in 1,000-row transactions; schema setup and
all seeding are excluded from the timings. The final matrix ran to completion
in one Chromium process with no resume.
The earlier baseline report retains its own setup and resume notes.

### Reproduce and interpret

```sh
npm run build
node scripts/benchmark-workloads.mjs /tmp/workload-envelope.json
```

The script binds an available loopback port, uses fresh isolated browser contexts,
and releases storage through client/context cleanup. Output is checkpointed after
each complete shape. `--resume` as a final argument reuses completed shapes after
checking the runtime hashes, package/browser versions, schema, repetitions and
environment; it does not combine partial samples from a failed shape. Use a new file when comparing a new
build, browser or environment. Allow several minutes and avoid concurrent builds
or tests.

“Cold open” means the first database open in a fresh context with a new OPFS name,
after the page has loaded the client module. It includes Worker/WASM first load;
it does not reset the browser process for every sample, OS cache, filesystem,
or machine. Assets come from loopback HTTP. Reopen uses a fresh Worker in the
same context with warm caches permitted. Result timing ends when query() resolves
with materialized rows on the requesting page: it combines the engine, cloning,
and owner/follower transport. JSON sizing and Playwright delivery to Node happen
after that timer. The owner/follower difference is not an isolated copy-cost
measurement.

The artifact provides all samples plus min/median/p90/max. With five samples,
nearest-rank p90 equals the maximum; it is not a reliable estimate of production
tail latency. Only this desktop/Chromium combination was measured. Mobile,
Firefox, WebKit, remote-network downloads, power/thermal control, concurrent
workloads and long-lived storage fragmentation remain unmeasured. Schema width,
indexes, data already present and transaction shape all matter: use these results
to choose application measurements, not as a maximum database size or a promise
for another device.

## Earlier transaction insertion diagnostics

These local before/after measurements accompany the append-only transaction
staging optimization. They are diagnostics, not CI timing thresholds or a
comparison with other databases. Both artifacts use the unpublished 0.0.6
manifest; their WASM hashes identify the actual before and after builds.

The before build was taken at `53b62a5`. Measurements ran on the same Apple M2
machine, without concurrent builds/tests during each run. This was not a
controlled hardware or thermal benchmark.

| Workload | Before | After |
| --- | --- | --- |
| 2,000 inserts, engine staging, no secondary index | 5,952 ms | 25 ms |
| 2,000 inserts, engine staging, UNIQUE text index | 7,550 ms | 26 ms |
| 1,000 inserts, browser memory, whole transaction | 1,557 ms | 304 ms |
| 1,000 inserts, browser OPFS, whole transaction | 1,571 ms | 310 ms |

All numbers are medians of three samples. The engine diagnostic uses Node
v25.2.1 and structured WASM calls on an in-memory page device, with a 100-row
warmup for each index shape. It excludes the client, Worker, and OPFS.
The browser diagnostic uses the actual packaged default Worker and three
fresh databases per storage mode, with primary-key indexing only. It checks
every committed row count and closes the clients/browser afterward.

Initialization, schema setup, statement preparation, and the verification
read are outside the insertion timing. Staging and commit are recorded
separately; the optimization primarily changes staging. Mixed writes still
use the previous complete validation path. Absolute timings vary by run.

To measure the current build:

```sh
npm run build
node scripts/benchmark-staging.mjs /tmp/transaction-staging.json
node scripts/benchmark-browser-inserts.mjs /tmp/browser-inserts.json
```

The browser script requires the project's Playwright Chromium installation
and uses local port 4189. Output paths are optional. Full samples, versions,
and WASM hashes are retained in the four JSON files in this directory.
