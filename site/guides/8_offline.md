# Offline

OPFS persists database rows. Reopening the application without a network also
requires its HTML, JavaScript, CSS, Worker, and WASM files to be available.
TinyJoin's optional Vite plugin caches the complete production build, including
lazy assets that the first visit has not used yet.

## Vite setup

Add tinyjoinOffline to the application's Vite configuration:

```ts
import {defineConfig} from 'vite';
import {tinyjoinOffline} from 'tinyjoin/vite';

export default defineConfig({
  plugins: [tinyjoinOffline()],
});
```

The [starter](/guides/getting-started/#install) includes this plugin.
No extra Worker entry, asset-copying command, or registration code is needed.
The plugin is a Node-only build integration; importing tinyjoin in application
code does not include it in the browser runtime.

Run the production build and serve the complete output over HTTPS, or localhost
while testing. The Vite development server does not register an offline service
worker. To exercise offline behavior locally, use `vite build` followed by
`vite preview`.

Production hosts must preserve the emitted asset paths, JavaScript/WASM MIME
types, and security headers, including on Worker responses. The
[hosting and CSP checklist](/guides/custom-workers/#production-hosting) includes
a restrictive policy tested with offline OPFS startup and reopen. The default
and custom Worker paths are verified with Vite in Chromium; other bundlers are
not currently verified.

The first visit needs a network connection. The service worker downloads and
verifies every build file before installation succeeds. After
`navigator.serviceWorker.ready` resolves, a subsequent navigation can use that
installed application offline. The first page is not forcibly taken over while
it is running.

## What gets cached

The plugin produces these files:

| File | Purpose |
| --- | --- |
| `tinyjoin-sw.js` | Application service worker. |
| `tinyjoin-register.js` | External registration script inserted into built HTML. |
| `tinyjoin-precache.json` | Build version, base, and relative asset URLs with SHA-256 content revisions. |
| `tinyjoin-precache.js` | Service-worker helper containing the same manifest. |

Every regular file in the final Vite output is included, including public files,
unused dynamic chunks, and TinyJoin's private lazy OPFS runtime. The service
worker and its manifest/helper are maintained by the browser's service-worker
installation mechanism rather than included recursively in their own cache.

API requests, cross-origin URLs, and files created after the build are outside
this cache. Bundle necessary application resources or provide your own caching
policy for them. Cache Storage and OPFS have separate contents; installing or
updating the application cache does not migrate or delete database rows.
Browsers can still clear or evict either kind of storage. See
[storage retention](/guides/storage-and-lifecycle/#browser-retention).

The plugin supports Vite bases such as `/`, `/my-app/`, and `./`. Its service
worker only controls the application's base directory. A CDN URL as the Vite
base is not supported because this integration requires same-origin files.

Navigation uses a cached emitted HTML file when one matches. Otherwise it falls
back to `index.html` for client-side routes within the application scope. Change
that file with `tinyjoinOffline({navigationFallback: 'app.html'})`, or disable
the fallback with `tinyjoinOffline({navigationFallback: false})`.

## Safe application updates

A new build gets a separate cache. Its files must all match their recorded
content hashes before it can install. An interrupted or mixed deployment leaves
the previously installed application available.

The new service worker waits until every tab controlled by the old one closes.
Running tabs keep the old application and its old assets, including lazy files
they have not fetched before. Once the replacement activates, it removes this
application's previous TinyJoin caches. Close all application tabs and reopen to
finish an update; reloading only one tab while another remains open can continue
using the old build.

Do not add automatic `skipWaiting()` or `clients.claim()` calls to force updates.
They can make a running application use assets from a different build. Deploy
the complete build together and retain normal static-file MIME types. If a
cached entry is later missing, the helper tries to restore the exact recorded
content from the network. If that content is unavailable or has changed, the
request fails with HTTP 503 rather than mixing in a newer deployment. Close the
application's tabs and reopen online to finish installing a newer release or
repair an incomplete cache from the matching deployment.

Application updates and database schema changes are separate decisions. Keep
schema setup idempotent and observe the
[storage compatibility boundary](/guides/releases/#v0-1-0).

## Integrating an existing service worker

An application should have one owner for its service-worker lifecycle. Automatic
TinyJoin registration detects another worker already controlling the application
and leaves it alone, with a console message explaining the conflict.

Use manifest mode when an application already owns its service worker:

```ts
export default defineConfig({
  plugins: [tinyjoinOffline({mode: 'manifest'})],
});
```

This emits only `tinyjoin-precache.json` and `tinyjoin-precache.js`. It does not
insert registration code or create `tinyjoin-sw.js`. A classic service worker
beside the build output can use the generated helper:

```js
importScripts('./tinyjoin-precache.js');

const precache = self.createTinyjoinPrecache();

self.addEventListener('install', (event) => {
  event.waitUntil(precache.install());
});
self.addEventListener('activate', (event) => {
  event.waitUntil(precache.activate());
});
self.addEventListener('fetch', (event) => {
  const response = precache.match(event.request);
  if (response) {
    event.respondWith(response);
  }
  // Otherwise leave this request to the application's existing fetch policy.
});
```

Combine this branch with the existing fetch handler so that each request calls
`respondWith` at most once. Keep the native waiting lifecycle described above;
calling activate early would remove assets still needed by old tabs. Register
the existing worker with `updateViaCache: 'none'` so its imported manifest is
checked for changes. For a relative Vite base and a service worker registered
outside the application directory, pass that directory explicitly:
`self.createTinyjoinPrecache(new URL('./my-app/', self.location.origin))`.

An existing precaching system can instead consume the JSON manifest. Resolve
its asset URLs against the application's build base and preserve content
verification and coherent version activation in that integration.

Consume the **complete** asset list, including the private lazy OPFS runtime,
Worker, and WASM files even when the first visit only opens a memory database.
Caching the entry script or the files observed during one online visit is not
sufficient for a later persistent open without a network.
