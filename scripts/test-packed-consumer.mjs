import {spawn, spawnSync} from 'node:child_process';
import {rmSync} from 'node:fs';
import {createServer} from 'node:http';
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rm,
  writeFile,
} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {dirname, join, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

import {chromium} from '@playwright/test';
import {WebSocketServer} from 'ws';

import {
  requireWasmArtifacts,
  wasmArtifacts,
} from './wasm-artifacts.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const fixture = resolve(root, 'test/consumers/vite');
const generatedRoot = await mkdtemp(
  join(tmpdir(), 'tinygres-packed-consumer-'),
);
const packageDirectory = resolve(generatedRoot, 'package');
const appDirectory = resolve(generatedRoot, 'app');
const port = 43_117;
const baseUrl = `http://127.0.0.1:${port}`;
const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
const cleanupGeneratedRoot = () =>
  rmSync(generatedRoot, {force: true, recursive: true});

process.once('exit', cleanupGeneratedRoot);
await mkdir(packageDirectory, {recursive: true});

// Build and pack the same clean dist directory that is published to npm.
run(npm, ['run', 'build'], root);
await assertBuildLibRejectsMissingMigrationArtifact();
const packOutput = run(
  npm,
  [
    'pack',
    './dist',
    '--ignore-scripts',
    '--json',
    '--pack-destination',
    packageDirectory,
  ],
  root,
);
const packed = parsePackOutput(packOutput);
assertPackedFiles(packed);
const tarball = resolve(packageDirectory, packed.filename);

await cp(fixture, appDirectory, {recursive: true});
const manifestPath = resolve(appDirectory, 'package.json');
const manifest = JSON.parse(await readFile(manifestPath, 'utf8'));
manifest.dependencies.tinygres = `file:${tarball}`;
await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

run(npm, ['install', '--no-audit', '--no-fund'], appDirectory);
const installedPackage = resolve(appDirectory, 'node_modules/tinygres');
const installedRealPath = await realpath(installedPackage);
const nodeModulesRealPath = await realpath(resolve(appDirectory, 'node_modules'));
if (installedRealPath !== resolve(nodeModulesRealPath, 'tinygres')) {
  throw new Error(
    `Expected a packed install under node_modules, received ${installedRealPath}`,
  );
}

const installedManifest = JSON.parse(
  await readFile(resolve(installedPackage, 'package.json'), 'utf8'),
);
if (installedManifest.name !== 'tinygres') {
  throw new Error('The installed tarball is not the tinygres package');
}
for (const developmentField of ['private', 'scripts', 'devDependencies']) {
  if (developmentField in installedManifest) {
    throw new Error(
      `Published TinyGres manifest contains development field ${developmentField}`,
    );
  }
}
if (
  installedManifest.dependencies &&
  Object.keys(installedManifest.dependencies).length > 0
) {
  throw new Error(
    `Published TinyGres unexpectedly has runtime dependencies: ${Object.keys(installedManifest.dependencies).join(', ')}`,
  );
}

const ssrOutput = run(
  process.execPath,
  [
    '--input-type=module',
    '--eval',
    "const pkg = await import('tinygres'); if (typeof pkg.createClient !== 'function') throw new Error('missing client export'); console.log('SSR_IMPORT_OK');",
  ],
  appDirectory,
);
if (!ssrOutput.includes('SSR_IMPORT_OK')) {
  throw new Error(`SSR-safe import did not complete:\n${ssrOutput}`);
}

run(npm, ['run', 'typecheck'], appDirectory);
run(npm, ['run', 'build'], appDirectory);
const builtFiles = await listFiles(resolve(appDirectory, 'dist'));
await assertConsumerMigrationBoundary(builtFiles);
if (!builtFiles.some((file) => file.endsWith('.wasm'))) {
  throw new Error(`The consumer build emitted no WASM asset:\n${builtFiles.join('\n')}`);
}

let server;
let serverOutput = '';
let browser;
let supabaseMock;
let completed = false;
try {
  server = spawn(
    npm,
    [
      'run',
      'preview',
      '--',
      '--host',
      '127.0.0.1',
      '--port',
      String(port),
      '--strictPort',
    ],
    {
      cwd: appDirectory,
      env: process.env,
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  server.stdout.on('data', (chunk) => {
    serverOutput += String(chunk);
  });
  server.stderr.on('data', (chunk) => {
    serverOutput += String(chunk);
  });
  supabaseMock = await startSupabaseMock();
  await waitForServer(server, baseUrl, () => serverOutput);
  browser = await chromium.launch({headless: true});
  const page = await browser.newPage();
  const runtimeRequests = [];
  page.on('request', (request) => runtimeRequests.push(request.url()));

  const appLocal = await exerciseWithoutMigration(
    page,
    `${baseUrl}/?worker=app-local`,
    runtimeRequests,
  );
  console.log(`APP_LOCAL_WORKER_OK ${JSON.stringify(appLocal)}`);

  const packageDefault = await exerciseWithoutMigration(
    page,
    `${baseUrl}/?worker=default`,
    runtimeRequests,
  );
  console.log(`PACKAGE_DEFAULT_WORKER_OK ${JSON.stringify(packageDefault)}`);

  for (const workerMode of ['app-local', 'default']) {
    const parameters = new URLSearchParams({
      worker: workerMode,
      source: 'supabase',
      supabaseUrl: supabaseMock.url,
    });
    const result = await exerciseSupabaseWithoutMigration(
      page,
      `${baseUrl}/?${parameters}`,
      runtimeRequests,
    );
    console.log(
      `SUPABASE_${workerMode.toUpperCase().replace('-', '_')}_WORKER_OK ${JSON.stringify(result)}`,
    );
  }
  if (supabaseMock.restRequests !== 4 || supabaseMock.joins !== 2) {
    throw new Error(
      `Expected two complete packed Supabase snapshots and joins, received ${JSON.stringify(supabaseMock)}`,
    );
  }
  supabaseMock.throwIfFailed();

  for (const workerMode of ['app-local', 'default']) {
    const databaseName = `packed-${workerMode}-${Date.now()}`;
    const parameters = new URLSearchParams({
      worker: workerMode,
      database: databaseName,
    });
    const written = await exerciseWithMigration(
      page,
      `${baseUrl}/?${parameters}&persistence=write`,
      runtimeRequests,
    );
    const restored = await exercisePersistedWithMigration(
      page,
      `${baseUrl}/?${parameters}&persistence=read`,
      runtimeRequests,
    );
    console.log(
      `OPFS_${workerMode.toUpperCase().replace('-', '_')}_WORKER_OK ${JSON.stringify({written, restored})}`,
    );
  }
  completed = true;
} finally {
  const cleanup = await Promise.allSettled([
    browser?.close(),
    supabaseMock?.close(),
    server ? stopServer(server) : undefined,
  ]);
  if (completed) {
    const failure = cleanup.find((result) => result.status === 'rejected');
    if (failure?.status === 'rejected') {
      throw failure.reason;
    }
  }
}

console.log(`PACKED_TARBALL ${relative(root, tarball)}`);
console.log(`PACKED_INSTALL ${relative(root, installedRealPath)}`);
console.log('SSR_IMPORT_OK');
console.log(`VITE_BUILD_OK ${builtFiles.length} files`);

await rm(generatedRoot, {force: true, recursive: true});
process.removeListener('exit', cleanupGeneratedRoot);

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    env: process.env,
    maxBuffer: 20 * 1024 * 1024,
  });
  const output = `${result.stdout ?? ''}${result.stderr ?? ''}`;
  if (result.status !== 0) {
    throw new Error(
      `${command} ${args.join(' ')} failed with ${result.status}:\n${output}`,
    );
  }
  return output;
}

