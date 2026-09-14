# Transaction insertion diagnostics

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
