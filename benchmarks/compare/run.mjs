import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {existsSync} from 'node:fs';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {createServer} from 'node:http';
import {arch, cpus, platform, release, tmpdir, totalmem} from 'node:os';
import {extname, join, resolve, sep} from 'node:path';
import {fileURLToPath} from 'node:url';
import {parseArgs} from 'node:util';
import {brotliCompressSync, constants, gzipSync} from 'node:zlib';

// Compares TinyJoin with SQLite (opfs-sahpool) and PGlite (opfs-ahp), each
// running in a Worker and storing to OPFS in a real Chromium profile. Every
// sample gets a fresh browser profile on disk, so storage is never shared, and
// a hung sample can be killed without affecting the next.
// Diagnostic distributions, never a CI timing threshold. Turso can be added
// with --engines for local comparison; it is never published.

const here = fileURLToPath(new URL('.', import.meta.url));
const root = resolve(here, '../..');
const ENGINES = ['tinyjoin', 'sqlite', 'pglite'];
const OPTIONAL_ENGINES = ['turso'];
const KNOWN_ENGINES = [...ENGINES, ...OPTIONAL_ENGINES];
const PACKAGES = {
  tinyjoin: 'tinyjoin',
  sqlite: '@sqlite.org/sqlite-wasm',
  pglite: '@electric-sql/pglite',
  turso: '@tursodatabase/database-wasm',
};
// How each engine stores to OPFS, as the report names it.
const STORAGE = {tinyjoin: 'opfs://', sqlite: 'opfs-sahpool', pglite: 'opfs-ahp://', turso: 'OPFS sync access handles'};
// Turso's threaded WebAssembly needs SharedArrayBuffer, so its page is served
// cross-origin isolated, from a second origin that only it uses.
const ISOLATED = new Set(['turso']);
// The results are published for the benchmarks guide to chart.
const PUBLISHED = resolve(root, 'site/data/benchmarks.json');

const {values: options} = parseArgs({
  options: {
    engines: {type: 'string', default: ENGINES.join(',')},
    workloads: {type: 'string'},
    samples: {type: 'string', default: '5'},
    timeout: {type: 'string', default: '60'},
    out: {type: 'string'},
    publish: {type: 'boolean', default: false},
    help: {type: 'boolean', default: false},
  },
});
if (options.help) {
  console.log(`Usage: node benchmarks/compare/run.mjs [options]

  --engines a,b      ${KNOWN_ENGINES.join(', ')} (default: ${ENGINES.join(',')})
  --workloads a,b    workload ids, including cold-open and reopen (default: all)
  --samples n        samples per engine and workload (default: 5)
  --timeout s        seconds before a sample is abandoned (default: 60)
  --out file         write the full JSON report
  --publish          full default run, written to
                     site/data/benchmarks.json`);
  process.exit(0);
}

const {workloads, ROWS} = await import('./app/workloads.js');
const STARTUP = [
  {id: 'cold-open', group: 'Startup', label: 'Load, create an empty database, and read from it'},
  {id: 'reopen', group: 'Startup', label: `Reopen a ${ROWS.toLocaleString('en-US')}-row database in a new session and count it`},
];
const ALL = [...STARTUP, ...workloads.map(({id, group, label, derivedFrom}) => ({id, group, label, derivedFrom}))];

const engines = options.engines.split(',');
const selected = options.workloads ? options.workloads.split(',') : ALL.map(({id}) => id);
const samples = Number(options.samples);
const timeoutMs = Number(options.timeout) * 1000;
for (const engine of engines) if (!KNOWN_ENGINES.includes(engine)) throw new Error(`Unknown engine: ${engine}`);
for (const id of selected) if (!ALL.some((workload) => workload.id === id)) throw new Error(`Unknown workload: ${id}`);
if (!(samples >= 1) || !(timeoutMs > 0)) throw new Error('--samples and --timeout must be positive');
if (options.publish && (options.workloads || options.engines !== ENGINES.join(',') || samples < 5 || options.out)) {
  throw new Error('--publish takes the full default suite: all engines and workloads, at least 5 samples, and no --out');
}

