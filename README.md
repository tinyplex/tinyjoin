<section id="hero"><h2 id="a-tiny-worker-first-relational-database-for-browser-apps">A tiny, worker-first <em>relational database</em> for browser apps.</h2><p>PostgreSQL-shaped SQL, running locally and away from the main thread.</p></section><nav id="actions" aria-label="Get started"><a class="start" href="https://tinyjoin.org/guides/getting-started/">Get started</a> <a href="https://tinyjoin.org/demos/">Try the demos</a> <a href="https://tinyjoin.org/api/">Read the API</a></nav><hr><section><h2 id="start-small">Start small</h2><p>Install TinyJoin, import <a href="https://tinyjoin.org/api/tinyjoin/functions/lifecycle/create/"><code>create</code></a>(), and open a database. The normal setup does not need Rust tooling, a Worker entry, a WASM plugin, or a runtime copying step.</p></section>

```sh
npm install tinyjoin
```

<section><h2 id="one-import-no-infrastructure-setup">One import. No infrastructure setup.</h2><p><a href="https://tinyjoin.org/api/tinyjoin/functions/lifecycle/create/"><code>create</code></a>() owns Worker construction and WebAssembly loading. Use a stable <code>opfs://name</code> when data should survive reloads in the same browser, or call <a href="https://tinyjoin.org/api/tinyjoin/functions/lifecycle/create/"><code>create</code></a>() with no argument for an ephemeral memory database.</p></section>

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

<section><h2 id="a-familiar-intentionally-small-api">A familiar, intentionally small API</h2><ul><li><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/sql/query/"><code>query</code></a>() runs one parameterized read or write statement.</li><li><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/sql/exec/"><code>exec</code></a>() runs a parameter-free SQL script atomically.</li><li><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/transactions/transaction/"><code>transaction</code></a>() commits related row mutations together.</li><li><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/sql/prepare/"><code>prepare</code></a>() retains a statement for repeated execution.</li><li><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/subscriptions/subscribe/"><code>subscribe</code></a>() reports which tables changed so an app can re-query.</li><li><a href="https://tinyjoin.org/api/tinyjoin/classes/lifecycle/client/methods/lifecycle/close/"><code>close</code></a>() releases statements, storage, and the Worker.</li></ul><p><a href="https://tinyjoin.org/api/tinyjoin/interfaces/query-results/results/"><code>Results</code></a> use the familiar <code>rows</code>, <code>fields</code>, <code>affectedRows</code>, <code>command</code>, and <code>rowCount</code> shape. TinyJoin also reports a database <code>revision</code> and changed <code>tables</code>.</p></section><section><h2 id="local-by-default">Local by default</h2><p>TinyJoin contains no hosted service, credentials, analytics, or hidden network path. Memory and OPFS use the same page-native database engine. Persistent OPFS storage is single-writer and intended for reconstructable application data; users can still clear or lose browser-managed storage.</p></section><section><h2 id="deliberately-bounded">Deliberately bounded</h2><p><strong>Important:</strong> TinyJoin is experimental. It implements a deliberately bounded SQL and type subset; it is not PostgreSQL compiled to WebAssembly and has no PostgreSQL server, wire protocol, or replication client.</p><p>Check the exact <a href="https://tinyjoin.org/guides/sql-compatibility/">SQL compatibility contract</a> before relying on unlisted PostgreSQL syntax or types.</p></section><section><h2 id="go-deeper-when-you-need-to">Go deeper when you need to</h2><ul><li>Follow the <a href="https://tinyjoin.org/guides/getting-started/">getting started guide</a>.</li><li>Browse the <a href="https://tinyjoin.org/api/">API reference</a>.</li><li>Review the <a href="https://tinyjoin.org/guides/releases/">release notes</a>.</li><li>Start an app with <a href="https://github.com/tinyplex/create-tinyjoin">create-tinyjoin</a>.</li><li>Read the <a href="https://github.com/tinyplex/tinyjoin">source</a>.</li></ul><p>TinyJoin is MIT licensed.</p></section>