import {spawn, spawnSync} from 'node:child_process';
import {rmSync} from 'node:fs';
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

import {requireWasmArtifacts, wasmArtifacts} from './wasm-artifacts.mjs';

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
await assertBuildLibRejectsMissingWasmArtifacts();
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
const nodeModulesRealPath = await realpath(
  resolve(appDirectory, 'node_modules'),
);
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
await assertInstalledOpfsLoader(installedPackage);

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
await assertConsumerPrivateRuntimeBoundary(builtFiles);
if (!builtFiles.some((file) => file.endsWith('.wasm'))) {
  throw new Error(
    `The consumer build emitted no WASM asset:\n${builtFiles.join('\n')}`,
  );
}

let server;
let serverOutput = '';
let browser;
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
  await waitForServer(server, baseUrl, () => serverOutput);
  browser = await chromium.launch({headless: true});
  const page = await browser.newPage();
  // Routing disables Chromium's HTTP cache. Every lazy module/WASM load is
  // therefore observable on the exact navigation that requested it.
  await page.route('**/*', (route) => route.continue());
  const runtimeRequests = [];
  page.on('request', (request) => runtimeRequests.push(request.url()));

  const appLocal = await exerciseMemory(
    page,
    `${baseUrl}/?worker=app-local`,
    runtimeRequests,
  );
  console.log(`APP_LOCAL_WORKER_OK ${JSON.stringify(appLocal)}`);

  const packageDefault = await exerciseMemory(
    page,
    `${baseUrl}/?worker=default`,
    runtimeRequests,
  );
  console.log(`PACKAGE_DEFAULT_WORKER_OK ${JSON.stringify(packageDefault)}`);

  for (const workerMode of ['app-local', 'default']) {
    const freshDatabaseName = `packed-fresh-${workerMode}-${Date.now()}`;
    const freshParameters = new URLSearchParams({
      worker: workerMode,
      database: freshDatabaseName,
    });
    const written = await exerciseFreshOpfs(
      page,
      `${baseUrl}/?${freshParameters}&persistence=write`,
      runtimeRequests,
    );
    const restored = await exercisePersistedOpfs(
      page,
      `${baseUrl}/?${freshParameters}&persistence=read`,
      runtimeRequests,
    );
    console.log(
      `OPFS_FRESH_${workerMode.toUpperCase().replace('-', '_')}_WORKER_OK ${JSON.stringify({written, restored})}`,
    );
  }
  completed = true;
} finally {
  const cleanup = await Promise.allSettled([
    browser?.close(),
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

async function assertBuildLibRejectsMissingWasmArtifacts() {
  for (const missing of wasmArtifacts) {
    const fixture = resolve(
      generatedRoot,
      `missing-${missing.replaceAll('/', '-')}`,
    );
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
        error.message.includes('Missing TinyGres WASM artifacts')
      ) {
        continue;
      }
      throw error;
    }
    throw new Error(`Build precondition accepted missing artifact ${missing}`);
  }
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
    'worker/default-entry.js',
    'wasm/tinygres_wasm.js',
    'wasm/tinygres_wasm_bg.wasm',
    'worker-opfs/tinygres_opfs_runtime.js',
    'worker/opfs-loader.js',
  ]) {
    if (!files.includes(required)) {
      throw new Error(`Packed TinyGres is missing ${required}`);
    }
  }
  const integrationFiles = files.filter(
    (file) =>
      file.startsWith('adapters/') ||
      file === 'source-options.js' ||
      file === 'source-options.d.ts' ||
      file === 'worker/builtin-source.js' ||
      file === 'worker/builtin-source.d.ts',
  );
  if (integrationFiles.length > 0) {
    throw new Error(
      `Packed TinyGres contains integration modules: ${integrationFiles.join(', ')}`,
    );
  }
  const nestedManifests = files.filter((file) =>
    file.endsWith('/package.json'),
  );
  if (nestedManifests.length > 0) {
    throw new Error(
      `Packed TinyGres contains nested package manifests: ${nestedManifests.join(', ')}`,
    );
  }
  for (const privateModule of ['opfs-engine', 'page-storage']) {
    for (const extension of ['js', 'd.ts']) {
      const path = `worker/${privateModule}.${extension}`;
      if (files.includes(path)) {
        throw new Error(
          `Packed TinyGres exposes private storage implementation module ${path}`,
        );
      }
    }
  }
  const packagedWasm = files.filter((file) => file.endsWith('.wasm'));
  if (
    packagedWasm.length !== 1 ||
    packagedWasm[0] !== 'wasm/tinygres_wasm_bg.wasm'
  ) {
    throw new Error(
      `Packed TinyGres must contain exactly its page WASM: ${packagedWasm.join(', ')}`,
    );
  }
  const privateRuntimeAssets = files.filter((file) =>
    file.startsWith('worker-'),
  );
  if (
    privateRuntimeAssets.length !== 1 ||
    privateRuntimeAssets[0] !== 'worker-opfs/tinygres_opfs_runtime.js'
  ) {
    throw new Error(
      `Packed TinyGres must contain exactly its OPFS runtime: ${privateRuntimeAssets.join(', ')}`,
    );
  }
}

async function assertInstalledOpfsLoader(packageDirectory) {
  const path = resolve(packageDirectory, 'worker/opfs-loader.js');
  const source = await readFile(path, 'utf8');
  for (const marker of [
    "'../worker-opfs/tinygres_opfs_runtime.js'",
    '/* @vite-ignore */',
    '/* webpackIgnore: true */',
  ]) {
    if (!source.includes(marker)) {
      throw new Error(`Packed OPFS loader is missing ${marker}: ${path}`);
    }
  }
}