// Byte sizes depend on the zlib bundled with Node, so published sizes use the
// same pinned Node as site/data/sizes.json.
const pinnedNode = (await readFile(resolve(root, '.node-version'), 'utf8')).trim();
if (process.version !== `v${pinnedNode}`) {
  const message = `Node ${process.version} differs from .node-version (${pinnedNode}); download sizes may differ slightly.`;
  if (options.publish) throw new Error(message);
  console.warn(message);
}

// The competitors are installed in this directory, not the repository root,
// so their exact versions are pinned by this directory's lockfile.
if (!KNOWN_ENGINES.slice(1).every((engine) => existsSync(resolve(here, 'node_modules', PACKAGES[engine], 'package.json')))) {
  console.log('Installing comparison engines...');
  execFileSync('npm', ['ci', '--no-audit', '--no-fund'], {cwd: here, stdio: 'inherit'});
}
if (!existsSync(resolve(root, 'dist/index.js'))) throw new Error('Build TinyJoin first: npm run build');

// Published results must measure the runtime the committed sources build, not a
// leftover experimental or stale build. Uncompressed sizes do not depend on
// zlib, so they must match site/data/sizes.json exactly.
if (options.publish) {
  const {measureSizes, readSizes} = await import(new URL('../../scripts/sizes.mjs', import.meta.url).href);
  const [measured, committed] = await Promise.all([measureSizes(), readSizes()]);
  const differing = Object.keys(measured).filter((group) => measured[group].raw !== committed[group]?.raw);
  if (differing.length) {
    throw new Error(
      `--publish measures dist/, but its ${differing.join(', ')} sizes differ from site/data/sizes.json. ` +
        'Run npm run build from the committed sources first.',
    );
  }
}
const version = async (engine) =>
  JSON.parse(await readFile(engine === 'tinyjoin' ? resolve(root, 'dist/package.json') : resolve(here, 'node_modules', PACKAGES[engine], 'package.json'), 'utf8')).version;
const git = (...args) => execFileSync('git', args, {cwd: root, encoding: 'utf8'}).trim();

// One Vite production build holds every engine, each in its own chunk.
const out = resolve(here, '.build');
const {build} = await import('vite');
await build({configFile: resolve(here, 'vite.config.mjs'), logLevel: 'warn'});
const manifest = JSON.parse(await readFile(resolve(out, '.vite/manifest.json'), 'utf8'));
const harnessFile = manifest['index.html'].file;

const TYPES = {'.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.data': 'application/octet-stream'};
const serve = (isolated) => async (request, response) => {
  try {
    const pathname = new URL(request.url, 'http://127.0.0.1').pathname;
    const path = resolve(out, `.${pathname === '/' ? '/index.html' : pathname}`);
    if (!path.startsWith(`${out}${sep}`)) throw new Error('Invalid asset path');
    const body = await readFile(path);
    response.setHeader('Content-Type', TYPES[extname(path)] ?? 'application/octet-stream');
    // Hashed assets may be cached, as a deployed application's would be;
    // the page itself is always revalidated.
    response.setHeader('Cache-Control', pathname === '/' ? 'no-cache' : 'public, max-age=31536000, immutable');
    if (isolated) {
      response.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
      response.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
    }
    response.end(body);
  } catch (error) {
    response.statusCode = 404;
    response.end(String(error));
  }
};
const listen = async (isolated) => {
  const server = createServer(serve(isolated));
  await new Promise((resolveListen, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolveListen);
  });
  return server;
};
const servers = [await listen(false), await listen(true)];
const [origin, isolatedOrigin] = servers.map((server) => `http://127.0.0.1:${server.address().port}`);

