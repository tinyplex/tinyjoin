<link rel="preload" as="image" href="https://tinybase.org/favicon.svg?asImg"><link rel="preload" as="image" href="https://synclets.org/favicon.svg?asImg"><link rel="preload" as="image" href="https://tinywidgets.org/favicon.svg?asImg"><link rel="preload" as="image" href="https://tinytick.org/favicon.svg?asImg"><section id="hero"><h2 id="a-tiny-worker-first-relational-database-for-browser-apps">A tiny, worker-first <em>relational database</em> for browser apps.</h2><p>PostgreSQL-shaped SQL, running locally and away from the main thread.</p></section><nav id="actions" aria-label="Get started"><a class="start" href="https://tinyjoin.org/guides/getting-started/">Get started</a> <a href="https://tinyjoin.org/demos/">Try the demos</a> <a href="https://tinyjoin.org/api/">Read the API</a></nav><hr><section><h2 id="small-enough-to-not-worry-about">Small enough to not worry about</h2><p>The whole database - the main-thread client, the Worker host, and the Rust WASM engine - is 299 KiB gzipped, and only 6 KiB of that ever runs on the UI thread.</p></section><div class="table"><table><thead><tr><th>Component</th><th style="text-align:right">gzip</th></tr></thead><tbody><tr><td>Main JS</td><td style="text-align:right">6 KiB</td></tr><tr><td>Worker JS</td><td style="text-align:right">15 KiB</td></tr><tr><td>Engine WASM</td><td style="text-align:right">279 KiB</td></tr><tr><td><strong>Everything</strong></td><td style="text-align:right"><strong>299 KiB</strong></td></tr></tbody></table></div><section><h2 id="your-first-tinyjoin-app">Your first <em>TinyJoin</em> app</h2><p>Scaffold a complete local todo app in JS or TS - and with its relational data saved in TinyJoin across reloads - in less than 60s.</p></section>

```bash
> npm create tinyjoin@latest

🎉 Welcome to TinyJoin!

📦 Creating your project...
```

<section><h2 id="start-small">Start small</h2><p>Install TinyJoin. There are no runtime dependencies, no servers to run, no accounts to create, and no native toolchains</p></section>

```sh
npm install tinyjoin
```

<section><h2 id="open-a-database">Open a database</h2><p><a href="https://tinyjoin.org/api/the-essentials/using-a-database/create/"><code>create</code></a>() owns Worker construction and WebAssembly loading, and resolves once the database is ready. Use <code>opfs://[name]</code> when data should survive reloads in the same browser, or call it with no argument for ephemeral in-memory storage.</p></section>

```ts
import {create} from 'tinyjoin';

const db = await create('opfs://my-app');
```

<section><h2 id="set-up-a-schema">Set up a schema</h2><p><a href="https://tinyjoin.org/api/the-essentials/using-a-database/exec/"><code>exec</code></a>() runs a parameter-free script as one implicit transaction, so schema setup stays a single call that is safe to run again on every load.</p></section>

```ts
await db.exec(`
  CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  )
`);
```

<section><h2 id="write-with-parameters">Write with parameters</h2><p><a href="https://tinyjoin.org/api/the-essentials/using-a-database/query/"><code>query</code></a>() runs one read or write statement. Application values go in the <code>$n</code> array and never reach the SQL text.</p></section>

```ts
const id = crypto.randomUUID();

await db.query(
  'INSERT INTO tasks (id, title) VALUES ($1, $2)',
  [id, 'Try TinyJoin'],
);
```

<section><h2 id="or-tag-a-template">Or tag a template</h2><p>The sql tagged template is the same parameterized call in a shorter form. It takes values only, so there is no way to interpolate raw SQL by accident.</p></section>

```ts
const title = 'Written with a tag';

await db.sql`
  INSERT INTO tasks (id, title)
  VALUES (${crypto.randomUUID()}, ${title})
`;
```

<section><h2 id="read-rows-back">Read rows back</h2><p><a href="https://tinyjoin.org/api/tinyjoin/interfaces/query-results/results/"><code>Results</code></a> use the familiar <code>rows</code>, <code>fields</code>, <code>affectedRows</code>, <code>command</code>, and <code>rowCount</code> shape. The row type is yours to declare, and TinyJoin adds a database <code>revision</code> and the <code>tables</code> a statement touched.</p></section>

