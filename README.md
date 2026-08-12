# TinyGres

TinyGres is an experimental, worker-first relational database for browser apps.
It provides a deliberately bounded PostgreSQL-shaped SQL surface in a small
Rust/WebAssembly engine kept off the browser's main thread.

> [!IMPORTANT] TinyGres is an early database prototype, not PostgreSQL compiled
> to WebAssembly. It intentionally implements only the documented SQL and type
> subset. Its optional Supabase adapter remains read-only, best-effort, and
> reconciles after reconnects; it is not a durable logical-replication stream.

The first proof of concept deliberately does a small number of things:

- owns an in-memory or opt-in persistent database inside a dedicated Web Worker;
- evaluates a documented subset of PostgreSQL-shaped SQL in Rust/WASM;
- supports typed tables, atomic DDL/DML, and staged transactions;
- applies normalized snapshot and server-change batches atomically;
- emits table-level invalidations so an application can re-query; and
- runs an optional built-in Supabase snapshot/Realtime source behind the worker
  boundary, while retaining an adapter seam for future transports.

## Building from source

The repository root is a private development package. A build creates a clean,
standalone npm package in `dist/`, including its public `package.json`, compiled
TypeScript, worker entry, and WASM runtime:

```sh
npm install
npm run build
```

Run `npm pack ./dist` to inspect the publishable tarball. Publishing will also
be done from `dist/`, never from the private repository root. Generated starter
apps and demos belong in a separate `create-tinygres` package built on
`tinycreate`; this repository keeps only minimal fixtures used by automated
browser and package tests.

Application developers consume the prebuilt worker and WASM from the npm
package; Rust is not required in consuming projects. Building this repository
from source requires `rustup`. The checked-in toolchain file selects the Rust
version and `wasm32-unknown-unknown` target, while the npm development
dependency provides `wasm-pack`. The build stages wasm-bindgen output in a
temporary directory and copies only the runtime JavaScript and `.wasm` files
into `dist/wasm`, so generated package metadata never appears under `src/`.
Cargo's compiler cache lives under `node_modules/.cache/tinygres` when using the
project scripts rather than creating a top-level `target/` directory. Use `npm
run cargo -- <arguments>` for other Cargo commands with the same behavior.

If `rustc` comes from Homebrew, install rustup alongside it and activate the
rustup proxies in the current shell:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- --default-toolchain none -y
. "$HOME/.cargo/env"
npm run build:wasm
```

There is no need to uninstall Homebrew Rust. In persistent shell configuration,
load `$HOME/.cargo/env` after Homebrew's `brew shellenv` so the rustup-managed
`rustc` and `cargo` take precedence for direct Cargo commands. The WASM build
resolves the repository toolchain through rustup even when another compiler
appears first in `PATH`, then checks for the target before invoking `wasm-pack`.

## Browser API

```ts
import {createClient} from 'tinygres';

type Post = {
  id: number;
  title: string;
  published: boolean;
};

const db = createClient({
  schemas: [{name: 'posts', primaryKey: ['id']}],
});

await db.ready();

// Snapshot/source integration: this is not an application write API.
await db.replaceTable(
  {name: 'posts', primaryKey: ['id']},
  [{id: 1, title: 'Hello from the worker', published: true}],
);

const result = await db.query<Post>(
  'SELECT id, title, published FROM posts WHERE published = $1',
  [true],
);

const unsubscribe = db.subscribe({tables: ['posts']}, async (event) => {
  console.log('Local tables changed', event.tables, event.revision);
  const refreshed = await db.query<Post>('SELECT * FROM posts');
  render(refreshed.rows);
});

// A read-only source adapter normally owns this integration API.
await db.applyBatch({
  sourceId: 'example-source',
  changes: [
    {
      type: 'upsert',
      table: 'posts',
      row: {id: 1, title: 'Changed on the server', published: true},
    },
  ],
});

unsubscribe();
await db.close();
```

For a standalone writable database, define the catalog and mutate it with SQL:

```ts
const db = createClient({
  storage: {kind: 'opfs', name: 'my-app-v1'},
});
await db.ready();

await db.exec(`
  CREATE TABLE tasks (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false,
    metadata JSONB
  )
`);

await db.exec('CREATE INDEX tasks_done ON tasks (done)');
await db.exec('CREATE UNIQUE INDEX tasks_title ON tasks (title)');

const inserted = await db.exec<Task>(
  `INSERT INTO tasks (id, title, metadata)
   VALUES ($1, $2, $3)
   RETURNING *`,
  [1, 'Ship TinyGres', {priority: 'high'}],
);

