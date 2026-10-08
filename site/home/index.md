# TinyJoin

<section id="hero">
  <h2>
    {{benchmarks.epithet}} <em>relational database</em> for your web app.
  </h2>
  <p>
    {{benchmarks.claim}}. Written in Rust, 
    supporting PostgreSQL-shaped SQL, and running locally, away from the 
    browser's main UI thread.
  </p>
</section>

<nav id="actions" aria-label="Get started">
  <a class="start" href="/guides/getting-started/">Get started</a>
  <a href="/demos/">Try the demos</a>
  <a href="/api/">Read the API</a>
</nav>

---


> ## Small enough not to notice
>
> TinyJoin is only {{sizes.total.gzip}} to download, compressed, smaller than
> SQLite ({{benchmarks.sqlite-download}}) and way smaller than PGlite
> ({{benchmarks.pglite-download}}).
> 
> Of that, only {{sizes.client.gzip}} runs on the UI thread!

| Component      |                     gzip |
| -------------- | -----------------------: |
| Main JS        |    {{sizes.client.gzip}} |
| Worker JS      |    {{sizes.worker.gzip}} |
| Engine WASM    |      {{sizes.wasm.gzip}} |
| **Everything** | **{{sizes.total.gzip}}** |

> ## {{benchmarks.heading}}
>
> We test each engine running the same SQL in a browser Worker, with its
> database on OPFS. {{benchmarks.tally}}, from opening a database to reading,
> writing, joining, and committing.
> 
> These charts show the median of 9 runs, fastest first. Brackets span the
> fastest to the slowest run. The Benchmarks guide has every result in detail,
> and how to run them yourself. 

{{benchmarks.highlights}}

> ## Your first _TinyJoin_ app
>
> Scaffold a complete local todo app in JS or TS - and with its relational data
> saved in TinyJoin across reloads - in less than 60s. Write its queries in SQL,
> or with the Drizzle ORM or the Kysely query builder.

```bash
> npm create tinyjoin@latest

🎉 Welcome to TinyJoin!

✔ Project name: … my-tinyjoin-app
✔ Language: › TypeScript
✔ Queries: › Drizzle ORM
✔ Todo data: › Save data across reloads (recommended)
✔ Install dependencies and start the app? … yes

📦 Creating your project...
```

> ## Or add it to an existing app
>
> Installing TinyJoin is simple. There are no runtime dependencies, no servers
> to run, no accounts to create, and no native toolchains.

```sh
npm install tinyjoin
```

> ## Open a database
>
> create() owns Worker construction and WebAssembly loading, and resolves once
> the database is ready. Use `opfs://[name]` when data should survive reloads in
> the same browser, or call it with no argument for ephemeral in-memory storage.

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

> ## Or bring a query builder
>
> The Drizzle ORM and the Kysely query builder run on the same Client. push()
> makes the database hold a Drizzle schema as the app starts, in one atomic
> change that keeps every row. And `npm create tinyjoin@latest` can start a new
> app with either.

```ts
import {drizzle, push} from 'tinyjoin/drizzle';
import {eq} from 'drizzle-orm';
import * as schema from './schema';

const orm = drizzle(db, {schema});
await push(orm, schema);

const open = await orm
  .select()
  .from(schema.tasks)
  .where(eq(schema.tasks.done, false));
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

> ## Go deeper when you need to
>
> - Follow the [getting started guide](/guides/getting-started/).
> - Browse the [API reference](/api/).
> - Understand the [caveats](/guides/caveats/).
> - Review the [release notes](/guides/releases/).
> - Start an app with
>   [create-tinyjoin](https://github.com/tinyplex/create-tinyjoin).
> - Read the [source](https://github.com/tinyplex/tinyjoin).

> ## Local by design
>
> TinyJoin keeps your database in the browser. Your app reads and writes data
> locally, without waiting for a database server. Keep data in memory or save it
> across reloads with OPFS; tabs opening the same persistent database share
> access automatically.
>
> Your app can keep working when the network drops. Apps created with the
> starter can also [reopen offline](/guides/offline/) once their production
> build has been cached.

<section id="warning" aria-labelledby="an-important-warning">

## An important warning

TinyJoin is experimental and verified on Chromium only, with a bounded SQL
dialect and no built-in remote synchronization (yet!). Browser storage can be
lost, so use it for data you can reconstruct. Read the
[caveats](/guides/caveats/) and [SQL compatibility
guide](/guides/sql-compatibility/) to check whether this project currently fits
your app. We're working on it though, so keep checking back.

</section>

---

<section id="family">
  <h2>Meet the family</h2>
  <p>
    TinyJoin is one of a group of small libraries that make rich client and
    local-first apps easier to build. Take a look at the others!
  </p>
  <ul>
    <li>
      <a href="https://tinybase.org/">
        <img src="https://tinybase.org/favicon.svg?asImg" alt="" width="40" height="40" />
        <b>TinyBase</b>
      </a>
      A reactive data store with persistence and synchronization.
    </li>
    <li>
      <a href="https://synclets.org/">
        <img src="https://synclets.org/favicon.svg?asImg" alt="" width="40" height="40" />
        <b>Synclets</b>
      </a>
      An open, storage-agnostic sync engine development kit.
    </li>
    <li>
      <a href="https://tinywidgets.org/">
        <img src="https://tinywidgets.org/favicon.svg?asImg" alt="" width="40" height="40" />
        <b>TinyWidgets</b>
      </a>
      A collection of tiny, reusable UI components.
    </li>
    <li>
      <a href="https://tinytick.org/">
        <img src="https://tinytick.org/favicon.svg?asImg" alt="" width="40" height="40" />
        <b>TinyTick</b>
      </a>
      A tiny but very useful task orchestrator.
    </li>
  </ul>
</section>
