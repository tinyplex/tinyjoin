**TinyJoin prelaunch audit — 14 September 2026**

Remediation is proceeding in the seven-summary-concern order below. The original audit is preserved after this progress record.

| Concern | Status | Validation |
| --- | --- | --- |
| 1. SQL correctness | Fixed shared predicate/assignment validation and heterogeneous JSON equality | 249 Rust tests; two real-WASM contract tests; typecheck, docs and size checks |
| 2. Resource limits | Pending | |
| 3. Release compatibility | Pending | |
| 4. First-use experience | Pending | |
| 5. Performance | Pending | |
| 6. Practical join scope | Pending | |
| 7. Documentation/site | Pending | |

Reviewed TinyJoin at `d6505bc` and the adjacent create-tinyjoin project. This is an assessment, not an implementation plan or security certification. No product fixes, releases, commits, deployments, or public issues were made. This report is the only added repository file.

**Recommendation: fix the correctness and resource-accounting defects before promoting the current checkout as the first public launch.** The architecture, package validation, documentation of intentional boundaries, and payload are strong enough to build on. A general SQL expansion or engine rewrite would distract from the smaller set of actual launch problems.

The highest priorities are inconsistent JSON predicate semantics, two ways small requests amplify work before resource checks, and a storage-format break hidden behind an unchanged package version. The starter also fails poorly on the expected second-tab case. Performance needs a concrete workload envelope: transaction staging is quadratic, and conservative joins can reject three tables of only 100 rows each.

Severity here expresses launch priority: P1 should be resolved before shipping this checkout; P2 needs an explicit fix, documentation, or deferral decision; P3 is a useful follow-up. “Confirmed” means a runtime or deterministic source-logic reproduction. Source-only risks and intentional limits are identified separately.

| ID | Priority | Finding | Recommended disposition |
| --- | --- | --- | --- |
| 1 | P1 | UPDATE/DELETE accept a JSON predicate SELECT rejects | Fix shared semantic validation |
| 2 | P1 | Repeated SQL parameters allocate amplified AST values before budgets | Fix expanded-value accounting |
| 3 | P1 | Shared parameter graphs cause exponential protocol validation | Fix total traversal accounting |
| 4 | P1 | Current page format cannot reopen npm 0.0.5 databases; version still 0.0.5 | New version and explicit compatibility boundary |
| 5 | P2, fix for launch | Structural equality fails on heterogeneous JSON columns | Fix implemented SQL contract |
| 6 | P2, fix for launch | Recommended starter stays on a spinner in a second tab | Visible failure/retry behavior |
| 7 | P2, fix for launch | Site navigation discards section anchors | Preserve URL/anchor navigation |
| 8 | P2 | Callback transaction writes have quadratic staging cost | Optimize or document batching and measured scope |
| 9 | P2 | Practical join limits are easy to reach with small tables | Concrete examples; decide intended workload |
| 10 | P2 | Uncertain commit and fatal recovery behavior lacks application guidance | Document safe reconciliation |
| 11 | P2 | Nested transaction calls deadlock; no timeout/abort escape hatch | Explicit unsupported-pattern guidance/API decision |
| 12 | P2 | Two demos misdescribe behavior users will copy | Correct demonstrations |
| 13 | P2 | Release gates exist but source/release CI is absent | Run meaningful gates on PRs/releases |
| 14 | P2/P3 | Build/test dependency advisories | Patch Vitest; resolve TinyDocs dependency upstream |
| 15 | P2/P3 | Mobile discovery, agent context, storage lifecycle, smaller SQL gaps | Bounded documentation/UX issues |

**1. Unsupported JSON ordering can change data — confirmed.**

On the rebuilt release WASM:

```sql
CREATE TABLE numeric_json (id INTEGER PRIMARY KEY, payload JSON);
INSERT INTO numeric_json VALUES (1, 3);
SELECT id FROM numeric_json WHERE payload > 2;
-- TYPE_MISMATCH: JSON ordering is unsupported
DELETE FROM numeric_json WHERE payload > 2 RETURNING id;
-- succeeds, deletes id 1, and advances the database revision
```

