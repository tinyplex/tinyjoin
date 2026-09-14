import {createHash, webcrypto} from 'node:crypto';
import {
  mkdtemp,
  mkdir,
  readFile,
  readdir,
  rm,
  writeFile,
} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {resolve} from 'node:path';
import {runInNewContext} from 'node:vm';
import {afterEach, describe, expect, test} from 'vitest';
import {build} from 'vite';

import {tinyjoinOffline} from '../../src/vite/index.js';
import {
  helperSource,
  registrationSource,
  serviceWorkerSource,
} from '../../src/vite/offline.js';

const directories: string[] = [];
afterEach(async () => {
  await Promise.all(
    directories
      .splice(0)
      .map((directory) => rm(directory, {force: true, recursive: true})),
  );
});

const digest = (source: string | Uint8Array): string =>
  createHash('sha256').update(source).digest('hex');
async function fixture() {
  const root = await mkdtemp(resolve(tmpdir(), 'tinyjoin-offline-'));
  directories.push(root);
  await mkdir(resolve(root, 'public'), {recursive: true});
  await writeFile(
    resolve(root, 'index.html'),
    '<!doctype html><script type="module" src="/main.js"></script>',
  );
  await writeFile(
    resolve(root, 'main.js'),
    "new Worker(new URL('./worker.js', import.meta.url), {type:'module'});",
  );
  await writeFile(
    resolve(root, 'worker.js'),
    "const url = new URL('./lazy.js', import.meta.url); self.onmessage = () => import(/* @vite-ignore */ url.href);",
  );
  await writeFile(
    resolve(root, 'lazy.js'),
    `export const value = 42;\n//${'not inlined '.repeat(500)}`,
  );
  await writeFile(
    resolve(root, 'public/asset #.txt'),
    'a public file not present in the module graph',
  );
  return root;
}

async function files(directory: string, prefix = ''): Promise<string[]> {
  const output: string[] = [];
  for (const entry of await readdir(resolve(directory, prefix), {
    withFileTypes: true,
  })) {
    const file = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) output.push(...(await files(directory, file)));
    else output.push(file);
  }
  return output.sort();
}

