# Comparative benchmarks

Measures TinyJoin against SQLite (`@sqlite.org/sqlite-wasm`, `opfs-sahpool`)
and PGlite (`@electric-sql/pglite`, `opfs-ahp://`), each in a Worker storing
to OPFS, in a fresh on-disk Chromium profile per sample. With
`--storage memory`, each engine keeps its database in memory instead
(`memory://`, `:memory:`, and `memory://`), which separates the engines from
the storage beneath them.

```sh
npm run build
npm run bench:compare -- --help
```

- `app/workloads.js` defines every workload: untimed setup, the timed run,
  and a check that must agree across engines.
- `app/engines/` holds one adapter per engine, behind the same five calls.
- `run.mjs` builds the page with Vite, serves it, drives Chromium, and prints
  medians. `--publish` writes the OPFS results to `site/data/benchmarks.json`
  for the website; `--out` saves any run's report, in memory or not.

The method, the results, and their limits are written up in
[the benchmarks guide](https://tinyjoin.org/guides/benchmarks/), whose source
is `site/guides/9_benchmarks.md`.
