# Native SQL regression corpus

`atomicity.json` retains small replay cases covering failed scripts, catalog
publication, transaction overlays, prepared bindings, and the reducer's injected
failure example. The examples are explicit acceptance cases, not claims of newly
discovered production bugs.

```sh
node scripts/cargo.mjs test -p tinyjoin-core corpus_support -- --nocapture
node scripts/cargo.mjs test -p tinyjoin-core semantic_property_tests -- --nocapture
```

The semantic mutation test uses three fixed seeds and 128 cases per seed. Every
case checks direct/prepared parsing and script execution with and without an
existing transaction. A failure runs the same oracle up to 2,048 times, printing
a JSON `Case` ready to add under `case` in this corpus; add the expected per-operation
error codes under `errors` (`null` means success). `name` retains the original
seed and case index. Normal CI runs never rewrite fixtures.

The reducer deterministically removes operations, UTF-8 SQL spans, trailing bind
slots, and nested JSON members/elements, then simplifies scalar values. It keeps
the exact invariant and error code: changing a partial-commit constraint failure
into a parse error is not an acceptable reduction. This is a budgeted greedy
reduction, not a proof of the globally shortest SQL. Operation order and the
initial schema are fixed. It targets the semantic SQL mutation corpus; pager I/O
fault schedules in `recovery_property_tests` retain their existing exhaustive
cut/seed replay and are not reduced here.

Replay starts with `items(id INTEGER PRIMARY KEY, v TEXT)` and one base row. In
transaction mode a second row is staged first. Failed writes must preserve rows,
revision, transaction state and the optional `audit_marker` table. After rollback
where appropriate, reopening must preserve rows and catalog state. Fixture error
codes also prevent a replay from becoming a vacuous success.

Two reducer self-tests intentionally supply failing test oracles. One simulates
an executor that publishes a write after `COLUMN_NOT_FOUND`; two independent
reductions must produce identical output and oracle-call counts, preserve that
same partial-write invariant, and replay safely against the real engine. Its
retained SQL is `SELECT g FROM items WHERE d=1`. The other proves that nested JSON
parameters shrink while retaining a specific injected parameter mismatch. No
production failure injection or external fuzzing dependency is shipped.
