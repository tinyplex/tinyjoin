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

> ## Your first _TinyJoin_ app
>
> Scaffold a complete local todo app - in TypeScript or JavaScript, with its
> data saved across reloads! - in less than 60s. It needs no database server,
> account, or credentials.

```bash
> npm create tinyjoin@latest

🎉 Welcome to TinyJoin!

📦 Creating your project...
```

> ## Start small
>
> Install TinyJoin. There is no server to run, no account to create, and no
> native toolchain to set up.

```sh
npm install tinyjoin
```

> ## Open a database
>
> create() owns Worker construction and WebAssembly loading, and resolves once
> the database is ready. Use a stable `opfs://name` when data should survive
> reloads in the same browser, or call it with no argument for an ephemeral
> memory database.

```ts
import {create} from 'tinyjoin';

const db = await create('opfs://my-app');
```

> ## Set up a schema
>
> exec() runs a parameter-free script as one implicit transaction, so schema
> setup stays a single call that is safe to run again on every load.

```ts
await db.exec(`
  CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  )
`);
```

> ## Write with parameters
>
> query() runs one read or write statement. Application values go in the `$n`
> array and never reach the SQL text.

```ts
const id = crypto.randomUUID();

await db.query(
  'INSERT INTO tasks (id, title) VALUES ($1, $2)',
  [id, 'Try TinyJoin'],
);
```

> ## Or tag a template
>
> The sql tagged template is the same parameterized call in a shorter form. It
> takes values only, so there is no way to interpolate raw SQL by accident.

```ts
const title = 'Written with a tag';

await db.sql`
  INSERT INTO tasks (id, title)
  VALUES (${crypto.randomUUID()}, ${title})
`;
```

> ## Read rows back
>
> Results use the familiar `rows`, `fields`, `affectedRows`, `command`, and
> `rowCount` shape. The row type is yours to declare, and TinyJoin adds a
> database `revision` and the `tables` a statement touched.

```ts
type Task = {id: string; title: string};

const {rows} = await db.query<Task>(
  'SELECT id, title FROM tasks ORDER BY title',
);
```

> ## Commit related changes together
>
> transaction() stages its writes and publishes them once. Reads inside the
> callback see the staged rows, and letting an error escape rolls the whole
> thing back.

```ts
await db.transaction(async (tx) => {
  await tx.query(
    'UPDATE tasks SET done = $1 WHERE id = $2',
    [true, id],
  );
  await tx.query(
    'DELETE FROM tasks WHERE done = $1',
    [true],
  );
});
```

> ## Re-run without re-parsing
>
> prepare() retains one parsed statement in the Worker. It resolves tables and
> types against the current catalog on every execution, so a compatible schema
> change does not make the handle stale.

```ts
const openTasks = await db.prepare<{
  id: string;
  title: string;
}>('SELECT id, title FROM tasks WHERE done = $1');

const {rows} = await openTasks.execute([false]);
```

> ## Let the view follow the data
>
> subscribe() reports which tables changed, so a UI can re-query instead of
> being told what to redraw by every writer. close() then releases statements,
> storage, and the Worker.

```ts
const unsubscribe = db.subscribe(
  {tables: ['tasks']},
  () => render(),
);

// Later
unsubscribe();
await db.close();
```

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