describe('tinyjoinOffline build', () => {
  test('precaches the final app, Worker, ignored lazy asset and public files under a base', async () => {
    const root = await fixture();
    await build({
      root,
      base: '/app/',
      configFile: false,
      logLevel: 'silent',
      plugins: [tinyjoinOffline()],
    });
    const directory = resolve(root, 'dist');
    const manifest = JSON.parse(
      await readFile(resolve(directory, 'tinyjoin-precache.json'), 'utf8'),
    );
    const expected = (await files(directory)).filter(
      (file) =>
        ![
          'tinyjoin-sw.js',
          'tinyjoin-precache.json',
          'tinyjoin-precache.js',
        ].includes(file),
    );
    expect(
      manifest.assets.map((asset: {url: string}) =>
        decodeURIComponent(asset.url),
      ),
    ).toEqual(expected);
    expect(expected.some((file) => /lazy-.*\.js$/.test(file))).toBe(true);
    expect(manifest.base).toBe('/app/');
    expect(await readFile(resolve(directory, 'index.html'), 'utf8')).toContain(
      'src="/app/tinyjoin-register.js"',
    );
    for (const asset of manifest.assets) {
      expect(asset.revision).toBe(
        digest(
          await readFile(resolve(directory, decodeURIComponent(asset.url))),
        ),
      );
    }
    const version = manifest.version;
    await writeFile(resolve(root, 'public/asset #.txt'), 'a second release');
    await build({
      root,
      base: '/app/',
      configFile: false,
      logLevel: 'silent',
      plugins: [tinyjoinOffline()],
    });
    expect(
      JSON.parse(
        await readFile(resolve(directory, 'tinyjoin-precache.json'), 'utf8'),
      ).version,
    ).not.toBe(version);
  });

  test('relative bases register from the correct directory for each HTML entry', async () => {
    const root = await fixture();
    await mkdir(resolve(root, 'nested'));
    await writeFile(
      resolve(root, 'nested/index.html'),
      '<!doctype html><script type="module" src="/main.js"></script>',
    );
    await build({
      root,
      base: './',
      configFile: false,
      logLevel: 'silent',
      plugins: [tinyjoinOffline()],
      build: {
        rolldownOptions: {
          input: {
            main: resolve(root, 'index.html'),
            nested: resolve(root, 'nested/index.html'),
          },
        },
      },
    });
    expect(await readFile(resolve(root, 'dist/index.html'), 'utf8')).toContain(
      'src="tinyjoin-register.js"',
    );
    expect(
      await readFile(resolve(root, 'dist/nested/index.html'), 'utf8'),
    ).toContain('src="../tinyjoin-register.js"');
  });

  test('manifest mode leaves service worker ownership with the application', async () => {
    const root = await fixture();
    await build({
      root,
      configFile: false,
      logLevel: 'silent',
      plugins: [tinyjoinOffline({mode: 'manifest'})],
    });
    const output = await files(resolve(root, 'dist'));
    expect(output).toContain('tinyjoin-precache.json');
    expect(output).toContain('tinyjoin-precache.js');
    expect(output).not.toContain('tinyjoin-register.js');
    expect(output).not.toContain('tinyjoin-sw.js');
    expect(
      await readFile(resolve(root, 'dist/index.html'), 'utf8'),
    ).not.toContain('tinyjoin-register');
  });

  test('manifest mode preserves and caches an application-owned registration script', async () => {
    const root = await fixture();
    await writeFile(
      resolve(root, 'public/tinyjoin-sw.js'),
      '// Application-owned worker',
    );
    await writeFile(
      resolve(root, 'public/tinyjoin-register.js'),
      '// Application-owned registration',
    );
    await build({
      root,
      configFile: false,
      logLevel: 'silent',
      plugins: [tinyjoinOffline({mode: 'manifest'})],
    });
    const manifest = JSON.parse(
      await readFile(resolve(root, 'dist/tinyjoin-precache.json'), 'utf8'),
    );
    expect(manifest.assets.map((asset: {url: string}) => asset.url)).toContain(
      'tinyjoin-register.js',
    );
    expect(await readFile(resolve(root, 'dist/tinyjoin-sw.js'), 'utf8')).toBe(
      '// Application-owned worker',
    );
  });

  test('rejects remote asset bases and reserved public-file collisions', async () => {
    const root = await fixture();
    await expect(
      build({
        root,
        base: 'https://cdn.example/app/',
        configFile: false,
        logLevel: 'silent',
        plugins: [tinyjoinOffline()],
      }),
    ).rejects.toThrow('same-origin');
    await writeFile(
      resolve(root, 'public/tinyjoin-sw.js'),
      'an existing worker',
    );
    await expect(
      build({
        root,
        configFile: false,
        logLevel: 'silent',
        plugins: [tinyjoinOffline()],
      }),
    ).rejects.toThrow('reserves tinyjoin-sw.js');
  });
});

function cacheHarness(
  version: string,
  sources: Record<string, string>,
  shared = new Map<string, Map<string, Response>>(),
) {
  const origin = 'https://example.test';
  const manifest = {
    version,
    base: '/app/',
    navigationFallback: 'index.html',
    assets: Object.entries(sources).map(([url, source]) => ({
      url,
      revision: digest(source),
    })),
  };
  const fetches: string[] = [];
  const self: Record<string, any> = {
    location: {origin},
    registration: {scope: `${origin}/app/`},
  };
  const network = new Map(
    Object.entries(sources).map(([url, source]) => [
      `${origin}/app/${url}`,
      source,
    ]),
  );
  runInNewContext(helperSource(manifest), {
    self,
    crypto: webcrypto,
    URL,
    Request,
    Response,
    Uint8Array,
    caches: {
      async open(name: string) {
        if (!shared.has(name)) shared.set(name, new Map());
        const cache = shared.get(name)!;
        return {
          async put(url: string, response: Response) {
            cache.set(url, response.clone());
          },
          async match(url: string) {
            return cache.get(url)?.clone();
          },
        };
      },
      async keys() {
        return [...shared.keys()];
      },
      async delete(name: string) {
        return shared.delete(name);
      },
    },
    async fetch(request: Request) {
      fetches.push(request.url);
      if (!network.has(request.url)) throw new Error('offline');
      return new Response(network.get(request.url));
    },
  });
  const cache = self.createTinyjoinPrecache();
  const request = (path: string, mode = 'cors', method = 'GET') => ({
    url: `${origin}${path}`,
    mode,
    method,
  });
  return {cache, fetches, network, shared, request};
}