async function assertConsumerPrivateRuntimeBoundary(files) {
  const javascript = files.filter((file) => file.endsWith('.js'));
  const wasm = files.filter((file) => file.endsWith('.wasm'));
  const opfsRuntime = javascript.filter((file) =>
    /tinygres_opfs_runtime(?:-[^/]*)?\.js$/.test(file),
  );
  const pageWasm = wasm.filter((file) =>
    /tinygres_wasm_bg(?:-[^/]*)?\.wasm$/.test(file),
  );
  if (opfsRuntime.length !== 1 || pageWasm.length !== 1 || wasm.length !== 1) {
    throw new Error(
      `Consumer build did not emit exactly one OPFS runtime and one page WASM:\n${files.join('\n')}`,
    );
  }

  const opfsImplementationMarkers = [
    'createOpfsWasmEngine',
    'Another TinyGres worker already has this OPFS database open',
  ];
  const privateOpfsImplementationMarkers = [
    'Another TinyGres worker already has this OPFS database open',
  ];
  const opfsRuntimeSource = await readFile(opfsRuntime[0], 'utf8');
  if (Buffer.byteLength(opfsRuntimeSource) > 96 * 1024) {
    throw new Error(
      `Consumer OPFS runtime exceeds its 96 KiB raw gate: ${opfsRuntime[0]}`,
    );
  }
  for (const marker of opfsImplementationMarkers) {
    if (!opfsRuntimeSource.includes(marker)) {
      throw new Error(
        `OPFS runtime is missing expected implementation marker: ${marker}`,
      );
    }
  }
  for (const [pattern, description] of [
    [/\bimport\s*\(/, 'a dynamic import'],
    [/\bnew URL\s*\(/, 'an unresolved asset URL'],
    [
      /data:(?:application\/wasm|text\/javascript)/i,
      'an inlined runtime or WASM data URL',
    ],
    [/tinygres_wasm(?:_bg)?/i, 'default WASM glue'],
    [/WASM returned an invalid binary response/, 'the WASM wire adapter'],
  ]) {
    if (pattern.test(opfsRuntimeSource)) {
      throw new Error(
        `OPFS runtime eagerly contains ${description}: ${opfsRuntime[0]}`,
      );
    }
  }

  for (const file of javascript) {
    if (opfsRuntime.includes(file)) {
      continue;
    }
    const source = await readFile(file, 'utf8');
    const leaked = privateOpfsImplementationMarkers.find((marker) =>
      source.includes(marker),
    );
    if (leaked) {
      throw new Error(
        `Consumer JavaScript eagerly contains OPFS implementation code (${leaked}): ${file}`,
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
  const text = await waitForConsumerResult(page, 'packed consumer');
  const result = JSON.parse(text ?? 'null');
  if (
    result.initialRevision !== 2 ||
    result.initialTitle !== 'from packed insert' ||
    result.changedRevision !== 3 ||
    result.changedTitle !== 'from packed update'
  ) {
    throw new Error(`Unexpected packed-consumer result: ${text}`);
  }
  return result;
}

async function exerciseMemory(page, url, requests) {
  return captureLazyRequests(requests, {opfs: false}, () =>
    exercise(page, url),
  );
}

async function exerciseFreshOpfs(page, url, requests) {
  return captureLazyRequests(requests, {opfs: true}, () => exercise(page, url));
}

async function exercisePersisted(page, url) {
  await page.goto(url);
  const text = await waitForConsumerResult(page, 'packed OPFS restart');
  const result = JSON.parse(text ?? 'null');
  if (result.revision !== 3 || result.title !== 'from packed update') {
    throw new Error(`Unexpected packed OPFS restart result: ${text}`);
  }
  return result;
}

async function exercisePersistedOpfs(page, url, requests) {
  return captureLazyRequests(requests, {opfs: true}, () =>
    exercisePersisted(page, url),
  );
}

async function waitForConsumerResult(page, description) {
  await page
    .locator(
      'body[data-status="passed"][data-closed="true"], body[data-status="failed"]',
    )
    .waitFor({timeout: 30_000});
  const [status, text] = await Promise.all([
    page.locator('body').getAttribute('data-status'),
    page.locator('#result').textContent(),
  ]);
  if (status === 'failed') {
    throw new Error(
      `${description} failed: ${text ?? 'unknown browser error'}`,
    );
  }
  return text;
}

async function captureLazyRequests(requests, expected, exerciseRuntime) {
  const firstRequest = requests.length;
  const result = await exerciseRuntime();
  const capturedRequests = requests.slice(firstRequest);
  const opfsRequests = capturedRequests.filter((url) =>
    isOpfsRuntimeRequest(url),
  );
  if (expected.opfs && opfsRequests.length !== 1) {
    throw new Error(
      `Packed OPFS runtime did not lazily load exactly one runtime chunk: ${capturedRequests.join(', ')}`,
    );
  }
  if (!expected.opfs && opfsRequests.length > 0) {
    throw new Error(
      `Memory packed runtime unexpectedly loaded OPFS code: ${opfsRequests.join(', ')}`,
    );
  }
  return result;
}

function isOpfsRuntimeRequest(url) {
  return url.includes('tinygres_opfs_runtime') || url.includes('/worker-opfs/');
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