async function assertBuildLibRejectsMissingMigrationArtifact() {
  const fixture = resolve(
    generatedRoot,
    'missing-migration-artifact',
  );
  const missing = 'wasm-migration/tinygres_migration_wasm_bg.wasm';
  await Promise.all(
    wasmArtifacts
      .filter((artifact) => artifact !== missing)
      .map(async (artifact) => {
        const path = resolve(fixture, artifact);
        await mkdir(dirname(path), {recursive: true});
        await writeFile(path, 'fixture');
      }),
  );
  try {
    await requireWasmArtifacts(fixture);
  } catch (error) {
    if (
      error instanceof Error &&
      error.message.includes('Missing default or migration WASM artifacts')
    ) {
      return;
    }
    throw error;
  }
  throw new Error('Build precondition accepted a missing migration artifact');
}

function parsePackOutput(output) {
  let parsed;
  try {
    parsed = JSON.parse(output);
  } catch {
    throw new Error(`Could not parse npm pack output:\n${output}`);
  }
  const entry = Array.isArray(parsed) ? parsed[0] : undefined;
  if (!entry || typeof entry.filename !== 'string') {
    throw new Error(`npm pack did not report a tarball filename:\n${output}`);
  }
  return entry;
}

function assertPackedFiles(packed) {
  const files = Array.isArray(packed.files)
    ? packed.files.map((file) => file.path)
    : [];
  for (const required of [
    'package.json',
    'index.js',
    'index.d.ts',
    'adapters/supabase/builtin.js',
    'adapters/supabase/native-realtime.js',
    'source-options.js',
    'worker/builtin-source.js',
    'worker/default-entry.js',
    'wasm/tinygres_wasm.js',
    'wasm/tinygres_wasm_bg.wasm',
    'wasm-migration/tinygres_migration_wasm.js',
    'wasm-migration/tinygres_migration_wasm_bg.wasm',
    'worker-migration/tinygres_migration_runtime.js',
  ]) {
    if (!files.includes(required)) {
      throw new Error(`Packed TinyGres is missing ${required}`);
    }
  }
  const nestedManifests = files.filter((file) => file.endsWith('/package.json'));
  if (nestedManifests.length > 0) {
    throw new Error(
      `Packed TinyGres contains nested package manifests: ${nestedManifests.join(', ')}`,
    );
  }
  for (const privateModule of [
    'worker/journal-codec.js',
    'worker/journal-payload.js',
    'worker/migration-engine.js',
    'worker/persistent-engine.js',
    'worker/snapshot-store.js',
  ]) {
    if (files.includes(privateModule)) {
      throw new Error(
        `Packed TinyGres exposes migration implementation module ${privateModule}`,
      );
    }
  }
}