```ts
type Task = {id: string; title: string};

const {rows} = await db.query<Task>(
  'SELECT id, title FROM tasks ORDER BY title',
);
```

<section><h2 id="commit-related-changes-together">Commit related changes together</h2><p><a href="https://tinyjoin.org/api/the-essentials/using-a-database/transaction/"><code>transaction</code></a>() stages its writes and publishes them once. Reads inside the callback see the staged rows, and letting an error escape rolls the whole thing back.</p></section>

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

<section><h2 id="re-run-without-re-parsing">Re-run without re-parsing</h2><p><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/sql/prepare/"><code>prepare</code></a>() retains one parsed statement in the Worker. It resolves tables and types against the current catalog on every execution, so a compatible schema change does not make the handle stale.</p></section>

```ts
const openTasks = await db.prepare<{
  id: string;
  title: string;
}>('SELECT id, title FROM tasks WHERE done = $1');

const {rows} = await openTasks.execute([false]);
```

<section><h2 id="let-the-view-follow-the-data">Let the view follow the data</h2><p><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/subscriptions/subscribe/"><code>subscribe</code></a>() reports which tables changed, so a UI can re-query instead of being told what to redraw by every writer. <a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/lifecycle/close/"><code>close</code></a>() then releases statements, storage, and the Worker.</p></section>

```ts
const unsubscribe = db.subscribe(
  {tables: ['tasks']},
  () => render(),
);

// Later
unsubscribe();
await db.close();
```

<section><h2 id="go-deeper-when-you-need-to">Go deeper when you need to</h2><ul><li>Follow the <a href="https://tinyjoin.org/guides/getting-started/">getting started guide</a>.</li><li>Browse the <a href="https://tinyjoin.org/api/">API reference</a>.</li><li>Understand the <a href="https://tinyjoin.org/guides/caveats/">caveats</a>.</li><li>Review the <a href="https://tinyjoin.org/guides/releases/">release notes</a>.</li><li>Start an app with <a href="https://github.com/tinyplex/create-tinyjoin">create-tinyjoin</a>.</li><li>Read the <a href="https://github.com/tinyplex/tinyjoin">source</a>.</li></ul></section><section><h2 id="local-by-design">Local by design</h2><p>TinyJoin keeps your database in the browser. Your app reads and writes data locally, without waiting for a database server. Keep data in memory or save it across reloads with OPFS; tabs opening the same persistent database share access automatically.</p><p>Your app can keep working when the network drops. Apps created with the starter can also <a href="https://tinyjoin.org/guides/offline/">reopen offline</a> once their production build has been cached.</p></section><section id="warning" aria-labelledby="an-important-warning"><h2 id="an-important-warning">An important warning</h2><p>TinyJoin is experimental and verified on Chromium only, with a bounded SQL dialect and no built-in remote synchronization (yet!). Browser storage can be lost, so use it for data you can reconstruct. Read the <a href="https://tinyjoin.org/guides/caveats/">caveats</a> and <a href="https://tinyjoin.org/guides/sql-compatibility/">SQL compatibility guide</a> to check whether this project currently fits your app. We&#x27;re working on it though, so keep checking back.</p></section><hr><section id="family"><h2 id="meet-the-family">Meet the family</h2><p>TinyJoin is one of a group of small libraries that make rich client and local-first apps easier to build. Take a look at the others!</p><ul><li><a href="https://tinybase.org/"><img src="https://tinybase.org/favicon.svg?asImg" alt="" width="40" height="40"> <b>TinyBase</b> </a>A reactive data store with persistence and synchronization.</li><li><a href="https://synclets.org/"><img src="https://synclets.org/favicon.svg?asImg" alt="" width="40" height="40"> <b>Synclets</b> </a>An open, storage-agnostic sync engine development kit.</li><li><a href="https://tinywidgets.org/"><img src="https://tinywidgets.org/favicon.svg?asImg" alt="" width="40" height="40"> <b>TinyWidgets</b> </a>A collection of tiny, reusable UI components.</li><li><a href="https://tinytick.org/"><img src="https://tinytick.org/favicon.svg?asImg" alt="" width="40" height="40"> <b>TinyTick</b> </a>A tiny but very useful task orchestrator.</li></ul></section>