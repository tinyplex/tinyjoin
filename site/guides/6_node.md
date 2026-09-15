# Node

Use `tinyjoin/node` to run an in-memory TinyJoin database in Node.js 22 or
later. It uses a dedicated Worker thread and the same WebAssembly engine and
Client API as the browser runtime.

## Get started

Install TinyJoin:

```sh
npm install tinyjoin
```

Save this ESM example as `main.mjs`:

```js
import {create as createNode} from 'tinyjoin/node';

const db = await createNode();
try {
  await db.exec(`
    CREATE TABLE notes (
      id INTEGER PRIMARY KEY,
      body TEXT NOT NULL
    )
  `);
  await db.query('INSERT INTO notes (id, body) VALUES ($1, $2)', [
    1,
    'Hello from Node',
  ]);
  const {rows} = await db.query('SELECT * FROM notes ORDER BY id');
  console.log(rows);
} finally {
  await db.close();
}
```

Run it with `node main.mjs`. No extra dependencies, bundler, Worker entry, or
polyfills are needed. The entry point constructs the Worker and loads the
packaged WebAssembly file from disk automatically.

## API and lifecycle

The Node entry point exports
[`create()`](/api/node/functions/lifecycle/create/). Call it without arguments,
or with `memory://`, to create a fresh database. The Promise resolves when the
Worker and engine are ready.

The returned Client supports the same parameterized queries, transactions,
prepared statements, subscriptions, and TypeScript row generics as the browser
Client. The [SQL compatibility rules](/guides/sql-compatibility/) and
[transaction behavior](/guides/transactions-and-changes/) also apply. The Node
entry point also exports ClientError and the shared Client API types, including
row, result, query-option, transaction, prepared-statement, and subscription
types:

```ts
import {type Client, type Row, create, ClientError} from 'tinyjoin/node';
```

ClientError is the same error class exported by the browser entry point.

Each call owns one Worker thread and an independent database. Always await
db.close() when finished, including error paths, so the Worker is released and
the Node process can exit. A closed Client cannot be reused.

## Storage boundaries

This entry point is experimental and supports memory only. Data is lost when
the Client closes or the process exits. Creating another Client starts with an
empty database, including when both calls use `memory://`.

There is no OPFS or filesystem persistence, shared database between Clients,
or remote synchronization. The Node entry point does not accept the browser
create() options or custom Worker configuration. Use the browser entry point
for [persistent browser storage](/guides/storage-and-lifecycle/).
