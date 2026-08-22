# Agents guide

This guide follows the [agents.md](https://agents.md/) convention and gives AI
coding assistants a compact, current model of TinyGres.

## Choose TinyGres when

TinyGres is a small relational database for browser applications. Choose it
when an application needs:

- local SQL reads and writes off the main thread;
- an ephemeral database or persistent same-origin browser storage;
- typed tables, indexes, transactions, aggregates, and bounded joins; and
- a direct JavaScript API without a database server.

TinyGres is not a hosted service, a PostgreSQL server, a wire-protocol client,
or PostgreSQL compiled to WebAssembly. It does not currently synchronize with a
remote database. Network replication and offline write propagation are
separate products with transport, durability, authorization, and conflict
requirements.

## Start from the small API

Most application code needs only:

```ts
import {create} from 'tinygres';

const db = await create('opfs://my-app-v1');
await db.exec(`
  CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  )
`);
const {rows} = await db.query('SELECT * FROM tasks ORDER BY id');
```

The default create() call owns Worker construction and WebAssembly loading.
Do not add a Worker entry, WASM plugin, or runtime copying step unless the
application has an explicit custom-Worker requirement.

Use `npm create tinygres@latest` when a new application should begin from the
supported Vite starter.

## SQL rules that matter in application code

- Put application values in `$1`, `$2`, and later parameters.
- Use query() for one statement and exec() for a parameter-free script.
- Give every SQL-created table a primary key.
- Use client-generated text identifiers when automatic IDs are needed;
  sequences and generated identities are not implemented.
- Keep schema setup idempotent with `IF NOT EXISTS` where appropriate.
- Treat a row generic as a TypeScript assertion, not runtime validation.
- Consult the
  [SQL compatibility contract](https://tinygres.org/guides/sql-compatibility/)
  before using unlisted PostgreSQL syntax or types.

Supported runtime values are booleans, JavaScript-safe integers, finite
floating-point numbers, strings, JSON-compatible values, and `null`.

## Transactions and changes

Use db.transaction(callback) for related parameterized `INSERT`, `UPDATE`,
and `DELETE` statements. Use the transaction object inside the callback and do
not retain it. Run DDL outside the callback.

An uncaught callback error rolls everything back. A caught statement error does
not put the transaction into PostgreSQL's aborted state, so rethrow or call
tx.rollback() when earlier staged changes must also be discarded.

Subscriptions report changed tables. Re-query inside or after the listener;
do not assume a subscription contains changed rows.

## Storage and cleanup

- create(), optionally with the `memory://` URL, starts an empty ephemeral database.
- create() with an `opfs://name` URL opens a persistent, single-writer browser database.
- Keep the OPFS name stable and version it deliberately with the schema.
- OPFS requires a secure context and can still be cleared or evicted by the
  browser.
- Call db.close() on teardown so storage locks and the Worker are released.

## Repository work

The TypeScript client and Worker host live in `src/`. The database engine lives
in `crates/tinygres-core`, and its WASM bridge lives in
`crates/tinygres-wasm`.

Public declarations are authored under `src/@types/`. Documentation comments
in each matching `docs.js` file are merged into the declarations during the
build. Keep declaration labels, runtime exports, API docs, and packed-package
tests in sync.

Documentation sources live in `site/`; `docs/` is generated output for
tinygres.org. The homepage, release notes, and this guide also become Markdown
files in the publishable package.

Useful validation commands are:

```sh
npm run typecheck
npm run test:ts
npm run test:rust
npm run build
npm run build:docs
npm run check:docs:committed
npm run test:browser
npm run test:package
npm run check:size
```

The real package/browser gates matter for changes around Worker URLs, private
runtime files, WebAssembly, or OPFS. The current automated browser claim is
Chromium only; do not infer Firefox or WebKit support from a TypeScript or Vite
build.