async function assertConsumerMigrationBoundary(files) {
  const javascript = files.filter((file) => file.endsWith('.js'));
  const runtime = javascript.filter((file) =>
    /tinygres_migration_runtime(?:-[^/]*)?\.js$/.test(file),
  );
  const glue = javascript.filter((file) =>
    /tinygres_migration_wasm(?:-[^/]*)?\.js$/.test(file),
  );
  if (runtime.length !== 1 || glue.length !== 1) {
    throw new Error(
      `Consumer build did not emit one migration runtime and glue asset:\n${javascript.join('\n')}`,
    );
  }

  const implementationMarkers = [
    'A journal transaction payload cannot be empty',
    'The TinyGres persistent engine is closed',
    'TinyGres could not determine whether the final OPFS commit marker was durable',
    'Another TinyGres worker already has this OPFS database open',
  ];
  const runtimeSource = await readFile(runtime[0], 'utf8');
  for (const marker of implementationMarkers) {
    if (!runtimeSource.includes(marker)) {
      throw new Error(
        `Migration runtime is missing expected implementation marker: ${marker}`,
      );
    }
  }

  for (const file of javascript) {
    if (runtime.includes(file) || glue.includes(file)) {
      continue;
    }
    const source = await readFile(file, 'utf8');
    const leaked = implementationMarkers.find((marker) =>
      source.includes(marker),
    );
    if (leaked) {
      throw new Error(
        `Consumer JavaScript eagerly contains migration implementation code (${leaked}): ${file}`,
      );
    }
  }
}

async function listFiles(directory) {
  const entries = await readdir(directory, {withFileTypes: true});
  const files = [];
  for (const entry of entries) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listFiles(path)));
    } else {
      files.push(path);
    }
  }
  return files;
}

async function waitForServer(child, url, output) {
  const timeoutAt = Date.now() + 30_000;
  while (Date.now() < timeoutAt) {
    if (child.exitCode !== null) {
      throw new Error(`Vite preview exited early:\n${output()}`);
    }
    try {
      const response = await fetch(url);
      if (response.ok) {
        return;
      }
    } catch {
      // The preview server has not bound its port yet.
    }
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 100));
  }
  throw new Error(`Timed out waiting for Vite preview:\n${output()}`);
}

async function exercise(page, url) {
  await page.goto(url);
  await page
    .locator('body[data-status="passed"][data-closed="true"]')
    .waitFor({timeout: 30_000});
  const text = await page.locator('#result').textContent();
  const result = JSON.parse(text ?? 'null');
  if (
    result.initialRevision !== 1 ||
    result.initialTitle !== 'from packed snapshot' ||
    result.changedRevision !== 2 ||
    result.changedTitle !== 'from packed change'
  ) {
    throw new Error(`Unexpected packed-consumer result: ${text}`);
  }
  return result;
}

async function exerciseWithoutMigration(page, url, requests) {
  return captureMigrationRequests(requests, false, () => exercise(page, url));
}

