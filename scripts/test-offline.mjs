import assert from 'node:assert/strict';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {createServer} from 'node:http';
import {tmpdir} from 'node:os';
import {extname, resolve} from 'node:path';

import {chromium} from '@playwright/test';
import {build} from 'vite';

import {tinyjoinOffline} from '../dist/vite/index.js';

const root = await mkdtemp(resolve(tmpdir(), 'tinyjoin-offline-browser-'));
const output = resolve(root, 'dist');
// Keep this policy identical to the hosting example in the custom-Worker guide.
const policy = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; connect-src 'self'; style-src 'self'; img-src 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'";
const servedRuntime = new Set();
let browser;
let server;
try {
  await writeFile(
    resolve(root, 'index.html'),
    '<!doctype html><html><body><script type="module" src="/main.js"></script></body></html>',
  );
  await buildRelease('one');
  server = createServer(async (request, response) => {
    try {
      const url = new URL(request.url, 'http://localhost');
      if (!url.pathname.startsWith('/app/')) {
        response.writeHead(404).end();
        return;
      }
      const relative =
        decodeURIComponent(url.pathname.slice('/app/'.length)) || 'index.html';
      const path = resolve(output, relative);
      if (!path.startsWith(`${output}/`)) {
        response.writeHead(404).end();
        return;
      }
      const content = await readFile(path);
      const extension = extname(path);
      if (extension === '.wasm' || relative.includes('tinyjoin_opfs_runtime')) {
        servedRuntime.add(relative);
      }
      response
        .writeHead(200, {
          'Cache-Control': 'no-store',
          'Content-Security-Policy': policy,
          'X-Content-Type-Options': 'nosniff',
          'Content-Type': {
            '.html': 'text/html',
            '.js': 'text/javascript',
            '.wasm': 'application/wasm',
            '.json': 'application/json',
          }[extension] ?? 'application/octet-stream',
        })
        .end(content);
    } catch {
      response.writeHead(404).end();
    }
  });
  await new Promise((done) => server.listen(0, '127.0.0.1', done));
  const url = `http://127.0.0.1:${server.address().port}/app/`;
  browser = await chromium.launch({headless: true});
  const context = await browser.newContext();
  const violations = [];
  const runtimeResponses = [];
  context.on('response', (response) => {
    if (/\.(?:js|wasm)$/.test(new URL(response.url()).pathname)) {
      runtimeResponses.push({url: response.url(), headers: response.headers()});
    }
  });
  await context.exposeBinding('recordPolicyViolation', (_, directive) => {
    violations.push(directive);
  });
  await context.addInitScript(() => {
    addEventListener('securitypolicyviolation', (event) => {
      globalThis.recordPolicyViolation(event.effectiveDirective);
    });
  });
  context.on('console', (message) => {
    if (/violates.*Content Security Policy|Refused to.*(?:script|worker|WebAssembly)/i.test(message.text())) {
      violations.push(message.text());
    }
  });
  const first = await context.newPage();
  const initial = await first.goto(url);
  assert.equal(initial.headers()['content-security-policy'], policy);
  await first.evaluate(() => navigator.serviceWorker.ready);
  const controlled = await first.reload();
  assert.equal(controlled.fromServiceWorker(), true);
  assert.equal(controlled.headers()['content-security-policy'], policy);
  assert.equal(await first.textContent('body'), 'one');
  await context.setOffline(true);
  // This module has never been executed: precaching must cover unused chunks too.
  assert.equal(await first.evaluate(() => globalThis.loadLazy()), 'lazy one');
  // Neither TinyJoin nor its lazy OPFS runtime has been used before going offline.
  await first.evaluate(async () => {
    await globalThis.openDatabase();
    await globalThis.database.exec('CREATE TABLE IF NOT EXISTS notes (id TEXT PRIMARY KEY, value TEXT NOT NULL)');
    await globalThis.database.query('INSERT INTO notes VALUES ($1, $2)', ['one', 'saved offline']);
  });
  assert.ok([...servedRuntime].some((path) => path.endsWith('.wasm')));
  assert.ok([...servedRuntime].some((path) => path.includes('tinyjoin_opfs_runtime')));
  const second = await context.newPage();
  const offlineNavigation = await second.goto(url);
  assert.equal(offlineNavigation.fromServiceWorker(), true);
  assert.equal(offlineNavigation.headers()['content-security-policy'], policy);
  assert.deepEqual(await second.evaluate(async () => {
    await globalThis.openDatabase();
    return (await globalThis.database.query('SELECT * FROM notes ORDER BY id')).rows;
  }), [{id: 'one', value: 'saved offline'}]);
  await second.evaluate(async () => {
    await globalThis.database.query('UPDATE notes SET value = $1 WHERE id = $2', ['updated by follower', 'one']);
  });
  await first.evaluate(() => globalThis.database.close());
  assert.deepEqual(await second.evaluate(async () => {
    const {rows} = await globalThis.database.query('SELECT * FROM notes ORDER BY id');
    await globalThis.database.close();
    return rows;
  }), [{id: 'one', value: 'updated by follower'}]);
  await first.reload();
  assert.deepEqual(await first.evaluate(async () => {
    await globalThis.openDatabase();
    const {rows} = await globalThis.database.query('SELECT * FROM notes ORDER BY id');
    await globalThis.database.close();
    return rows;
  }), [{id: 'one', value: 'updated by follower'}]);
  await context.setOffline(false);

  await buildRelease('two');
  await first.evaluate(async () =>
    (await navigator.serviceWorker.getRegistration()).update(),
  );
  await first.waitForFunction(
    async () =>
      (await navigator.serviceWorker.getRegistration()).waiting?.state ===
      'installed',
  );
  assert.equal((await first.evaluate(() => caches.keys())).length, 2);
  await context.setOffline(true);
  await first.reload();
  assert.equal(await first.textContent('body'), 'one');
  await first.close();
  assert.equal(await second.textContent('body'), 'one');
  assert.equal((await second.evaluate(() => caches.keys())).length, 2);
  await second.close();

  // Native service-worker waiting, rather than a timer or force-activation message,
  // must retire v1 only once both old tabs have closed.
  await waitFor(async () => {
    for (const worker of context.serviceWorkers()) {
      try {
        if ((await worker.evaluate(() => caches.keys())).length === 1)
          return true;
      } catch {
        // An old worker can disappear while the replacement activates.
      }
    }
    return false;
  });
  const third = await context.newPage();
  const upgraded = await third.goto(url);
  assert.equal(upgraded.fromServiceWorker(), true);
  assert.equal(upgraded.headers()['content-security-policy'], policy);
  assert.equal(await third.textContent('body'), 'two');
  assert.equal(await third.evaluate(() => globalThis.loadLazy()), 'lazy two');
  assert.deepEqual(await third.evaluate(async () => {
    await globalThis.openDatabase();
    const {rows} = await globalThis.database.query('SELECT * FROM notes ORDER BY id');
    await globalThis.database.close();
    return rows;
  }), [{id: 'one', value: 'updated by follower'}]);
  await context.setOffline(false);

  await buildRelease('three');
  const manifest = JSON.parse(
    await readFile(resolve(output, 'tinyjoin-precache.json'), 'utf8'),
  );
  const lazy = manifest.assets.find((asset) => /lazy-.*\.js$/.test(asset.url));
  assert.ok(lazy);
  await writeFile(
    resolve(output, lazy.url),
    'export const value = "mixed deployment";',
  );
  const failedState = await third.evaluate(async () => {
    const registration = await navigator.serviceWorker.getRegistration();
    const installed = new Promise((resolve) => {
      registration.addEventListener(
        'updatefound',
        () => {
          const worker = registration.installing;
          worker.addEventListener('statechange', () => {
            if (worker.state === 'redundant' || worker.state === 'installed')
              resolve(worker.state);
          });
        },
        {once: true},
      );
    });
    await registration.update();
    return installed;
  });
  assert.equal(failedState, 'redundant');
  assert.equal((await third.evaluate(() => caches.keys())).length, 1);
  await context.setOffline(true);
  await third.reload();
  assert.equal(await third.textContent('body'), 'two');
  assert.equal(await third.evaluate(() => globalThis.loadLazy()), 'lazy two');
  assert.deepEqual(violations, []);
  assert.ok(runtimeResponses.some(({url}) => url.endsWith('.wasm')));
  assert.ok(runtimeResponses.some(({url}) => /default-entry/.test(url)));
  for (const {url, headers} of runtimeResponses) {
    assert.equal(headers['content-security-policy'], policy, url);
    assert.equal(headers['content-type'], url.endsWith('.wasm') ? 'application/wasm' : 'text/javascript', url);
  }
  console.log(
    'OFFLINE_RELEASE_LIFECYCLE_OK restrictive CSP, default Worker/WASM, offline OPFS first open and reopen, follower write, unused lazy asset, two-tab waiting update, offline activation, torn deployment rollback',
  );
  await context.close();
} finally {
  await browser?.close();
  if (server) await new Promise((done) => server.close(done));
  await rm(root, {force: true, recursive: true});
}

async function buildRelease(version) {
  await writeFile(
    resolve(root, 'main.js'),
    `document.body.textContent = ${JSON.stringify(version)};
globalThis.loadLazy = async () => (await import('./lazy.js')).value;
globalThis.openDatabase = async () => {
  const {create} = await import('tinyjoin');
  globalThis.database = await create('opfs://offline-csp-v1');
};`,
  );
  await writeFile(
    resolve(root, 'lazy.js'),
    `export const value = ${JSON.stringify(`lazy ${version}`)};`,
  );
  await build({
    root,
    base: '/app/',
    configFile: false,
    logLevel: 'silent',
    resolve: {alias: {tinyjoin: resolve('dist/index.js')}},
    plugins: [tinyjoinOffline()],
  });
}

async function waitFor(callback) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    if (await callback()) return;
    await new Promise((done) => setTimeout(done, 50));
  }
  throw new Error('Timed out waiting for service-worker activation');
}
