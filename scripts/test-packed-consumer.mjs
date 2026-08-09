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
      `Published Tinygres manifest contains development field ${developmentField}`,
    );
  }
}

const ssrOutput = run(
  process.execPath,
  [
    '--input-type=module',
    '--eval',
    "const pkg = await import('tinygres'); if (typeof pkg.createTinygresClient !== 'function') throw new Error('missing client export'); console.log('SSR_IMPORT_OK');",
  ],
  appDirectory,
);
if (!ssrOutput.includes('SSR_IMPORT_OK')) {
  throw new Error(`SSR-safe import did not complete:\n${ssrOutput}`);
}

run(npm, ['run', 'typecheck'], appDirectory);
run(npm, ['run', 'build'], appDirectory);
const builtFiles = await listFiles(resolve(appDirectory, 'dist'));
if (!builtFiles.some((file) => file.endsWith('.wasm'))) {
  throw new Error(`The consumer build emitted no WASM asset:\n${builtFiles.join('\n')}`);
}

const server = spawn(
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
let serverOutput = '';
server.stdout.on('data', (chunk) => {
  serverOutput += String(chunk);
});
server.stderr.on('data', (chunk) => {
  serverOutput += String(chunk);
});

let browser;
try {
  await waitForServer(server, baseUrl, () => serverOutput);
  browser = await chromium.launch({headless: true});
  const page = await browser.newPage();

  const appLocal = await exercise(page, `${baseUrl}/?worker=app-local`);
  console.log(`APP_LOCAL_WORKER_OK ${JSON.stringify(appLocal)}`);

  const packageDefault = await exercise(page, `${baseUrl}/?worker=default`);
  console.log(`PACKAGE_DEFAULT_WORKER_OK ${JSON.stringify(packageDefault)}`);
} finally {
  await browser?.close();
  await stopServer(server);
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
  ]) {
    if (!files.includes(required)) {
      throw new Error(`Packed Tinygres is missing ${required}`);
    }
  }
  const nestedManifests = files.filter((file) => file.endsWith('/package.json'));
  if (nestedManifests.length > 0) {
    throw new Error(
      `Packed Tinygres contains nested package manifests: ${nestedManifests.join(', ')}`,
    );
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
  await page.locator('body[data-status="passed"]').waitFor({timeout: 30_000});
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