async function exerciseWithMigration(page, url, requests) {
  return captureMigrationRequests(requests, true, () => exercise(page, url));
}

async function exercisePersisted(page, url) {
  await page.goto(url);
  await page
    .locator('body[data-status="passed"][data-closed="true"]')
    .waitFor({timeout: 30_000});
  const text = await page.locator('#result').textContent();
  const result = JSON.parse(text ?? 'null');
  if (result.revision !== 2 || result.title !== 'from packed change') {
    throw new Error(`Unexpected packed OPFS restart result: ${text}`);
  }
  return result;
}

async function exercisePersistedWithMigration(page, url, requests) {
  return captureMigrationRequests(requests, true, () =>
    exercisePersisted(page, url),
  );
}

async function exerciseSupabase(page, url) {
  await page.goto(url);
  await page
    .locator('body[data-status="passed"][data-closed="true"]')
    .waitFor({timeout: 30_000});
  const text = await page.locator('#result').textContent();
  const result = JSON.parse(text ?? 'null');
  if (
    result.phase !== 'live-best-effort' ||
    result.revision !== 1 ||
    result.title !== 'from packed Supabase snapshot'
  ) {
    throw new Error(`Unexpected packed Supabase result: ${text}`);
  }
  return result;
}

async function exerciseSupabaseWithoutMigration(page, url, requests) {
  return captureMigrationRequests(requests, false, () =>
    exerciseSupabase(page, url),
  );
}

async function captureMigrationRequests(requests, expected, exerciseRuntime) {
  const firstRequest = requests.length;
  const result = await exerciseRuntime();
  const capturedRequests = requests.slice(firstRequest);
  const migrationRequests = capturedRequests.filter((url) =>
    isMigrationRequest(url),
  );
  const requestedRuntime = migrationRequests.some((url) =>
    /tinygres_migration_runtime(?:-[^/]*)?\.js(?:\?|$)/.test(url),
  );
  const requestedGlue = migrationRequests.some((url) =>
    /tinygres_migration_wasm(?:-[^/]*)?\.js(?:\?|$)/.test(url),
  );
  const requestedWasm = migrationRequests.some((url) =>
    /tinygres_migration_wasm_bg(?:-[^/]+)?\.wasm(?:\?|$)/.test(url),
  );
  if (expected && (!requestedRuntime || !requestedGlue || !requestedWasm)) {
    throw new Error(
      `Persistent packed runtime did not lazily load migration runtime, glue, and WASM: ${capturedRequests.join(', ')}`,
    );
  }
  if (!expected && migrationRequests.length > 0) {
    throw new Error(
      `Memory packed runtime unexpectedly loaded migration artifacts: ${migrationRequests.join(', ')}`,
    );
  }
  return result;
}

function isMigrationRequest(url) {
  return (
    url.includes('tinygres_migration_runtime') ||
    url.includes('/worker-migration/') ||
    url.includes('tinygres_migration_wasm') ||
    url.includes('migration-engine') ||
    url.includes('persistent-engine') ||
    url.includes('snapshot-store')
  );
}

