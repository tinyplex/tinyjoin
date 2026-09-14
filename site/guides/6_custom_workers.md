# Custom Workers

Most applications should use create(). It constructs TinyJoin's packaged
module Worker and preserves the relative Worker, OPFS runtime, and WebAssembly
assets during the supported Vite build path.

An application that needs to own the Worker can provide a factory:

```ts
const db = await create({
  workerFactory: () =>
    new Worker(new URL('./tinyjoin.worker.ts', import.meta.url), {
      name: 'tinyjoin',
      type: 'module',
    }),
});
```

The Worker entry starts the same engine and automatic OPFS coordination:

```ts
import {startWorker} from 'tinyjoin/worker';

startWorker();
```

Use at most one of `worker`, `workerFactory`, or `workerUrl`. A custom Worker is
an advanced bundling boundary, not a requirement for persistent storage.

Tabs using the same OPFS name and compatible TinyJoin release share one owner,
even when their Workers use different bundle URLs. Keep startWorker() in the
entry to participate in that protocol. An arbitrary Worker-like replacement is
responsible for implementing its own storage and coordination behavior. The
packaged Worker also requests subscription refreshes when the page becomes
visible or resumes; custom lifecycle integration should re-query on restoration.

Synchronous OPFS access is available in a dedicated Worker, not a SharedWorker.
Ordinary browser Workers also cannot open arbitrary PostgreSQL TCP connections.
Network replication or offline write synchronization would require separately
designed transports, durability, authorization, and conflict semantics; none is
hidden in the current package.