SELECT performs schema-level predicate type validation before scanning. UPDATE and DELETE validate column existence but omit the type check, allowing runtime scalar comparison of a numeric JSON value. This violates the explicit contract that JSON supports structural equality/inequality only and unsupported forms fail explicitly.

Evidence: [query.rs](crates/tinyjoin-core/src/query.rs), lines 77–80 and 1710–1763; [statement.rs](crates/tinyjoin-core/src/statement.rs), lines 544–549 and 724–728. The same omission makes some invalid predicates succeed when no rows match. Invalid UPDATE assignments can likewise be accepted when no row reaches normalization.

Use common typed validation for SELECT, aggregates, joins, UPDATE, DELETE, and prepared execution before any row-dependent work. Regression coverage should compare empty and populated tables and assert that failed mutations publish no changes. This is consistency within the existing dialect, not a request for additional SQL.

**2. Parameter reuse amplifies memory before the work limits — confirmed.**

An empty table and one 64 KiB string suffice:

```js
const sql = `SELECT id FROM items WHERE payload IN (${Array(384).fill('$1').join(',')}) LIMIT 0`;
const params = ['x'.repeat(64 * 1024)];
```

The 1,199-byte SQL and parameter pass the current TypeScript bridge preflight. The release WASM successfully returns zero rows, but its linear memory grows from **1,769,472 to 26,935,296 bytes**. The parameter is copied 384 times into owned AST values: at least 24 MiB of string copies before any scan, even though `LIMIT 0` makes row work unnecessary.

Evidence: [query.rs](crates/tinyjoin-core/src/query.rs), `bind_parameter` at 1281–1286 and prepared binding at 1368–1378; [wasm-preflight.ts](src/worker/wasm-preflight.ts), 151–159. The ingress budget measures the supplied parameter once; SQL binding clones it for every occurrence. The allowed 1,024-element IN list and near-1-MiB value imply a much larger possible allocation. That dangerous upper case was deliberately not executed.

Charge expanded binding/AST memory before cloning, or retain parameter references instead of copying values at each occurrence. Cover ordinary and prepared queries, IN lists, and repeated values in mutations. The finding concerns availability in the application’s own Worker, not a WASM sandbox escape or cross-origin disclosure.

**3. A tiny shared JavaScript graph causes exponential validation — confirmed.**

```js
let value = 1;
for (let depth = 0; depth < 24; depth++) value = [value, value];
// Use value as one query parameter.
```

This is an acyclic graph with 24 unique arrays, not an enormous JSON text. Structured clone preserves its shared references. The protocol validator removes each object from its cycle-detection set after walking a branch, then walks the same descendants again for the other branch. Work is exponential in depth. A Node 25.2.1 probe directly calling current-source `isWorkerRequest` accepted depths 20, 22, and 24 in approximately **125 ms, 696 ms, and 3.24 seconds**. These are validator timings, not browser/API round trips. Smaller independent reruns showed the same growth; absolute times vary. Higher depths were not run; the Worker-blocking consequence follows from source ordering rather than an extreme browser-hang experiment.

Evidence: [protocol.ts](src/protocol.ts), 401–423; [host.ts](src/worker/host.ts), 315–332. Validation happens synchronously before queueing, so the bridge’s later node/work budgets cannot protect it. Plain `JSON.parse()` does not create such sharing; JavaScript application code can, including by reusing nested objects.

Add a shared total-work budget or safe memoization plus bounded expanded-value accounting. A depth cap and per-array length cap alone are insufficient. This also exposes a resilience limitation: a blocked Worker cannot process the normal close request, and the default Client has no public forced-disposal operation.

**4. The launch checkout and published package have incompatible storage formats — confirmed in Chromium.**

The registry currently serves `tinyjoin@0.0.5` from gitHead `76426b0ab6b326086d58ea0ef9185791439d2064`. The current checkout still declares 0.0.5, but its superblock format is 2; the published runtime uses format 1.

