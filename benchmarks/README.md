# Workload measurements

## Launch workload envelope

`workload-envelope.json` retains five samples for each of nine OPFS workloads on
an Apple M2 (8 logical CPUs, 16 GiB RAM), macOS (Darwin 25.5.0), headless Chromium
151.0.7922.34, and the packaged default Worker. These are local diagnostics,
not a timing gate, a competitor comparison, or a supported device limit. All
runtime JavaScript/WASM byte lengths and SHA-256 hashes are in the artifact.

Practical guidance for this measured shape:

- Keep mixed transactions short. Tens of sequential mixed writes are a sensible
  starting point to measure: 25 operations took about 59 ms, 100 about 929 ms,
  and 250 about 7.8 seconds. All same-name Clients wait for that transaction.
  Multi-row statements can reduce repeated validation; append-only insertion
  has a separate incremental path and is measured below.
- Bound result size. Materializing 100 short rows took a few milliseconds; 5,000
  1-KiB payload rows took about 1.35 seconds. Paging a UI does not help if it still
  requests every row from the database. These timings include execution and
  delivery, not rendering.
- Budget populated startup separately from the first empty open. The empty open
  was normally about 22 ms; reopening the largest measured database took about
  4.3 seconds, followed by another 1.4-second first read. Opening validates the
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
| 25 | 45.8 (45.0–47.0) | 12.9 | 58.8 |
| 100 | 866.3 (863.8–869.0) | 62.8 | 929.1 |
| 250 | 7,562.9 (7,389.6–7,656.7) | 207.9 | 7,777.3 |

`workload-pilot.json` also retains three completed 500-operation samples: staging
29.73–29.92 seconds, commit 411–414 ms. That pilot was stopped before the planned
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
| 100 | 64 | 10,481 | 73,728 | 36.5 | 5.6 | 2.3 | 2.6 |
| 100 | 1,024 | 106,481 | 466,944 | 75.2 | 14.7 | 11.3 | 11.7 |
| 1,000 | 64 | 106,781 | 385,024 | 181.7 | 21.3 | 16.0 | 16.6 |
| 1,000 | 1,024 | 1,066,781 | 4,300,800 | 545.3 | 111.9 | 105.2 | 107.0 |
| 5,000 | 64 | 542,781 | 1,777,664 | 1,050.1 | 85.4 | 77.0 | 79.5 |
| 5,000 | 1,024 | 5,342,781 | 21,344,256 | 4,293.3 | 1,367.5 | 1,352.0 | 1,341.9 |

The largest payload shape is seeded in 1,000-row transactions; schema setup and
all seeding are excluded from the timings. Its final five samples were resumed
in a new Chromium process after adjusting that setup. The report records both
start and resume times. Earlier completed shapes used identical setup semantics
and the same runtime hashes.

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
