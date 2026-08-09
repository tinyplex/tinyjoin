# Tinygres

Tinygres is an experimental, worker-first local query cache for PostgreSQL
data. Its query and storage engine is written in Rust, compiled to WebAssembly,
and kept off the browser's main thread.

> [!IMPORTANT]
> Tinygres is an early read-only prototype. It does not persist data, accept
> application writes, or provide complete PostgreSQL SQL compatibility. Its
> Supabase adapter is best-effort and reconciles after reconnects; it is not a
> durable logical-replication stream.

The first proof of concept deliberately does a small number of things:

- owns an in-memory replica inside a dedicated Web Worker;
- evaluates a documented subset of PostgreSQL `SELECT` in Rust/WASM;
- applies normalized snapshot and server-change batches atomically;
- emits table-level invalidations so an application can re-query; and
- keeps source adapters behind the worker boundary for future Supabase and
  PostgreSQL replication transports.

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
version and `wasm32-unknown-unknown` target, while the npm development dependency
provides `wasm-pack`. The build stages wasm-bindgen output in a temporary
directory and copies only the runtime JavaScript and `.wasm` files into
`dist/wasm`, so generated package metadata never appears under `src/`. Cargo's
compiler cache lives under `node_modules/.cache/tinygres` when using the project
scripts rather than creating a top-level `target/` directory. Use
`npm run cargo -- <arguments>` for other Cargo commands with the same behavior.

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
`rustc` and `cargo` take precedence. The build command checks for the WASM target
and prints this setup guidance before invoking `wasm-pack`.

## Browser API

```ts
import {createTinygresClient} from 'tinygres';

type Post = {
  id: number;
  title: string;
  published: boolean;
};

const db = createTinygresClient({
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

// Later, a read-only source adapter will call this for a server change.
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

`createTinygresClient` creates a dedicated module worker by default. It is safe
to import during server rendering; the worker is only constructed when the
function is called in a browser.

For an application-owned worker, pass `worker`, `workerFactory`, or `workerUrl`:

```ts
const db = createTinygresClient({
  workerFactory: () =>
    new Worker(new URL('./tinygres.worker.ts', import.meta.url), {
      name: 'tinygres',
      type: 'module',
    }),
  schemas: [{name: 'posts', primaryKey: ['id']}],
});
```

The worker entry can install a future source adapter without moving network or
replication work onto the UI thread:

```ts
// tinygres.worker.ts
import {startTinygresWorker} from 'tinygres/worker';

startTinygresWorker({source: myReadOnlyReplicaSource});
```

Adapter functions live in the worker and normalize their input into table
snapshots and change batches. They are not serialized through `postMessage`.

## Supabase adapter

The first source adapter snapshots explicitly selected tables through the
Supabase Data API, then uses Supabase Realtime to accelerate changes. Put the
adapter and its Supabase client inside an application-owned worker:

```ts
// tinygres.worker.ts
import {createClient} from '@supabase/supabase-js';
import {
  createSupabaseJsRealtimeTransport,
  createSupabaseSource,
} from 'tinygres/supabase';
import {startTinygresWorker} from 'tinygres/worker';

const url = import.meta.env.VITE_SUPABASE_URL;
const publishableKey = import.meta.env.VITE_SUPABASE_PUBLISHABLE_KEY;
const supabase = createClient(url, publishableKey, {
  auth: {
    autoRefreshToken: false,
    detectSessionInUrl: false,
    persistSession: false,
  },
});

startTinygresWorker({
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
const db = createTinygresClient({
  worker: new Worker(new URL('./tinygres.worker.ts', import.meta.url), {
    name: 'tinygres',
    type: 'module',
  }),
});
await db.ready();
```

Install `@supabase/supabase-js` in the application when using this helper;
Tinygres deliberately does not bundle it or add it to the core runtime. The
example above covers anonymous/public-key access. Passing authenticated sessions
from the main thread into the worker, including safe token refresh and cache
namespacing, is a planned integration point rather than a supported contract
yet. Never put a Supabase secret or service-role key in browser code.

Supabase Realtime does not provide a durable client cursor or transaction
boundaries. Tinygres therefore reports this source as `live-best-effort`, marks
it stale after a disconnect or malformed payload, and replaces affected local
snapshots before reporting it live again.

## Supabase-style builder

The initial builder intentionally exposes only the implemented surface:

```ts
const {data, error} = await db
  .from<Post>('posts')
  .select('id, title, published')
  .eq('published', true)
  .limit(20);
```

This syntax queries the local replica. It does not make a PostgREST request.
Values are represented as JSON-compatible values.

## Current SQL compatibility

Tinygres currently accepts one read-only `SELECT` statement containing:

- one unqualified or schema-qualified table;
- `*` or a list of simple column names;
- equality filters joined with `AND`;
- string, number, boolean, `NULL`, or PostgreSQL-style `$1` parameters; and
- an optional non-negative `LIMIT`.

Joins, aliases, ordering, grouping, aggregates, subqueries, expressions, and
write statements are rejected with an `UNSUPPORTED_SQL` error. `NULL = NULL`
does not match, following SQL null semantics.

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

The browser test covers the complete Phase-1 path: initialize the real module
worker, load the WASM engine, query its snapshot, apply a fake remote change,
receive an invalidation, and re-query the new row.

The packed-package test separately proves SSR-safe import, declarations, a
production Vite build, and real browser execution through both the packaged
default worker and an application-owned worker. It installs the tarball rather
than resolving Tinygres through a workspace link.

The current feasibility target is an uncompressed WASM binary smaller than
700 KiB. The size check is intentionally independent of gzip size so it cannot
hide startup and compilation cost.

## Direction

The adapter boundary is intended to support a later cursor-aligned PostgreSQL
replication gateway without changing the local query API. The next storage phase
will add OPFS-backed persistence and crash recovery while keeping the current
in-memory driver for tests and ephemeral use.

Tinygres is MIT licensed.