Using the exact npm tarball and current dist under the same temporary browser origin, the old runtime created and closed a persistent database. Opening it with the current runtime failed with:

```text
ClientError: UNSUPPORTED_PAGE
Superblock format version 1 is not supported
```

Evidence: [package.json](package.json), line 3; [page.rs](crates/tinyjoin-core/src/page.rs), 41 and 336–340; [release source](site/guides/8_releases.md), which has only the existing 0.0.5 note. Public declaration signatures remain aligned; this is a storage-format/package-release problem.

The experimental warning already permits breaking changes. It does not replace a release-specific compatibility note or a new immutable npm version. Decide whether the next release offers migration/export guidance or deliberately starts a new database name, then publish that boundary explicitly. Current docs and payload claims describe newer code than `npm install tinyjoin` currently installs.

The starter currently installs `^0.0.5`; that range excludes 0.0.6. Update and republish create-tinyjoin with the next TinyJoin version and verify the registry-backed combination. Evidence: [starter CLI](../create-tinyjoin/src/cli.ts), line 132; [generated-app tests](../create-tinyjoin/test/generated-app.test.ts), 99–113. The registry starter is 0.0.6 and its source/templates match that published revision.

**5. Structural equality is unreliable for legal mixed JSON values — confirmed.**

Store `{a: 1}` in one row’s JSON column and `[1]` in another. `WHERE payload = $1` with `{a: 1}` should select the object row; `<>` should select the array row. Both currently raise TYPE_MISMATCH when they encounter the other JSON kind.

Evidence: [query.rs](crates/tinyjoin-core/src/query.rs), `values_equal` at 1514–1522, versus its schema-level JSON allowance at 1744–1760. The SQL contract permits scalar, array, and object JSON in the same column and promises structural equality. The runtime comparator only handles certain matching value variants.

Carry JSON-column semantics into comparison, preserving the documented SQL NULL behavior and strict non-JSON type checks. Test the cross-product of object, array, string, number, boolean, and null, including IN and mutation predicates. This affects an advertised feature and should be fixed rather than explained away with a new limitation.

**6. The persistent starter hides expected failures — confirmed in Chromium.**

The generated production starter works in its first tab. Opening a second tab at the same origin produces an unhandled “Another TinyJoin worker already has this OPFS database open” error, leaves `#loading` visible, and never creates the todo input. There is no explanation or retry path.

Evidence: [app template](../create-tinyjoin/templates/client/src/app.ts.hbs), 12–19, and [entry template](../create-tinyjoin/templates/client/src/index.ts.hbs), 5–6. The UI awaits startup without catching rejection. Unsupported storage or corruption follows the same source-level error path, although those failures were not separately injected here.

Mutation handlers also ignore rejected writes. [todoInput.ts.hbs](../create-tinyjoin/templates/client/src/todoInput.ts.hbs), 15–20, clears input before insertion succeeds; the list template similarly ignores mutation failures. This is a source-confirmed broader problem, not a claim that quota failure was reproduced.

Keep the single-writer design. Catch open failures, display actionable feedback, and retain user input until a write succeeds. Add a duplicate-tab failure test and a representative rejected-write test. The supported starter should demonstrate how applications live within the library’s boundaries.

**7. Documentation navigation drops anchors — confirmed with the actual navigation logic.**

The handler intercepts both `#if-tinyjoin-is-not-the-right-fit` and `/guides/sql-compatibility/#hard-limits`, but passes only `link.pathname` to its navigation function. It fetches a page fragment, pushes a hashless URL, and scrolls to the top. These important caveat links do not reach their intended sections.

Evidence: [nav.ts](site/js/nav.ts), 22–40 and 43–52. A deterministic test of the transpiled source recorded those exact hashless fetch/history operations. This finding does not rely on a screenshot. Preserve same-document anchors, hashes on cross-page navigation, and history restoration.

The navigation implementation also has no stale-request protection: two overlapping navigations can finish out of order. That is a source-level follow-up worth covering alongside anchors. Do not turn this small fix into a site rewrite.