await db.transaction(async (tx) => {
  await tx.exec('UPDATE tasks SET done = true WHERE id = $1', [1]);
  await tx.exec(
    'INSERT INTO tasks (id, title) VALUES ($1, $2)',
    [2, 'Survives the same atomic commit'],
  );
  // Queries inside the callback see staged rows. Other state is published only
  // after the callback and its outstanding operations complete successfully.
  console.log((await tx.query<Task>('SELECT * FROM tasks')).rows);
});
```

Every standalone SQL statement is atomic. `transaction()` stages all statements
against an isolated candidate database, rolls them back when the callback or a
statement fails, persists one complete commit to OPFS, and then emits one
table-level invalidation. The transaction object must not escape its callback.
While a replication source is configured, local SQL writes and transactions are
rejected: the current source contract is read-only and a reconciliation snapshot
must never silently overwrite application state.

`createClient` creates a dedicated module worker by default. It is safe
to import during server rendering; the worker is only constructed when the
function is called in a browser.

For an application-owned worker, pass `worker`, `workerFactory`, or `workerUrl`:

```ts
const db = createClient({
  workerFactory: () =>
    new Worker(new URL('./tinygres.worker.ts', import.meta.url), {
      name: 'tinygres',
      type: 'module',
    }),
  schemas: [{name: 'posts', primaryKey: ['id']}],
});
```

The worker entry can install a custom source adapter without moving network or
replication work onto the UI thread:

```ts
// tinygres.worker.ts
import {startWorker} from 'tinygres/worker';

startWorker({source: myReadOnlyReplicaSource});
```

Adapter functions live in the worker and normalize their input into table
snapshots and change batches. They are not serialized through `postMessage`.
When a custom source and OPFS are combined, the application must include the
source identity and authorization scope in its storage name. TinyGres can bind
storage automatically only for serializable built-in source configurations.

## OPFS persistence

Memory remains the default. Opt into persistent browser storage by assigning a
stable name to the database:

```ts
const db = createClient({
  schemas: [{name: 'posts', primaryKey: ['id']}],
  storage: {kind: 'opfs', name: 'my-project-public-posts-v1'},
});

await db.ready();
```

Names must contain 1–64 ASCII letters, numbers, dots, underscores, or hyphens,
and start with a letter or number.

`ready()` restores the saved schemas, rows, and revision before a source
adapter starts. Mutations resolve only after a checksummed append record and
its independent commit marker have been flushed. Recovery replays complete
records over the latest checkpoint, truncates only an incomplete final record,
and fails closed on framed corruption. Two alternating checkpoint/journal pairs
ensure compaction cannot overwrite the last complete state.

OPFS persistence is deliberately single-writer. A second Worker opening the
same name fails rather than risking concurrent mutation; closing or terminating
the owning Worker releases the lock. Different names are independent. Include
the project, dataset, schema version, and authenticated user or authorization
scope in the name whenever those affect which rows may be cached. A name is a
namespace, not an encryption or access-control boundary.

For the built-in Supabase source, TinyGres derives the physical OPFS namespace
from the logical name plus the normalized project URL, publishable key, table
mapping, primary keys, and selected columns. Changing that visibility contract
opens a separate cache instead of exposing rows from the previous one. Database
policy definitions are not part of that client-side fingerprint: change the
logical storage name (or clear the old cache) whenever an anonymous policy or
publication changes.

Synchronous OPFS access requires a secure context and a dedicated Worker. It is
not available in a `SharedWorker`. There is no silent fallback to memory when
OPFS is requested but unavailable, locked, corrupt, or out of quota.

Browser storage is still reconstructable cache data: users can clear it and a
browser may evict best-effort storage under pressure. Applications that need
stronger retention can make an explicit, user-appropriate
`navigator.storage.persist()` request; TinyGres does not make that policy
decision during startup.

The journal checkpoints after 128 records or 1 MiB. A small update to the
10,000-row browser fixture currently appends hundreds of bytes rather than
rewriting its multi-megabyte checkpoint. This transitional engine still exports
one in-memory pre-mutation snapshot so it can roll back a failed OPFS flush, and
caps snapshots and individual journal transactions at 16 MiB. A prepared Rust
commit seam and paged storage are later milestones for removing that remaining
whole-database memory cost and raising the data-size ceiling.

## Supabase adapter

The first source adapter snapshots explicitly selected tables through the
Supabase Data API, then uses Supabase Realtime to accelerate changes. The
default worker needs only a serializable configuration; it includes the narrow
REST and Phoenix WebSocket behavior required for this flow and has no Supabase
SDK dependency:

```ts
import {createClient} from 'tinygres';

