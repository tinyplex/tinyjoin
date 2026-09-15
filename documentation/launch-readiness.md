# Launch readiness evidence

Checked 2026-09-15 06:15 UTC. This is a
read-only preflight of the currently deployed site plus a local candidate status
record. It does **not** establish that the candidate has been deployed or published.

## Deployment and registry state

| Item | Observed state |
| --- | --- |
| Local TinyJoin candidate | `0.0.6`; public npm latest is `0.0.5`; exact `tinyjoin@0.0.6` lookup returns E404 |
| Local starter candidate | `0.0.7`; public npm latest is `0.0.6`; exact `create-tinyjoin@0.0.7` lookup returns E404 |
| Starter repository | Authenticated GitHub query reports `tinyplex/create-tinyjoin` as PRIVATE |
| Live candidate match | Homepage, full agent reference, canonical agent guide, and WASM differ from local generated `docs/`; expected before the pending deployment |

Registry versions were checked with `npm view`, and repository visibility with
`gh repo view tinyplex/create-tinyjoin --json isPrivate,visibility,url`. No package,
GitHub, DNS, visibility, or deployment changes were performed.

## Live HTTPS and assets

HTTP apex, HTTP www, and HTTPS www each returned one 301 redirect to
[HTTPS apex](https://tinyjoin.org/), which returned 200. TLS certificate checks
were enabled. The [homepage](https://tinyjoin.org/),
[full agent reference](https://tinyjoin.org/llms-full.txt),
[agent-reference index](https://tinyjoin.org/llms.txt), and
[canonical agent guide](https://tinyjoin.org/guides/agents-guide/) returned 200,
with the expected HTML or plain-text MIME type and negotiated `Content-Encoding:
gzip`. The optional root `/agents.md` probe returned 404; the website's generated
agent guide is the canonical `/guides/agents-guide/` page, and no root `agents.md`
is generated in `docs/`.

All published runtime paths below returned 200 and actual gzip responses. JS MIME
was `application/javascript; charset=utf-8`; WASM MIME was `application/wasm`.
`curl --location --compressed` retained response headers, wire download bytes,
and decoded bodies; SHA-256 compares decoded bodies against local `docs/lib`.
These are negotiated responses, not an assumption based on `.gz` sibling files.

| Path under `/lib/` | Decoded bytes | Gzip wire bytes | Matches local candidate? |
| --- | --- | --- | --- |
| `index.js` | 10,204 | 4,073 | Yes |
| `protocol.js` | 3,530 | 1,474 | Yes |
| `wasm/tinyjoin_wasm.js` | 9,553 | 3,463 | Yes |
| `wasm/tinyjoin_wasm_bg.wasm` | 738,352 | 282,267 | No |
| `worker/default-entry.js` | 26,066 | 9,188 | Yes |
| `worker/index.js` | 26,101 | 9,206 | Yes |
| `worker-opfs/tinyjoin_opfs_runtime.js` | 5,541 | 2,405 | Yes |

The six-file default Worker graph totals 793,246
decoded bytes and 302,870 wire bytes for these
responses. The alternative `worker/index.js` entry is excluded from that sum.
All six JavaScript artifacts match the candidate exactly; the deployed engine is
the earlier build:

- Deployed WASM: `591ef50594362ceac3cbdb1702c88fa668cd7a79b307009ece2765fb1f045c81`
- Candidate WASM: `c4a5996edb590e4a44a1b2509e6835cab8a83114a61bf7171ea2346336935f2b`

## Live browser checks

Headless Chromium `151.0.7922.34`, fresh isolated contexts:

| Check | Result |
| --- | --- |
| Existing `test/site/demos.spec.ts`, using live base URL | 2 passed in 11.5 seconds: Todo add/toggle/delete/reload reset; bank commit and rollback with balance checks |
| Two tabs import live `/lib/index.js`, open one unique OPFS name | Passed: follower read owner write, owner read follower write |
| Close both Clients, reopen same OPFS name | Passed: both exact rows preserved |
| Page errors | None |
| Runtime network graph during OPFS check | Six default assets returned 200 with JS/WASM MIME and gzip |
| Service-worker registrations in fresh docs-site context | Empty |

The OPFS name was unique to a disposable browser profile. Clients were closed,
then the context/browser were discarded; no existing user database was opened.
The browser checks exercised the **deployed** engine hash above, so repeat them
after publishing the candidate. Temporary configs/logs are under
`/tmp/tinyjoin-live-preflight/`; tests used a live base URL with no local web server.

## Required activation checks

- Run the new GitHub validation workflows on the pushed commits; local evidence
  does not establish execution on the Actions runner.
- Deploy the candidate docs/runtime, then repeat the HTTPS, deployed-hash, demo,
  and same-name two-tab OPFS checks against those exact assets.
- Publish/activate TinyJoin `0.0.6`, then registry-test and publish the starter
  `0.0.7`; perform a clean `npm create tinyjoin@latest` check using registry packages.
- Resolve the private starter repository's public launch/link availability.
- Deploy a production app generated from the released starter to a real HTTPS
  origin. Verify installed caching, offline reload, OPFS data retention and two-tab
  behavior there. The documentation site has no offline service worker; its
  successful demos and OPFS checks do not verify deployed offline-app behavior.

## Combined candidate validation

The full gate's checks passed locally, with two test-harness corrections followed
by focused reruns and completion of the remaining stages:

| Check | Result |
| --- | --- |
| Rust advisory audit | 29 packages; no vulnerabilities or warnings; revision recorded in `ci-and-security.md` |
| TypeScript typecheck | Passed for library, docs, and browser fixtures |
| Native Rust | 266 core and 10 WASM-crate tests passed, including deterministic recovery and reduction |
| TypeScript tests | 127 passed |
| Real-WASM contracts | 10 passed, including literal documentation examples and backup boundaries |
| Generated documentation | 328 HTML pages/link checks; freshness and runtime byte matching passed |
| Site Chromium tests | 16 passed, including search keyboard and mobile navigation |
| Runtime Chromium tests | 19 passed, including OPFS, owner loss, transactions, and process restart |
| Packed consumer | Default/custom Workers, fresh OPFS reopen, SSR import, Vite production build, and local guides passed |
| Restrictive CSP/offline | First persistent open offline, follower write, close/reopen, native waiting updates, torn deployment rejection, and preserved response policies passed |
| Size | WASM 737,145 raw / 277,336 gzip bytes; size gate passed |
| Starter candidate | All four generated builds and 15 Chromium lifecycle tests passed using an immutable local tarball |
| Starter tooling | 17 CLI tests, typecheck, spelling, and clean npm audit passed |
| UI review | Chrome at 1440px and 390px; adoption caveat, starter command, search selection and native accessibility state reviewed |

The new backup test initially exceeded its five-second timeout because setup
committed 1,001 rows separately. Bounded 250-row setup batches preserve all
boundary cases and pass within the original limit. Runtime browser tests initially
reused a docs preview on port 4173. The fixture now requires its own server and
strict port; `TINYJOIN_BROWSER_PORT=4184 npm run test:browser:built` passed all 19
tests. These failures did not expose an engine regression.

## Issue disposition

Focused commits use closing references for TinyJoin #1, #2, #4, #5, #6, #7, #9,
#10, #12, #13, #14, and #16, plus starter #1 and #2. The generated full agent
reference includes all guide changes. Existing homepage warning edits are
preserved. The accepted TinyDocs advisory remains closed.

TinyJoin #8 has an implemented and browser-tested search fix, but observed
screen-reader announcements remain unverified. VoiceOver was enabled with the
maintainer's permission, but returned to off before announcements could be
observed. Its final observed setting was off. Native accessibility state is
useful evidence, but does not replace that manual check. The search commit uses
`Refs #8` rather than a closing reference.

TinyJoin #15 remains open for the activation checks above. No registry
publication or starter visibility change is implied by committing these fixes.