**8. Performance: ordinary transaction loops become expensive — measured.**

For every statement, `PagedTransaction::stage` validates and clones the entire accumulated candidate write set. Prepared statements avoid parsing but do not remove this work. Evidence: [paged_transaction.rs](crates/tinyjoin-core/src/paged_transaction.rs), 124–128 and 153–230; [paged_storage.rs](crates/tinyjoin-core/src/paged_storage.rs), 216–279.

A second exploratory run after the release suite finished used Node 25.2.1, the rebuilt release WASM, and an in-memory JavaScript PageDevice. The table had an integer primary key and a short text column. Single-row inserts were prepared; the comparison used INSERT batches of up to 250 rows.

| Rows in one transaction | Single-row staging | Batched staging | Commit after staging, approximately |
| ---: | ---: | ---: | ---: |
| 100 | 26 ms | 1.0 ms | 15–17 ms |
| 250 | 85 ms | 1.9 ms | 49–52 ms |
| 500 | 325 ms | 4.6 ms | 107–109 ms |
| 1,000 | 1,287 ms | 11.3 ms | 225–226 ms |
| 2,000 | 5,229 ms | 33.0 ms | 462–463 ms |

Doubling from 500 to 1,000 to 2,000 rows roughly quadruples staging time. At 2,000 rows, batching reduces staging by about 158×; including commit, the comparison is approximately 5.69 seconds versus 0.50 seconds. These are local diagnostic measurements, not a benchmark against other databases or browser/OPFS throughput claims. An earlier run during parallel checks had slower absolute times and the same scaling.

A targeted improvement would maintain incremental overlay validation/counters and validate final publication once, while preserving statement atomicity and caught-error semantics. If deferred, show safe parameterized batching and an honest practical import envelope. Do not suggest that prepare() alone solves bulk-write cost.

Other performance boundaries deserve explicit guidance:

- Secondary indexes are disabled throughout callback transactions, even before any writes. Primary-key lookup remains available. See [paged_transaction.rs](crates/tinyjoin-core/src/paged_transaction.rs), 438–461.
- UPDATE and DELETE scan even with a primary-key predicate. This is already documented in the SQL contract.
- Aggregate execution visits the full table rather than reusing SELECT’s lookup path. See [aggregate.rs](crates/tinyjoin-core/src/aggregate.rs), around 167.
- Opening an existing database validates tables and indexes, including row decoding and index checks. Startup grows with dataset/index size; there is no established startup latency envelope. See [paged_storage.rs](crates/tinyjoin-core/src/paged_storage.rs), 816–1039.

The existing Chromium latency test returned p50 **0.4 ms**, p95 **1.1 ms**, p99 **2.5 ms** for a warmed tiny two-row-result query. The 10,000-row OPFS persistence fixture reported initial population **10.64 s**, a single-row update **252 ms**, and reopen **494–509 ms**. These are observations from a parallel correctness suite, not controlled benchmarks. They warrant investigation; they do not establish universal performance. The current caveat correctly makes no published throughput claim, though its suggestion that a bounded planner has no pathological cases should be softened in light of these results.

**9. Three small tables can exceed the join bound — confirmed, already documented in principle.**

With 100 primary-key rows in each of `a`, `b`, and `c`, this query is rejected even though it has only 100 actual matches:

```sql
SELECT a.id FROM a
JOIN b ON a.id = b.id
JOIN c ON b.id = c.id;
```

Adding a selective WHERE clause and `LIMIT 1` still rejects. The conservative candidate estimate is `100 × 100 + 100 × 100 × 100 = 1,010,000`, above the one-million bound. Evidence: [join.rs](crates/tinyjoin-core/src/join.rs), 218–233. Execution uses nested loops; index selectivity is not used to reduce this preflight product.

The SQL contract already explains conservative bounds, so this is an intentional limit, not a hidden implementation bug. However, “up to eight tables” is a poor guide to the practical envelope. Add a concrete example near the many-to-many documentation. Decide whether this serves the intended launch workloads. Tighter bounds or index/hash joins are deliberate product work; simply raising the cap trades rejection for more work.