describe('generated offline lifecycle', () => {
  test('serves a complete installed release offline, including a previously unused lazy module', async () => {
    const state = cacheHarness('one', {
      'index.html': 'app one',
      'worker.js': 'worker one',
      'lazy-opfs.js': 'lazy one',
    });
    await state.cache.install();
    state.network.clear();
    expect(
      await (
        await state.cache.match(state.request('/app/?view=todo', 'navigate'))
      ).text(),
    ).toBe('app one');
    expect(
      await (
        await state.cache.match(state.request('/app/lazy-opfs.js'))
      ).text(),
    ).toBe('lazy one');
    expect(
      await (
        await state.cache.match(state.request('/app/deep/route', 'navigate'))
      ).text(),
    ).toBe('app one');
    expect(
      state.cache.match(state.request('/outside/', 'navigate')),
    ).toBeUndefined();
    expect(state.cache.match(state.request('/app/api'))).toBeUndefined();
    expect(
      state.cache.match(state.request('/app/worker.js', 'cors', 'POST')),
    ).toBeUndefined();
    expect(state.fetches).toHaveLength(3);
  });

  test('keeps the old cache while a verified new release is waiting and only removes it at activation', async () => {
    const first = cacheHarness('one', {'index.html': 'app one'});
    await first.cache.install();
    const second = cacheHarness('two', {'index.html': 'app two'}, first.shared);
    await second.cache.install();
    expect(first.shared.size).toBe(2);
    expect(
      await (
        await first.cache.match(first.request('/app/', 'navigate'))
      ).text(),
    ).toBe('app one');
    await second.cache.activate();
    expect(first.shared.size).toBe(1);
    expect(
      await (
        await second.cache.match(second.request('/app/', 'navigate'))
      ).text(),
    ).toBe('app two');
    expect(serviceWorkerSource()).not.toContain('skipWaiting');
    expect(serviceWorkerSource()).not.toContain('clients.claim');
  });

  test('rejects a torn deployment and preserves the installed version', async () => {
    const first = cacheHarness('one', {'index.html': 'app one'});
    await first.cache.install();
    const second = cacheHarness(
      'two',
      {'index.html': 'app two', 'lazy.js': 'lazy two'},
      first.shared,
    );
    second.network.set(
      'https://example.test/app/lazy.js',
      'a file from another deployment',
    );
    await expect(second.cache.install()).rejects.toThrow('Build changed');
    expect(first.shared.size).toBe(1);
    expect(
      await (
        await first.cache.match(first.request('/app/', 'navigate'))
      ).text(),
    ).toBe('app one');
  });

  test('does not mix a new deployment into an evicted cache entry', async () => {
    const state = cacheHarness('one', {'index.html': 'app one'});
    await state.cache.install();
    state.shared.values().next().value!.clear();
    state.network.set('https://example.test/app/index.html', 'app two');
    expect(
      (await state.cache.match(state.request('/app/', 'navigate'))).status,
    ).toBe(503);
    expect(state.fetches).toHaveLength(2);
  });

  test('repairs an evicted entry online only when its content matches the installed release', async () => {
    const state = cacheHarness('one', {'index.html': 'app one'});
    await state.cache.install();
    state.shared.values().next().value!.clear();
    expect(
      await (
        await state.cache.match(state.request('/app/', 'navigate'))
      ).text(),
    ).toBe('app one');
    state.network.clear();
    expect(
      await (
        await state.cache.match(state.request('/app/', 'navigate'))
      ).text(),
    ).toBe('app one');
    expect(state.fetches).toHaveLength(2);
  });

  test('does not replace an application-owned service worker', async () => {
    let registered = false;
    const warnings: string[] = [];
    runInNewContext(registrationSource(), {
      URL,
      document: {
        currentScript: {src: 'https://example.test/app/tinyjoin-register.js'},
      },
      navigator: {
        serviceWorker: {
          async getRegistration() {
            return {active: {scriptURL: 'https://example.test/app/own-sw.js'}};
          },
          async register() {
            registered = true;
          },
        },
      },
      console: {warn: (message: string) => warnings.push(message)},
    });
    await new Promise((done) => setTimeout(done, 0));
    expect(registered).toBe(false);
    expect(warnings[0]).toContain('existing service worker');
  });
});
