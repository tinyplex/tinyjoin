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
      response
        .writeHead(200, {
          'Cache-Control': 'no-store',
          'Content-Type':
            extname(path) === '.html' ? 'text/html' : 'text/javascript',
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
  const first = await context.newPage();
  await first.goto(url);
  await first.evaluate(() => navigator.serviceWorker.ready);
  const controlled = await first.reload();
  assert.equal(controlled.fromServiceWorker(), true);
  assert.equal(await first.textContent('body'), 'one');
  await context.setOffline(true);
  // This module has never been executed: precaching must cover unused chunks too.
  assert.equal(await first.evaluate(() => globalThis.loadLazy()), 'lazy one');
  await context.setOffline(false);
  const second = await context.newPage();
  await second.goto(url);

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
  assert.equal(await third.textContent('body'), 'two');
  assert.equal(await third.evaluate(() => globalThis.loadLazy()), 'lazy two');
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
  console.log(
    'OFFLINE_RELEASE_LIFECYCLE_OK first install, unused lazy asset, two-tab waiting update, offline activation, torn deployment rollback',
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
    `document.body.textContent = ${JSON.stringify(version)}; globalThis.loadLazy = async () => (await import('./lazy.js')).value;`,
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