**10. Applications need the uncertain-commit contract — missing documentation.**

The storage implementation carefully distinguishes abortable failures from failures after publication may have happened. It poisons unusable engines and exposes errors such as `RECOVERY_REQUIRED`, `STORAGE_COMMIT_OUTCOME_UNKNOWN`, and `STORAGE_ENGINE_POISONED`. The guides do not explain these states or safe replay behavior.

Evidence: [pager.rs](crates/tinyjoin-core/src/pager.rs), 430–467 and 504–510; [WASM bridge](src/worker/wasm-bridge.ts), 147–206 and 308–312; [WASM engine](crates/tinyjoin-wasm/src/lib.rs), 191–211. Existing fault tests support the implementation’s caution.

A rejected write Promise is not always proof that nothing committed. Likewise, a Worker can commit and disappear before delivering acknowledgement. Document stopping use of that Client, closing/reopening the same persistent database, and inspecting/reconciling persisted state before replaying a mutation. Explain stable application IDs and idempotent operations. An ordinary retryable flag is not a guarantee that repeating a write is harmless. Memory databases cannot recover lost data by reopening.

**11. Transaction re-entry and cancellation need explicit boundaries.**

This confirmed pattern forms a Promise dependency cycle:

```js
await db.transaction(async () => {
  await db.transaction(() => 42);
});
```

The outer transaction waits for its callback; the inner transaction is queued after the outer one. A deterministic current-client probe emitted only `init` and `beginTransaction` and never progressed. Evidence: [client.ts](src/client/client.ts), 280 and 415–419. The existing advice to use `tx` helps, but does not expressly warn that nested Client transaction calls deadlock. Show helpers accepting `tx` rather than starting another Client transaction.

Rejecting every transaction while a callback is active would change the documented queue behavior for independent callers. This needs a deliberate API choice; documenting unsupported nesting is an honest initial disposition.

Initialization, requests, and close also have no timeout, AbortSignal, queue bound, or public forced-disposal method. See [rpc.ts](src/client/rpc.ts), 130–155, and [client.ts](src/client/client.ts), 303–312. A Worker that loads without starting its host, or one stuck before replying, can leave promises and normal cleanup pending. Document that `Promise.race` stops waiting but does not cancel work or release the OPFS lock. Any future cancellation design must account for uncertain write outcomes.

A smaller ergonomics issue: after a terminal Worker error, a probe reported `terminated=true`, `ready=true`, `closed=false`; subsequent queries correctly failed with WORKER_TERMINATED. The narrow ready documentation describes initialization, so this is not a clear API-contract violation. Consider propagating terminal state to Client/prepared handles, or state clearly that ready is not a health check.

**12. Correct the two copyable demos.**

- The [bank transaction demo](site/demos/2_bank_transactions.md), 66–77, checks insufficient funds and throws before either UPDATE. Its prose at 82–84 says a first update ran and was rolled back. The failed example therefore does not demonstrate the rollback it claims. Deliberately fail after a staged write, or change the explanation.
- The [todo demo](site/demos/1_todo_starter.md), 29–30, suggests switching memory to `opfs://todos` to keep data. Its unconditional CREATE TABLE at 41–47 fails on reload. Merely adding IF NOT EXISTS would still repeat unconditional seed additions at 130–131. Show complete persistent initialization or direct users to the persistent starter.

These are documentation/demo defects. The engine’s actual rollback and normal starter persistence tests passed.

**13. Strong local release gates are not running as source CI.**

The repository contains only [.github/workflows/site.yml](.github/workflows/site.yml). It checks committed documentation and deploys it on selected main-branch changes. It does not run Rust, TypeScript, WASM contract, browser, package, or size validation on PRs. Changes under the engine can bypass even the workflow’s path filter.

Use the existing scripts as a source/release workflow with the pinned Rust toolchain, lockfiles, Chromium, and appropriate artifacts. This audit’s passing gate is a snapshot, not continuing protection. Add targeted semantic/resource regressions from this review; avoid substituting more happy-path tests for the missing cross-statement cases.

