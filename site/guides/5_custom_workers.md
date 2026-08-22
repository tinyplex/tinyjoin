# Custom Workers

Most applications should use create(). It constructs TinyGres's packaged
module Worker and preserves the relative Worker, OPFS runtime, and WebAssembly
assets during the supported Vite build path.

An application that needs to own the Worker can provide a factory:

```ts
const db = await create({
  workerFactory: () =>
    new Worker(new URL('./tinygres.worker.ts', import.meta.url), {
      name: 'tinygres',
      type: 'module',
    }),
});
```

The Worker entry starts the same standalone engine:

```ts
import {startWorker} from 'tinygres/worker';

startWorker();
```

Use at most one of `worker`, `workerFactory`, or `workerUrl`. A custom Worker is
an advanced bundling boundary, not a requirement for persistent storage.

Synchronous OPFS access is available in a dedicated Worker, not a SharedWorker.
Ordinary browser Workers also cannot open arbitrary PostgreSQL TCP connections.
Network replication or offline write synchronization would require separately
designed transports, durability, authorization, and conflict semantics; none is
hidden in the current package.