async function startSupabaseMock() {
  const state = {errors: [], joins: 0, restRequests: 0};
  const server = createServer((request, response) => {
    const url = new URL(request.url ?? '/', 'http://127.0.0.1');
    if (request.method === 'OPTIONS') {
      respondCors(response, 204);
      return;
    }
    try {
      if (request.method !== 'GET' || url.pathname !== '/rest/v1/posts') {
        throw new Error(`Unexpected packed Supabase HTTP request: ${request.method} ${url}`);
      }
      state.restRequests += 1;
      const headers = request.headers;
      const firstPage = headers.range === '0-499';
      const terminatingPage = headers.range === '1-500';
      if (
        headers.apikey !== 'sb_publishable_packed_test' ||
        headers.authorization !== undefined ||
        headers['accept-profile'] !== 'public' ||
        (!firstPage && !terminatingPage) ||
        url.searchParams.get('select') !== 'id,title' ||
        url.searchParams.get('order') !== 'id.asc'
      ) {
        throw new Error(
          `Unexpected packed Supabase snapshot request: ${url} ${JSON.stringify(headers)}`,
        );
      }
      respondCors(
        response,
        200,
        JSON.stringify(
          firstPage
            ? [{id: 1, title: 'from packed Supabase snapshot'}]
            : [],
        ),
      );
    } catch (error) {
      state.errors.push(error);
      respondCors(response, 500, JSON.stringify({message: String(error)}));
    }
  });
  const webSockets = new WebSocketServer({noServer: true});
  server.on('upgrade', (request, socket, head) => {
    const url = new URL(request.url ?? '/', 'http://127.0.0.1');
    if (url.pathname !== '/realtime/v1/websocket') {
      socket.destroy();
      return;
    }
    webSockets.handleUpgrade(request, socket, head, (webSocket) => {
      webSockets.emit('connection', webSocket, request);
    });
  });
  webSockets.on('connection', (socket, request) => {
    try {
      const endpoint = new URL(request.url ?? '/', 'http://127.0.0.1');
      if (
        endpoint.searchParams.get('apikey') !==
          'sb_publishable_packed_test' ||
        endpoint.searchParams.get('vsn') !== '1.0.0'
      ) {
        throw new Error(`Unexpected packed Supabase Realtime URL: ${endpoint}`);
      }
    } catch (error) {
      state.errors.push(error);
      socket.close(1008, 'Invalid test connection');
      return;
    }
    socket.on('message', (message) => {
      try {
        const frame = JSON.parse(String(message));
        if (frame.event === 'heartbeat') {
          socket.send(
            JSON.stringify({
              topic: 'phoenix',
              event: 'phx_reply',
              payload: {status: 'ok', response: {}},
              ref: frame.ref,
            }),
          );
          return;
        }
        if (frame.event !== 'phx_join') {
          return;
        }
        state.joins += 1;
        const subscriptions = frame.payload?.config?.postgres_changes;
        if (
          !Array.isArray(subscriptions) ||
          subscriptions.length !== 1 ||
          frame.payload?.config?.broadcast?.replication_ready !== true ||
          subscriptions[0]?.schema !== 'public' ||
          subscriptions[0]?.table !== 'posts' ||
          JSON.stringify(subscriptions[0]?.select) !==
            JSON.stringify(['id', 'title'])
        ) {
          throw new Error(
            `Unexpected packed Supabase join: ${JSON.stringify(frame)}`,
          );
        }
        socket.send(
          JSON.stringify({
            topic: frame.topic,
            event: 'phx_reply',
            payload: {
              status: 'ok',
              response: {
                postgres_changes: [
                  {id: 101, event: '*', schema: 'public', table: 'posts'},
                ],
              },
            },
            ref: frame.ref,
            join_ref: frame.ref,
          }),
        );
        for (const payload of [
          {
            status: 'ok',
            extension: 'postgres_changes',
            message: 'Subscribed to PostgreSQL',
          },
          {
            status: 'ok',
            extension: 'system',
            message: 'Replication connection established',
          },
        ]) {
          socket.send(
            JSON.stringify({
              topic: frame.topic,
              event: 'system',
              payload,
              ref: null,
              join_ref: frame.ref,
            }),
          );
        }
      } catch (error) {
        state.errors.push(error);
        socket.close(1011, 'Invalid test frame');
      }
    });
  });
  await new Promise((resolvePromise, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolvePromise);
  });
  const address = server.address();
  if (!address || typeof address === 'string') {
    throw new Error('Packed Supabase server did not bind an IPv4 port');
  }
  return {
    get joins() {
      return state.joins;
    },
    get restRequests() {
      return state.restRequests;
    },
    url: `http://127.0.0.1:${address.port}`,
    throwIfFailed() {
      if (state.errors.length > 0) {
        throw state.errors[0];
      }
    },
    async close() {
      for (const socket of webSockets.clients) {
        socket.terminate();
      }
      await new Promise((resolvePromise) => webSockets.close(resolvePromise));
      await new Promise((resolvePromise, reject) =>
        server.close((error) => (error ? reject(error) : resolvePromise())),
      );
    },
  };
}

function respondCors(response, status, body = '') {
  response.writeHead(status, {
    'Access-Control-Allow-Headers':
      'accept-profile, apikey, authorization, range, range-unit',
    'Access-Control-Allow-Methods': 'GET, OPTIONS',
    'Access-Control-Allow-Origin': '*',
    'Content-Type': 'application/json',
  });
  response.end(body);
}

async function stopServer(child) {
  if (child.exitCode !== null) {
    return;
  }
  await new Promise((resolvePromise) => {
    const timeout = setTimeout(() => {
      child.kill('SIGKILL');
      resolvePromise();
    }, 5_000);
    timeout.unref();
    child.once('exit', () => {
      clearTimeout(timeout);
      resolvePromise();
    });
    child.kill('SIGTERM');
  });
}