**14. Dependency security: development tooling findings, no production npm findings.**

Live `npm audit` reported four affected dev-tree packages representing two underlying advisories:

- Vitest and `@vitest/mocker` at 4.1.10: a moderate path-traversal/file-read advisory, patched in 4.1.11. The upstream advisory scopes unauthenticated exposure to particular reachable development-server plugin configurations; this is not evidence that TinyJoin applications expose such a server. [Upstream Vitest advisory](https://github.com/vitest-dev/vitest/security/advisories/GHSA-82fw-gwwq-j7x9).
- TinyDocs’ `html-minifier` dependency: a high-severity ReDoS advisory affecting build-time parsing. The dependency processes documentation input, not runtime database input. The audit’s proposed downgrade to TinyDocs 0.1.1 is not a sensible automatic fix; address or replace the dependency upstream. [Upstream issue](https://github.com/kangax/html-minifier/issues/1135).

`npm audit --omit=dev` reported zero findings. TinyJoin ships no runtime npm dependencies; Rust/WASM dependencies are still part of the shipped code and this result does not cover them. The locked WASM third-party notices check passed. This review did not perform a complete Rust advisory audit, cryptographic review, penetration test, or historical secret scan.

Security strengths include parameter-only SQL interpolation, Worker execution, strict protocol/bridge boundaries, careful hostile-value handling, explicit error codes, bounded page structures, and fail-closed storage behavior. The storage guide correctly says names are namespaces, not encryption or access-control boundaries. I found no evidence of network exfiltration or cross-origin access in the inspected runtime. The two resource-amplification findings remain substantive despite those strengths.

**15. Documentation, newcomer UX, and agent adoption follow-ups.**

The library’s multi-tab caveat is already unusually clear. [Caveats](site/guides/4_caveats.md), 48–65, says one writer/effectively one tab, explains the exclusive lock and local-only subscriptions, names absent coordination mechanisms, and suggests application strategies. Both lock rejection and independent database names are tested. Lack of multi-tab concurrency itself should not be filed as an undocumented defect. The missing piece is starter behavior and copyable failure handling.

Other boundaries are also documented: Chromium-only verification, secure contexts, no silent storage fallback, eviction/clearing, experimental formats, no synchronization/server, hard resource limits, and extensive unsupported SQL. Foreign keys, CHECK, upsert, arithmetic, LIKE, dates/decimal, and subqueries are explicit non-goals today. Adding them is a roadmap choice, not necessary “feature completeness” work for this audit.

Useful small documentation issues:

- Explain database backup/reconstruction, reset, and obsolete-name cleanup. Versioning an OPFS name neither migrates nor removes the old database. There is no public export/import/list/delete-database workflow.
- Explain that DELETE/DROP free pages for internal reuse; physical files retain their high-water size. There is no normal VACUUM/compaction API. Logical deletion should not imply immediate quota reclamation.
- Document the deployment boundary: supported Vite path, other bundlers’ unverified status, Worker/WASM asset URLs and MIME types, and applicable CSP requirements. A restrictive policy applied to a Worker can require explicit WebAssembly permission; avoid recommending broad unsafe-eval indiscriminately. [Browser CSP reference](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/script-src).
- Explain smaller SQL restrictions found in probes: joins reject `*` and `table.*`; a dot-containing quoted column makes the table unjoinable even if unselected. See [join.rs](crates/tinyjoin-core/src/join.rs), 629–641 and 1204–1208.
- Fix duplicate simple projections consistently: `SELECT id,id` returns duplicate fields when empty, but COLUMN_NOT_FOUND when a row matches. An internal prepared-marker-shaped JSON object can also be mistaken for an ordinary LIMIT placeholder and produce LIMIT 0 behavior instead of a type error. See [query.rs](crates/tinyjoin-core/src/query.rs), 294–306, 826–839, 1302–1322, and 1827–1839. These are lower-priority validation defects, suitable for the shared parser/AST work.

The homepage provides a sensible path from install/create through parameters, queries, transactions, prepared statements, and subscriptions. Canonical examples avoid custom Worker boilerplate. Caveats and alternatives are substantive. A brief experimental/Chromium/single-writer cue closer to the first persistent example would help users encounter the consequential boundaries before copying it.

Mobile discovery needs attention: [layout.less](site/less/layout.less), 23–27 and 97–115, hides primary navigation/search below 38rem and the documentation sidebar below 60rem, without a replacement menu. Breadcrumbs and the homepage still provide routes, so users are not completely trapped, but navigation is needlessly indirect. The search uses selectable-looking list items without an announced combobox/active-option relationship. These are source-based UX/accessibility findings; full visual and screen-reader QA remains outstanding.

Agent support is already a strength: compact AGENTS guidance, packed declarations and SQL contract, default Worker ownership, explicit assertions-versus-validation advice, and a noninteractive starter with machine-readable options. No public export/declaration signature mismatch was found in the inspected surface. Improve it with:

- A genuinely comprehensive one-fetch plain-text reference. `llms-full.txt` currently contains only the compact agent guide, omitting the SQL contract, caveats, and API; `llms.txt` does not link it. See [site/build.ts](site/build.ts), 126–129, and [llms.txt](site/extras/llms.txt).
- Packed local caveats and error/recovery guidance so agents can ground themselves without a live docs domain.
- Correct generated [AGENTS.md](../create-tinyjoin/templates/AGENTS.md.hbs), 18–20: seed rows are created only when the table does not exist, not whenever it is empty. The latter advice risks resurrecting deleted todos.
- Add one-tab/retention caveats to the generated README, plus a complete noninteractive command and validated examples for safe batching and transaction helpers.

**Code quality and modularization assessment.**

The core layering is sound: PageDevice → pager/cache → B-tree/codec → paged storage → SQL execution → PagedEngine → WASM/Worker → Client. Most modules are private, public exports are deliberately limited, and Rust denies unreachable public exports. The older whole-store Engine is test-only, not shipped WASM bloat. Page publication and storage failure handling have substantial targeted tests.

The main maintenance problem is repeated semantics across statement families. Query, mutation, join, and aggregate modules mix parsing, validation, execution, ordering, and memory accounting. The SELECT-versus-DML bug is concrete evidence of drift. Share typed predicate/assignment validation and AST/binding rules first. Consider separating B-tree traversal, codec, and overflow handling as those areas change; its roughly 2,400 lines of production code justify review, not an automatic rewrite. Client lifecycle/transaction/prepared state can similarly be separated behind the existing API if maintenance becomes difficult.

The build already bundles internal modules, so sensible source modularization need not add browser requests. Preserve that property and measure emitted bytes. Do not add generic frameworks or broaden SQL as a side effect of cleanup.

The most useful next test layer is a semantic matrix: supported predicate × runtime type × empty/populated × direct/prepared × standalone/transaction × indexed/scanned. Model/property or fuzz tests for parser input, structured values, persisted pages, and atomic mutation sequences would complement the current example-driven suite. No dedicated fuzz/property harness was found. Differential PostgreSQL tests must account for TinyJoin’s deliberate semantic differences.

**Payload assessment: healthy, accurately measured, not a launch blocker.**

The rebuilt default OPFS runtime, with each file compressed separately and the alternative custom-Worker entry excluded:

| Component | Raw bytes | gzip -9 bytes | Brotli -11 bytes |
| --- | ---: | ---: | ---: |
| Main client + shared protocol | 12,613 | 5,124 | 4,527 |
| Worker + WASM glue + OPFS module | 26,722 | 10,661 | 9,446 |
| WASM | 729,841 | 274,536 | 217,171 |
| **Complete runtime** | **769,176** | **290,321** | **231,144** |

That is **751.1 KiB raw / 283.5 KiB gzip / 225.7 KiB Brotli** overall. The WASM is **712.7 KiB raw**, leaving **311.3 KiB** under the 1 MiB gate. The displayed 284 KiB gzip total matches measured metadata. Memory mode does not need the private OPFS module, saving 5,541 raw / 2,403 gzip bytes.

The build already uses size optimization, LTO, one codegen unit, panic abort, stripping, wasm-opt -Oz, JS bundling/minification, and lazy OPFS loading. Code occupies 662,862 WASM bytes and data 60,797; there is no large debug/custom-section windfall to remove. The binary dominates transfer, so further tiny JS rearrangements have low priority next to correctness and algorithmic costs. Symbol-level attribution and measured experiments would be needed before promising further WASM savings.

These are reproducible compression measurements, not proof of hosted Content-Encoding. The public domain currently serves parking content, so actual TinyJoin asset compression/cache/MIME delivery cannot yet be verified. No current competitor-size claim was made; comparing only WASM against another product’s complete runtime would be misleading.

**Public activation still remains to be done.**

Read-only checks during this audit found both GitHub repositories private. `tinyplex/tinyjoin` reports `has_pages=false`; the Pages endpoint returns 404. `https://tinyjoin.org/` serves a small script redirecting to `/lander`, whose assets identify a parking page. This is expected unfinished activation work before a first public launch, separate from library defects.

Before announcing, the selected registry package, starter, public repository, deployed docs, and documented storage format must describe the same release. Verify production MIME, compression, caching, Worker URLs, demos, and root links once the intended site exists. No visibility, DNS, Pages, or publishing settings were changed here.

**Verification completed and its limits.**

| Check | Result |
| --- | --- |
| TinyJoin `npm run check:release` | Passed after allowing installed toolchain/browser access outside the sandbox |
| Rust tests | 237 core + 10 WASM-side native tests passed |
| TypeScript tests | 100 passed |
| Structured WASM contract test | 1 passed |
| Chromium suite | 8 passed, including real Worker, joins, writes, OPFS reopen and browser-process restart |
| Packed consumers | Default and custom Worker, memory and OPFS, fresh installs/reloads, SSR import and Vite build passed |
| Docs freshness/build/checks and demo-runtime check | Passed as part of release gate |
| WASM size gate | Passed; published size metadata matches |
| Clippy across workspace/all targets, warnings denied | Passed |
| Locked WASM third-party notices | Passed |
| npm production audit | Zero findings; dev findings described above |
| Starter typecheck and CLI suite | Passed; 17 CLI tests |
| Registry-backed generated starter matrix | All four JS/TS × memory/OPFS builds passed |
| Existing starter Chromium tests | 2 passed, including CRUD/reload and no seed resurrection |
| Additional probes | Confirmed findings above, including duplicate-tab spinner and old/new OPFS incompatibility |

The browser persistence test forcibly terminates a Worker after an acknowledged commit. That verifies acknowledged data survives termination; it is not an exhaustive real-browser kill-at-every-write test. Rust fault-injection tests separately cover torn writes, failed flushes, and recovery. No broad browser corruption fuzz campaign was performed.

Firefox/WebKit, mobile hardware, controlled cold-start/network benchmarks, real deployed TinyJoin HTTP delivery, visual responsive QA, and screen-reader behavior were not validated. The computer-use interface exposed no browser for visual inspection; automated Chromium tests and source logic were available. A suspected pagehide/BFCache problem was explored, but back navigation performed a full reload and worked. It is not counted as a confirmed defect.

The initial sandboxed release attempt stopped at wasm-pack permissions, and the first registry audit could not resolve npm. Approved reruns completed. Those environment failures are not product findings. Repository source and generated outputs remained unchanged after validation.

**Decision to take from the audit:** preserve the small product. Fix the semantic/resource defects, make the release boundary explicit, and repair the first-use failure paths. Decide consciously whether to improve transaction/join performance now or document a genuinely useful workload envelope. Multi-tab coordination, broader SQL, backup APIs, fuzzing, and deeper modularization can then be scoped as separate work rather than becoming an open-ended launch rewrite.