const db = createClient({
  storage: {kind: 'opfs', name: 'my-app-cache'},
  source: {
    kind: 'supabase',
    url: import.meta.env.VITE_SUPABASE_URL,
    publishableKey: import.meta.env.VITE_SUPABASE_PUBLISHABLE_KEY,
    tables: [
      {
        // `schema` defaults to `public`.
        table: 'posts',
        primaryKey: ['id'],
        columns: ['id', 'title', 'published'],
      },
    ],
  },
});

// Local readiness never waits for the network. An OPFS cache is queryable here;
// a new cache has the configured schemas and no rows yet.
await db.ready();
const cached = await db.query('SELECT * FROM posts');

const unsubscribe = db.subscribeToSyncState((state) => {
  console.log('TinyGres sync state', state.phase);
});

// Wait through transient reconnects until a complete remote baseline is live.
await db.whenSynced({timeoutMs: 30_000});
const reconciled = await db.query('SELECT * FROM posts');

unsubscribe();
await db.close();
```

`getSyncState()` returns the latest state, and `subscribeToSyncState()`
immediately emits that same snapshot before reporting later transitions.
`whenSynced()` resolves only for `live-best-effort` or a future durable-live
state; it rejects terminal configuration/permission errors, cancellation,
timeout, client closure, and calls made without a source.

The built-in configuration deliberately supports one invariant anonymous
visibility scope through a browser-safe Supabase publishable key. Each selected
table must be entirely readable by the anonymous role through the Data API—for
example, with a table-wide `USING (true)` policy—and included in Supabase's
Realtime publication. Row-dependent anonymous RLS, policy changes, and
visibility based on JWT claims are not supported: a browser cache cannot infer
that a previously visible row has become hidden. Enable `REPLICA IDENTITY FULL`
for synchronized tables so UPDATE and DELETE events contain enough old row
identity to repair primary-key changes safely. Never put a secret or
service-role key in browser code; TinyGres rejects those recognizable key
forms.

Authenticated sessions, token refresh, and changing per-user RLS visibility
need an explicit auth-generation and cache-transition contract and are not yet
supported by the built-in source. For advanced experiments, an
application-owned worker can still inject the official Supabase client:

```ts
// tinygres.worker.ts
import {createClient} from '@supabase/supabase-js';
import {
  createSupabaseJsRealtimeTransport,
  createSupabaseSource,
} from 'tinygres/supabase';
import {startWorker} from 'tinygres/worker';

const url = import.meta.env.VITE_SUPABASE_URL;
const publishableKey = import.meta.env.VITE_SUPABASE_PUBLISHABLE_KEY;
const supabase = createClient(url, publishableKey, {
  auth: {
    autoRefreshToken: false,
    detectSessionInUrl: false,
    persistSession: false,
  },
});

startWorker({
  source: createSupabaseSource({
    url,
    publishableKey,
    realtime: createSupabaseJsRealtimeTransport(supabase),
    tables: [
      {
        schema: 'public',
        table: 'posts',
        primaryKey: ['id'],
        columns: ['id', 'title', 'published'],
      },
    ],
  }),
});
```

```ts
// app.ts
const db = createClient({
  worker: new Worker(new URL('./tinygres.worker.ts', import.meta.url), {
    name: 'tinygres',
    type: 'module',
  }),
});
await db.ready();
```

Install `@supabase/supabase-js` in the application when using this helper;
TinyGres deliberately does not bundle it or add it to the core runtime. The
example above also covers anonymous/public-key access. Adapter functions remain
inside the worker and are never passed through `postMessage`.

Supabase Realtime does not provide a durable client cursor or transaction
boundaries. TinyGres therefore reports this source as `live-best-effort`, marks
it stale after a disconnect or untrusted payload, quarantines incremental
changes until integrity is restored, and replaces complete table snapshots
before reporting it live again. The REST snapshot is not transactionally
aligned with the Realtime stream, so repeated changes during a snapshot trigger
another bounded pass. This is an offline-readable cache with reconciliation,
not logical replication or an upstream write path.

See Supabase's documentation for [Postgres Changes setup](https://supabase.com/docs/guides/realtime/postgres-changes),
the [Realtime wire protocol](https://supabase.com/docs/guides/realtime/protocol),
and [browser-safe API keys](https://supabase.com/docs/guides/getting-started/api-keys).

## Supabase-style builder

The initial builder intentionally exposes only the implemented surface:

```ts
const {data, error} = await db
  .from<Post>('posts')
  .select('id, title, published')
  .eq('published', true)
  .gte('priority', 2)
  .order('id', {ascending: false})
  .range(0, 19);
