# TinyGres

<section id="hero">
  <h2>
    A tiny, worker-first <em>relational database</em> for browser apps.
  </h2>
  <p>
    PostgreSQL-shaped SQL, running locally and away from the main thread.
  </p>
</section>

<a class="start" href="https://tinygres.org/guides/getting-started/">Get started</a>

<a href="https://tinygres.org/demos/">Try the demos</a>

<a href="https://tinygres.org/api/">Read the API</a>

---

TinyGres gives an application a PostgreSQL-shaped SQL database without a
database server. The engine runs away from the main thread in a dedicated
Worker, and a persistent database can live in the browser's Origin Private File
System.

> **Important:** TinyGres is experimental. It implements a deliberately bounded SQL and type
> subset; it is not PostgreSQL compiled to WebAssembly and has no PostgreSQL
> server, wire protocol, or replication client.

## Start small

```sh
npm install tinygres
```

```ts
import {create} from 'tinygres';

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
  'Try TinyGres',
]);

const {rows} = await db.query<{
  id: string;
  title: string;
  done: boolean;
}>('SELECT * FROM tasks ORDER BY id');

await db.close();
```

That is the complete infrastructure setup. TinyGres constructs its packaged
Worker and loads its WebAssembly engine when create() runs. Rust, Worker
entry files, and WASM configuration are not part of ordinary application code.

Use create() with no argument for an ephemeral memory database. Use a stable
`opfs://name` when data should survive reloads in the same browser.

## A familiar, intentionally small API

- query() runs one parameterized read or write statement.
- exec() runs a parameter-free SQL script atomically.
- transaction() commits related row mutations together.
- prepare() retains a statement for repeated execution.
- subscribe() reports which tables changed so an app can re-query.
- close() releases statements, storage, and the Worker.

Results use the familiar `rows`, `fields`, `affectedRows`, `command`, and
`rowCount` shape. TinyGres also reports a database `revision` and changed
`tables`.

## Local by default

TinyGres contains no hosted service, credentials, analytics, or hidden network
path. Memory and OPFS use the same page-native database engine. Persistent OPFS
storage is single-writer and intended for reconstructable application data;
users can still clear or lose browser-managed storage.

## Go deeper when you need to

- Follow the [getting started guide](https://tinygres.org/guides/getting-started/).
- Check the exact [SQL compatibility contract](https://tinygres.org/guides/sql-compatibility/).
- Browse the [API reference](https://tinygres.org/api/).
- Review the [release notes](https://tinygres.org/guides/releases/).
- Start an app with [create-tinygres](https://github.com/tinyplex/create-tinygres).
- Read the [source](https://github.com/tinyplex/tinygres).

TinyGres is MIT licensed.
