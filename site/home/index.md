# TinyJoin

<section id="hero">
  <h2>
    A tiny, worker-first <em>relational database</em> for browser apps.
  </h2>
  <p>
    PostgreSQL-shaped SQL, running locally and away from the main thread.
  </p>
</section>

<nav id="actions" aria-label="Get started">
  <a class="start" href="/guides/getting-started/">Get started</a>
  <a href="/demos/">Try the demos</a>
  <a href="/api/">Read the API</a>
</nav>

---

> ## Start small
>
> Install TinyJoin, import create(), and open a database. The normal setup does
> not need Rust tooling, a Worker entry, a WASM plugin, or a runtime copying
> step.

```sh
npm install tinyjoin
```

> ## One import. No infrastructure setup.
>
> create() owns Worker construction and WebAssembly loading. Use a stable
> `opfs://name` when data should survive reloads in the same browser, or call
> create() with no argument for an ephemeral memory database.

```ts
import {create} from 'tinyjoin';

const db = await create('opfs://my-app');

await db.exec(`
  CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  )
`);

await db.query('INSERT INTO tasks (id, title) VALUES ($1, $2)', [
  crypto.randomUUID(),
  'Try TinyJoin',
]);

const {rows} = await db.query<{
  id: string;
  title: string;
  done: boolean;
}>('SELECT * FROM tasks ORDER BY id');

await db.close();
```

> ## A familiar, intentionally small API
>
> - query() runs one parameterized read or write statement.
> - exec() runs a parameter-free SQL script atomically.
> - transaction() commits related row mutations together.
> - prepare() retains a statement for repeated execution.
> - subscribe() reports which tables changed so an app can re-query.
> - close() releases statements, storage, and the Worker.
>
> Results use the familiar `rows`, `fields`, `affectedRows`, `command`, and
> `rowCount` shape. TinyJoin also reports a database `revision` and changed
> `tables`.

> ## Local by default
>
> TinyJoin contains no hosted service, credentials, analytics, or hidden network
> path. Memory and OPFS use the same page-native database engine. Persistent
> OPFS storage is single-writer and intended for reconstructable application
> data; users can still clear or lose browser-managed storage.

> ## Deliberately bounded
>
> **Important:** TinyJoin is experimental. It implements a deliberately bounded
> SQL and type subset; it is not PostgreSQL compiled to WebAssembly and has no
> PostgreSQL server, wire protocol, or replication client.
>
> Check the exact [SQL compatibility
> contract](/guides/sql-compatibility/) before relying on
> unlisted PostgreSQL syntax or types.

> ## Go deeper when you need to
>
> - Follow the [getting started guide](/guides/getting-started/).
> - Browse the [API reference](/api/).
> - Review the [release notes](/guides/releases/).
> - Start an app with [create-tinyjoin](https://github.com/tinyplex/create-tinyjoin).
> - Read the [source](https://github.com/tinyplex/tinyjoin).
>
> TinyJoin is MIT licensed.
