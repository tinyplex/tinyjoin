/**
 * The vite module provides optional production offline caching for Vite
 * applications, including TinyJoin's lazily loaded Worker, OPFS, and WASM files.
 * This Node-only build integration is separate from the browser runtime.
 * @packageDocumentation
 * @module vite
 * @since v0.0.6
 */
/// vite

/**
 * The TinyjoinOfflineOptions interface configures production offline caching.
 * @category Offline
 * @since v0.0.6
 */
/// TinyjoinOfflineOptions

/**
 * The mode property selects automatic service-worker registration or integration
 * with an existing service worker. The default is `service-worker`.
 *
 * The `manifest` mode emits `tinyjoin-precache.json` and
 * `tinyjoin-precache.js` without registering or replacing a service worker.
 * @category Offline
 * @since v0.0.6
 */
/// TinyjoinOfflineOptions.mode

/**
 * The navigationFallback property names the emitted HTML file served for
 * otherwise unmatched navigation requests within the application scope.
 * It defaults to `index.html`; use `false` to disable SPA navigation fallback.
 * @category Offline
 * @since v0.0.6
 */
/// TinyjoinOfflineOptions.navigationFallback

/**
 * The tinyjoinOffline function returns a Vite plugin that precaches the complete
 * emitted production build, including lazy runtime assets, with verified content
 * hashes. It does not register a service worker during development.
 *
 * The first visit requires a network connection. Wait for
 * `navigator.serviceWorker.ready` before expecting a subsequent navigation to
 * work offline. Storage persistence and application-file caching remain separate:
 * OPFS stores the database, while the service worker stores the application.
 *
 * Updates wait until all tabs controlled by the old service worker close. The
 * plugin does not force a new worker onto running tabs. Serve the whole build
 * together at one same-origin root-relative or relative Vite base. External
 * requests and files created after the build are not cached.
 * @example
 * ```ts
 * import {defineConfig} from 'vite';
 * import {tinyjoinOffline} from 'tinyjoin/vite';
 *
 * export default defineConfig({plugins: [tinyjoinOffline()]});
 * ```
 * @category Offline
 * @since v0.0.6
 */
/// tinyjoinOffline