```

This syntax queries the local replica. It does not make a PostgREST request.
Values are represented as JSON-compatible values.

## Current SQL compatibility

TinyGres currently accepts one statement at a time. `SELECT` supports:

- one unqualified or schema-qualified table;
- `*` or a list of simple column names;
- `=`, `<>`/`!=`, `<`, `<=`, `>`, and `>=` comparisons;
- `AND`, `OR`, `NOT`, parentheses, `IN`/`NOT IN`, and `IS [NOT] NULL`
  with SQL three-valued null logic;
- string, number, boolean, `NULL`, or PostgreSQL-style `$1` parameters; and
- simple multi-column `ORDER BY` with `ASC`/`DESC` and
  `NULLS FIRST`/`NULLS LAST`;
- optional non-negative `LIMIT` and `OFFSET`.

Standalone writable databases additionally support:

- `CREATE TABLE` and `CREATE TABLE IF NOT EXISTS` with a required inline or
  table-level primary key;
- column types `BOOLEAN`, `SMALLINT`/`INTEGER`/`BIGINT`, `REAL`/`DOUBLE
  PRECISION`, `TEXT`/`VARCHAR`, and `JSON`/`JSONB`;
- literal defaults, `NULL`/`NOT NULL`, and composite primary keys;
- `CREATE [UNIQUE] INDEX [IF NOT EXISTS]` over boolean, integer, and text
  columns, including composite indexes;
- multi-row `INSERT ... VALUES`, filtered `UPDATE`, and filtered `DELETE`;
- `$1` parameters, `DEFAULT`, and simple `RETURNING *`/column lists; and
- callback transactions through `db.transaction()`.

Complete primary-key equality uses direct row lookup. A complete equality match
for every column of a secondary index uses its maintained postings; partial
composite matches, ranges, `OR`, and `NOT` currently scan the table. Unique
indexes follow PostgreSQL's default behavior of allowing multiple keys that
contain `NULL`. Text comparison and ordering use deterministic Unicode code-point
ordering rather than PostgreSQL database collations.

Integers are restricted to JavaScript's exactly representable safe-integer
range. Type names are compatibility spellings over this smaller runtime type
set: for example, `BIGINT` does not provide 64-bit values and `JSONB` currently
uses JSON-compatible structured values. `NULL = NULL` does not match, following
SQL null semantics.

Joins, aliases, grouping, aggregates, subqueries, general
expressions, foreign keys,
`ON CONFLICT`, sequences/generated IDs, type modifiers, `ALTER`/`DROP`, and SQL
`BEGIN` tokens are rejected with an `UNSUPPORTED_SQL` or schema error. This is
an explicit compatibility boundary, not an accidental promise of full
PostgreSQL behavior.

## Development and validation

```sh
npm run typecheck       # authored TypeScript source and unit tests
npm run test:ts         # client, RPC, and worker-host unit tests
npm run test:rust       # native Rust engine tests
npm run build           # assemble the complete publishable dist package
npm run test:browser    # real browser Worker/WASM flow
npm run test:package    # pack dist and install it in a clean Vite app
npm run check:size      # hard 700 KiB uncompressed WASM gate
```

The browser tests cover the complete Phase-1 path—initialize the real module
Worker, query, apply a fake remote change, receive an invalidation, and
re-query—plus the persistence path through a real dedicated Worker and OPFS
restart. The writable proof creates a typed table, inserts and updates inside a
transaction, verifies rollback after a constraint failure, closes the Worker,
and reopens the committed state. The Supabase suite adds a credential-free
protocol server that exercises the
actual default Worker, PostgREST snapshot, Phoenix join/change/reconnect flow,
and source-bound OPFS recovery.

The packed-package test separately proves SSR-safe import, declarations, a
production Vite build, and real browser execution through both the packaged
default worker and an application-owned worker. It also starts the built-in
Supabase source through both worker modes without installing a Supabase SDK. It
installs the tarball rather than resolving TinyGres through a workspace link.

The current feasibility target is an uncompressed WASM binary smaller than 700
KiB. The size check is intentionally independent of gzip size so it cannot hide
startup and compilation cost.

## Direction

The immediate direction is a useful small local database: schema migrations and
bounded aggregates, followed by prepared commits and paged persistence. Joins
will follow only after qualified column references can be added without making
ambiguous row semantics part of the public contract.
Full PostgreSQL catalogs, extensions, server
concurrency, and arbitrary wire compatibility are not goals. The existing
adapter boundary remains available for optional remote read sources and a later
cursor-aligned gateway without defining the core product around sync.

TinyGres is MIT licensed.