const {chromium} = await import('@playwright/test');
const report = {
  measuredAt: new Date().toISOString(),
  storage: 'opfs',
  samples,
  timeoutMs,
  environment: {
    node: process.version,
    os: platform(),
    osRelease: release(),
    architecture: arch(),
    cpu: cpus()[0]?.model,
    logicalCpus: cpus().length,
    memoryBytes: totalmem(),
    headless: true,
    network: 'loopback HTTP; hashed assets cacheable',
  },
  engines: {},
  download: {},
  results: [],
};
for (const engine of engines) {
  report.engines[engine] = {package: PACKAGES[engine], version: await version(engine), storage: STORAGE[engine]};
}
if (report.engines.tinyjoin) {
  report.engines.tinyjoin.commit = git('rev-parse', '--short', 'HEAD');
  report.engines.tinyjoin.dirty = git('status', '--porcelain', '--', 'src', 'crates').length > 0;
}

const compress = (bytes) => ({
  raw: bytes.length,
  gzip: gzipSync(bytes, {level: 9}).length,
  brotli: brotliCompressSync(bytes, {params: {[constants.BROTLI_PARAM_QUALITY]: 11}}).length,
});

async function session(engine, profile, callback) {
  const context = await chromium.launchPersistentContext(profile, {headless: true});
  const errors = [];
  const fetched = new Set();
  try {
    report.environment.browserVersion ??= context.browser()?.version();
    const page = context.pages()[0] ?? (await context.newPage());
    page.on('pageerror', (error) => errors.push(String(error)));
    page.on('console', (message) => message.type() === 'error' && errors.push(message.text()));
    page.on('requestfinished', (request) => fetched.add(new URL(request.url()).pathname));
    await page.goto(ISOLATED.has(engine) ? isolatedOrigin : origin);
    await page.waitForFunction(() => window.benchReady);
    report.environment.userAgent ??= await page.evaluate(() => navigator.userAgent);
    let timer;
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => reject(Object.assign(new Error('timeout'), {timedOut: true})), timeoutMs);
    });
    try {
      return await Promise.race([callback(page, fetched), timeout]);
    } finally {
      clearTimeout(timer);
    }
  } catch (error) {
    if (errors.length) error.message += `\n  ${errors.join('\n  ')}`;
    throw error;
  } finally {
    await context.close().catch(() => {});
  }
}

// Everything the page fetched for the engine: not the page or the harness.
// Brotli at quality 11 is slow on PGlite's assets, so each file is measured once.
const measured = new Map();
async function measureDownload(fetched) {
  const files = [];
  for (const pathname of [...fetched].sort()) {
    const file = pathname.slice(1);
    if (pathname === '/' || file === harnessFile) continue;
    if (!measured.has(file)) measured.set(file, compress(await readFile(resolve(out, file))));
    files.push({file, ...measured.get(file)});
  }
  const total = (key) => files.reduce((sum, file) => sum + file[key], 0);
  return {raw: total('raw'), gzip: total('gzip'), brotli: total('brotli'), files};
}

async function sample(engine, id) {
  const profile = await mkdtemp(join(tmpdir(), `tinyjoin-compare-${engine}-`));
  try {
    if (id === 'cold-open') {
      return await session(engine, profile, async (page, fetched) => {
        const result = await page.evaluate((engine) => window.bench.coldOpen(engine), engine);
        return {...result, download: await measureDownload(fetched)};
      });
    }
    if (id === 'reopen') {
      await session(engine, profile, (page) => page.evaluate((engine) => window.bench.seedReopen(engine), engine));
      return await session(engine, profile, (page) => page.evaluate((engine) => window.bench.reopen(engine), engine));
    }
    return await session(engine, profile, (page) => page.evaluate(({engine, id}) => window.bench.run(engine, id), {engine, id}));
  } finally {
    await rm(profile, {recursive: true, force: true});
  }
}

