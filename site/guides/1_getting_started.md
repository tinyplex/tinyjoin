# Getting started

TinyJoin is a relational database that runs locally in a browser. The normal
setup is one import and one asynchronous call.

## Install

```sh
npm install tinyjoin
```

TinyJoin publishes JavaScript, TypeScript declarations, its Worker runtime, and
the WebAssembly engine together. An application does not need Rust tooling or a
separate Worker plugin.

For a new application, `npm create tinyjoin@latest` generates a working Vite
starter. Its production build includes [offline loading](/guides/offline/).

For a complete noninteractive setup, supply every starter option:

```sh
npm create tinyjoin@latest -- --non-interactive \
  --projectName my-tinyjoin-app \
  --language typescript \
  --storage opfs \
  --installAndRun false
cd my-tinyjoin-app
npm install
npm run dev
```

Use `--language javascript` for JavaScript, or `--storage memory` for data that
starts fresh on each load. The starter's `--list-options` flag lists its options.

## Open a database

TinyJoin is experimental and verified on Chromium only. Browser storage can
be lost, so start with data you can reconstruct. Read the
[caveats](/guides/caveats/) before adopting persistent storage.

```ts
import {create} from 'tinyjoin';

const db = await create('opfs://my-app');
```

The Promise resolves after the Worker and database are ready. The string gives
the persistent database a stable local name. Call create() without an
argument while experimenting with an ephemeral database.

Use the same name in every tab. TinyJoin automatically shares one database
owner and delivers committed-change notifications across those Clients. See
[storage and tab handover](/guides/storage-and-lifecycle/) for lifecycle details.

## Create a table

Schema setup can be idempotent, so the same application startup works for a new
or existing database:

```ts
await db.exec(`
  CREATE TABLE IF NOT EXISTS notes (
    id TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    pinned BOOLEAN NOT NULL DEFAULT false
  )
`);
```

exec() accepts a parameter-free script and commits all of it together. Use it
for schema setup. TinyJoin requires every SQL-created table to have a primary
key.

## Write with parameters

```ts
await db.query('INSERT INTO notes (id, body) VALUES ($1, $2)', [
  crypto.randomUUID(),
  'Hello from the browser',
]);
```

Keep application values in the parameter array. TinyJoin parameters are
one-based (`$1`, `$2`, and so on) and accept JSON-compatible values.

## Read typed rows

```ts
type Note = {
  id: string;
  body: string;
  pinned: boolean;
};

const {rows} = await db.query<Note>(
  'SELECT id, body, pinned FROM notes ORDER BY id',
);
```

The generic describes the expected result to TypeScript; it does not validate
rows at runtime. Use an explicit projection when callers require a stable
shape.

## Close cleanly

```ts
window.addEventListener(
  'pagehide',
  () => void db.close().catch(console.error),
  {once: true},
);
window.addEventListener('pageshow', (event) => {
  if (event.persisted) {
    window.location.reload();
  }
});
```

Closing releases this Client's prepared statements and Worker. Other Clients
for the same name continue, with automatic owner handover when needed. Closing
is asynchronous and safe to call more than once.

Closing cannot be undone. This example reloads a page restored from the
back/forward cache so startup creates a fresh Client. A browser does not await
pagehide cleanup; finish and await writes during normal use. Applications
with their own lifecycle can reopen and recreate their database state instead
of reloading. See [storage and lifecycle](/guides/storage-and-lifecycle/).

The [Todo starter demo](/demos/todo-starter/) puts these calls together in a
small browser application.
