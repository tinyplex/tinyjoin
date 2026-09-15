<!-- cspell:words RustSec zmij bumpalo GHSA gwwq -->

# Release CI and dependency security

`.github/workflows/validate.yml` runs the complete `npm run check:release`
gate on every pull request and main-branch push. It pins Node through
`.node-version`, uses the Rust version and WASM target in
`rust-toolchain.toml`, and installs dependencies with `npm ci`. Rust tests
and WASM builds use `--locked`. It installs Chromium and its system
dependencies explicitly.

The full gate includes Rust tests and the deterministic interruption corpus,
TypeScript, real-WASM contracts, Chromium runtime and documentation tests,
packed consumers, generated documentation, size checks, and offline lifecycle
checks. Console output, including failed corpus seeds and operations, browser
traces, and advisory JSON are retained as workflow artifacts for 14 days.
No path filter excludes engine or runtime changes. The job has read-only
repository access, does not persist checkout credentials, and has no publishing
or deployment credentials, including on fork pull requests.

`npm run prePublishPackage` installs locked dependencies, then runs this same
full gate before `publishPackage` can publish. The gate starts with spellcheck,
then installs or reuses the pinned auditor before running the remaining checks.
Spellcheck excludes `.cache/` and is not repeated during the build.
The independent Pages deployment workflow keeps its deployment permissions;
source validation does not use them. Require the `Full release validation`
check in branch protection when enabling merge enforcement.

## Repeat the Rust advisory audit

```sh
npm run setup:rust:advisories
npm run check:rust:advisories
```

The setup command installs maintained `cargo-audit` 0.22.2 with its own locked
dependency graph into `.cache/tinyjoin/audit-tools/`. This ignored cache lives
outside `node_modules`, so `npm ci` preserves it and repeated setup calls reuse
the installed version. CI caches the auditor by OS, architecture, auditor
version, and Rust toolchain; the first run for a new key compiles it once.

The check refreshes the
[RustSec advisory database](https://github.com/RustSec/advisory-db), audits
the entire committed `Cargo.lock` without architecture or OS filtering, and
fails on vulnerabilities or informational warnings, including yanked crates.
Network or database failures fail the check; stale/offline success is not
silently substituted. There are no advisory exceptions.

The advisory database lives in `.cache/tinyjoin/advisory-db/` and is refreshed
on every check, including when the auditor is reused. CI caches the auditor
installation, not audit results or the advisory database.

Reports in `.cache/tinyjoin/security/` record the audit result,
advisory database revision/date, check time, lockfile SHA-256, and target
dependency inventory. The inventory follows normal dependencies from
`tinyjoin-wasm` for `wasm32-unknown-unknown`, excluding procedural macro
targets and their host-only dependency trees from the runtime group. Build
and test dependencies are still included in the full audit. This is a
dependency-level inventory, not a claim that every crate function survives
the WASM optimizer. It does not audit the Rust standard library or downloaded
build executables; the licensing notice check remains separate.

### Recorded audit: 15 September 2026

- Command: `npm run check:rust:advisories` using `cargo-audit 0.22.2`.
- Checked at: `2026-09-15T10:28:30.147Z`.
- RustSec revision: `e2e640471715167f73e22eaf761f2e547adafeec`.
- Database commit date: `2026-09-14T18:06:06+02:00`; 1,246 advisories.
- Lockfile SHA-256:
  `383903f5e11f0d173ea8eea6885b147a024a0080d93f5ee3b2a3f959d54df3e4`.
- Result: 28 locked packages checked; zero vulnerabilities, zero warnings,
  and no ignored advisories. This is a dated snapshot; CI refreshes it.
- External target dependencies: 18 runtime and eight build/macro dependencies.
  The two workspace crates account for the remainder of the lockfile.

Runtime dependencies: `cfg-if`, `futures-core`, `futures-task`, `futures-util`,
`itoa`, `js-sys`, `memchr`, `once_cell`, `pin-project-lite`, `serde`,
`serde-wasm-bindgen`, `serde_core`, `serde_json`, `slab`, `unicode-ident`,
`wasm-bindgen`, `wasm-bindgen-shared`, and `zmij`.

Build/macro dependencies: `bumpalo`, `proc-macro2`, `quote`, `rustversion`,
`serde_derive`, `syn` 3, `wasm-bindgen-macro`, and
`wasm-bindgen-macro-support`. Exact versions are recorded in `Cargo.lock`
and each generated inventory.

## JavaScript audit snapshot

Both TinyJoin and create-tinyjoin now lock `vitest` and `@vitest/mocker` to
5.0.1. The full npm audits on 15 September 2026 no longer report
[GHSA-82fw-gwwq-j7x9](https://github.com/vitest-dev/vitest/security/advisories/GHSA-82fw-gwwq-j7x9).
The starter audit reports zero findings.

TinyJoin's full tooling audit still reports
[GHSA-pfq8-rq6v-vf5m](https://github.com/advisories/GHSA-pfq8-rq6v-vf5m)
for `html-minifier` 4.0.0 through TinyDocs 0.1.76 (two high-severity dependency
entries for the same advisory). These are the latest published versions at
this snapshot; no supported patched upgrade is available. TinyDocs invokes
the minifier on repository documentation and rendered examples at build
time. It is not a dependency of the shipped browser database. Malicious
documentation input could consume build CPU until the CI timeout.

The maintainer explicitly accepted this existing documentation-build risk on
15 September 2026; its issue remains closed and it is not a launch blocker.
No audit suppression or dependency override was added. Reassess when TinyDocs
replaces this dependency or the documentation input boundary changes.