const distribution = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length / 2;
  const median = sorted.length % 2 ? sorted[Math.floor(middle)] : (sorted[middle - 1] + sorted[middle]) / 2;
  const round = (value) => Math.round(value * 100) / 100;
  return {median: round(median), min: round(sorted[0]), max: round(sorted.at(-1))};
};

const formatMs = (ms) => (ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${ms.toFixed(ms < 10 ? 2 : 1)} ms`);
const cell = (result) =>
  result == null ? '' : result.timedOut ? `> ${formatMs(timeoutMs)}` : result.error ? 'error' : formatMs(result.median);

const save = async () => {
  const text = `${JSON.stringify(report, null, 2)}\n`;
  if (options.publish) await writeFile(PUBLISHED, text);
  if (options.out) await writeFile(resolve(options.out), text);
};

try {
  for (const workload of ALL.filter(({id}) => selected.includes(id))) {
    const entry = {...workload, engines: {}};
    report.results.push(entry);
    const abandoned = new Set();
    for (const engine of engines) entry.engines[engine] = {samples: []};
    // Engines alternate within each round, so drift affects them equally.
    for (let round = 0; round < samples; round++) {
      for (const engine of engines.filter((engine) => !abandoned.has(engine))) {
        const result = entry.engines[engine];
        try {
          const {ms, info, check, download} = await sample(engine, workload.id);
          result.samples.push(Math.round(ms * 100) / 100);
          result.check = check;
          if (info?.engineVersion) report.engines[engine].engineVersion = info.engineVersion;
          if (download) report.download[engine] = download;
        } catch (error) {
          // Later samples would be abandoned too, so each failure is recorded once.
          abandoned.add(engine);
          if (error.timedOut) result.timedOut = true;
          else result.error = error.message.split('\n')[0];
          console.warn(`  ${engine} ${workload.id}: ${error.timedOut ? `timed out after ${formatMs(timeoutMs)}` : error.message}`);
        }
      }
    }
    for (const engine of engines) {
      const result = entry.engines[engine];
      if (result.samples.length && !result.timedOut && !result.error) Object.assign(result, distribution(result.samples));
    }
    const checks = engines.map((engine) => entry.engines[engine].check).filter(Boolean).map((check) => JSON.stringify(check));
    entry.checksAgree = new Set(checks).size <= 1;
    if (!entry.checksAgree) console.warn(`  ${workload.id}: engines disagree: ${checks.join(' / ')}`);
    console.log(`${workload.id.padEnd(20)} ${engines.map((engine) => `${engine} ${cell(entry.engines[engine])}`.padEnd(24)).join('')}`);
    await save();
  }
} finally {
  for (const server of servers) server.close();
}

const kib = (bytes) => `${(bytes / 1024).toLocaleString('en-US', {maximumFractionDigits: 0})} KiB`;
console.log('\nDownload (gzip):', engines.filter((engine) => report.download[engine]).map((engine) => `${engine} ${kib(report.download[engine].gzip)}`).join(', ') || 'not measured');
console.log('\nMedians, and TinyJoin relative to the fastest engine:\n');
console.table(Object.fromEntries(report.results.map((entry) => {
  const row = Object.fromEntries(engines.map((engine) => [engine, cell(entry.engines[engine])]));
  const medians = engines.map((engine) => entry.engines[engine].median).filter((median) => median != null);
  const tinyjoin = entry.engines.tinyjoin?.median;
  if (tinyjoin != null && medians.length > 1) row['tinyjoin ×'] = `${(tinyjoin / Math.min(...medians)).toFixed(1)}×`;
  if (!entry.checksAgree) row.checks = 'DISAGREE';
  return [entry.id, row];
})));
if (report.results.some((entry) => !entry.checksAgree)) {
  process.exitCode = 1;
  console.error('Engines returned different results for at least one workload; see above.');
}
if (options.publish) console.log(`\nWrote ${PUBLISHED}. Run npm run build:docs to update the site.`);
