# Comparative benchmarks

Measures TinyJoin against SQLite (`@sqlite.org/sqlite-wasm`, `opfs-sahpool`)
and PGlite (`@electric-sql/pglite`, `opfs-ahp://`), each in a Worker storing
to OPFS, in a fresh on-disk Chromium profile per sample.

Turso (`@tursodatabase/database-wasm`) is an optional fourth engine for local
comparison: add it with `--engines tinyjoin,sqlite,pglite,turso`. It is not in
the default set, and `--publish` does not accept it. Unlike the others, its
engine runs on the page's thread, with a Worker of its own for OPFS, and its
page is served cross-origin isolated, as its threaded WebAssembly requires.

```sh
npm run build
npm run bench:compare -- --help
```

- `app/workloads.js` defines every workload: untimed setup, the timed run,
  and a check that must agree across engines.
- `app/engines/` holds one adapter per engine, behind the same five calls.
- `run.mjs` builds the page with Vite, serves it, drives Chromium, and prints
  medians. `--publish` writes the results to `site/data/benchmarks.json` for
  the website; `--out` saves any run's report.
- `probes.mjs` holds the CPU probe the runner waits on before each round and
  the flush probe it waits on before each sample, which on macOS issues the
  flush Chromium's OPFS flush issues, through `flush-probe.py`; `orders.mjs`
  gives each round its engine order. `--probe 60` times the probes alone for a
  minute, to see whether the machine is quiet before a long run.

The docs build also summarizes the published results in
`docs/benchmark-card.html`, a 1600x900 card to share, from the template in
`site/benchmark-card.html`. `site/benchmarks.ts` chooses the workloads its
tiles feature, and the build warns if TinyJoin no longer leads in one. After
publishing, build the docs with the new card, capture it as an image, and
build the docs again to publish that image:

```sh
npm run build:card
```

The image is written to `site/extras/benchmark-card.png` and published as
`/benchmark-card.png`, the benchmarks guide's Open Graph image. It is stamped
with a hash of the card it was captured from, and the docs check fails when a
rebuilt card no longer matches it.

The method, the results, and their limits are written up in
[the benchmarks guide](https://tinyjoin.org/guides/benchmarks/), whose source
is `site/guides/9_benchmarks.md`.
