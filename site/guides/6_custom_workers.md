# Custom Workers

Most applications should use create(). It constructs TinyJoin's packaged
module Worker and preserves the relative Worker, OPFS runtime, and WebAssembly
assets during the supported Vite build path.

For Node.js, use [`tinyjoin/node`](/guides/node/), which constructs its own
Worker thread and loads WebAssembly without a custom bootstrap.

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

## Production hosting

The default Worker and custom Worker paths are verified with Vite production
builds in Chromium. Other bundlers are not currently verified; a successful
TypeScript build alone does not check runtime asset resolution. Serve and test
the complete emitted build before relying on another bundler.

- Use HTTPS, or a trustworthy localhost origin for development. OPFS and
  service workers require a secure context.
- Keep the application, initial module Worker, service worker, and all runtime
  assets on the same origin. Serve the entire build directory at its configured
  Vite base, preserving the emitted relative paths and private lazy OPFS files.
- Return JavaScript as `text/javascript` and WASM as `application/wasm`.
  Return HTML only for navigation routes: a missing Worker or WASM URL must
  return an error rather than an HTML fallback. JSON manifests use
  `application/json`.
- Deploy the build as one version. Do not remove files still needed by running
  tabs. See [offline updates](/guides/offline/#safe-application-updates) for
  installation, cache integrity, and waiting-tab behavior.

### Content Security Policy

This restrictive starting policy is tested with the default Worker, OPFS,
two-tab access, and offline close/reopen in a production Vite fixture:

```http
Content-Security-Policy: default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; connect-src 'self'; style-src 'self'; img-src 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'
X-Content-Type-Options: nosniff
```

Send these headers on HTML **and Worker/service-worker JavaScript responses**.
A Worker has its own execution policy; an HTML policy alone does not establish
the Worker script's execution restrictions. Keep the headers on cached
responses too. The fixture checks an offline navigation using those responses.

`worker-src 'self'` permits same-origin Workers and service workers.
`connect-src 'self'` permits runtime and precache fetches.
`script-src 'self' 'wasm-unsafe-eval'` permits the emitted scripts and
WebAssembly compilation without enabling JavaScript `eval`. The narrower
WebAssembly permission is defined by
[Content Security Policy](https://www.w3.org/TR/CSP/#directive-script-src).
No `blob:`, `data:`, inline-script permission, or broad `unsafe-eval` permission
is required by the verified default Vite path.

Adapt styles, images, and other application resources deliberately: this example
uses external scripts and styles on the same origin. Test the actual application
with its response headers; custom Worker URLs and other bundlers may emit a
different asset graph. The production fixture runs with
`npm run build && npm run test:offline:built` in this repository.
