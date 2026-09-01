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

## Open a database

```ts
import {create} from 'tinyjoin';

const db = await create('opfs://my-app');
```

The Promise resolves after the Worker and database are ready. The string gives
the persistent database a stable local name. Call create() without an
argument while experimenting with an ephemeral database.

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
  () => void db.close(),
  {once: true},
);
```

Closing releases prepared statements, storage, and the Worker. It is
asynchronous and safe to call more than once.

The [Todo starter demo](/demos/todo-starter/) puts these calls together in a
small browser application.
