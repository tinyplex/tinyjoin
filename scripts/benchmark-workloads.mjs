import {createHash} from 'node:crypto';
import {createServer} from 'node:http';
import {readFile, readdir, writeFile} from 'node:fs/promises';
import {arch, cpus, platform, release, totalmem} from 'node:os';
import {extname, resolve, sep} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium} from '@playwright/test';

// Build first. Diagnostic distributions, never a CI timing threshold.
// Each sample owns a fresh browser context and closes all clients and OPFS files.
const root = fileURLToPath(new URL('../', import.meta.url));
const dist = resolve(root, 'dist');
const args = process.argv.slice(2);
const outputs = args.filter(argument => !argument.startsWith('--'));
const output = outputs[0];
const resume = args.includes('--resume');
const quick = args.includes('--quick');
const pointWrites = args.includes('--point-writes');
if (outputs.length > 1 || new Set(args).size !== args.length || args.some(argument => argument.startsWith('--') && !['--resume', '--quick', '--point-writes'].includes(argument)) || (resume && !output) || (quick && pointWrites)) throw new Error('Usage: node scripts/benchmark-workloads.mjs [output.json] [--quick | --point-writes] [--resume]');
const repetitions = quick || pointWrites ? 3 : 5;
const runtime = [];
async function fingerprint(directory = dist) {
  for (const entry of await readdir(directory, {withFileTypes: true})) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) await fingerprint(path);
    else if (['.js', '.wasm'].includes(extname(path))) {
      const bytes = await readFile(path);
      runtime.push({path: path.slice(dist.length + 1), bytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex')});
    }
  }
}
await fingerprint();
runtime.sort((a, b) => a.path.localeCompare(b.path));
const server = createServer(async (request, response) => {
  try {
    const pathname = new URL(request.url, 'http://127.0.0.1').pathname;
    response.setHeader('Cache-Control', 'public, max-age=3600');
    if (pathname === '/') {
      response.setHeader('Content-Type', 'text/html');
      response.end('<!doctype html><title>TinyJoin workload benchmark</title><script type="module">import * as tinyjoin from "/index.js"; window.tinyjoin=tinyjoin;</script>');
      return;
    }
    const path = resolve(dist, `.${pathname}`);
    if (!path.startsWith(`${dist}${sep}`)) throw new Error('Invalid asset path');
    response.setHeader('Content-Type', extname(path) === '.wasm' ? 'application/wasm' : 'text/javascript');
    response.end(await readFile(path));
  } catch (error) {
    response.statusCode = 404;
    response.end(String(error));
  }
});
await new Promise((resolve, reject) => {server.once('error', reject); server.listen(0, '127.0.0.1', resolve);});
const origin = `http://127.0.0.1:${server.address().port}`;
let browser;
const distribution = values => {
  const sorted = [...values].sort((a, b) => a - b);
  return {count: sorted.length, min: sorted[0], median: sorted[Math.floor(sorted.length / 2)], p90: sorted[Math.ceil(sorted.length * 0.9) - 1], max: sorted.at(-1)};
};
const report = {
  measuredAt: new Date().toISOString(),
  packageVersion: JSON.parse(await readFile(resolve(dist, 'package.json'), 'utf8')).version,
  runtime,
  environment: {node: process.version, os: platform(), osRelease: release(), architecture: arch(), cpu: cpus()[0]?.model, logicalCpus: cpus().length, memoryBytes: totalmem(), headless: true, viewport: {width: 1280, height: 720}, cpuThrottling: 'none', network: 'loopback HTTP; warm cache permitted within each isolated context'},
  repetitions,
  profile: pointWrites ? 'point-writes' : quick ? 'quick' : 'full',
  schema: 'CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT NOT NULL, payload TEXT NOT NULL); CREATE UNIQUE INDEX items_title ON items (title)',
  definitions: {
    coldOpenMs: 'First create(opfs://fresh-name) in a fresh browser context, before schema setup. Client module already loaded; Worker/WASM first-load included. Browser process, OS, and filesystem caches are not reset.',
    reopenMs: 'After closing the sole client, create the same populated OPFS name in the same context. Includes fresh Worker and catalog open; HTTP/OS caches may be warm.',
    reopenReadMs: 'First ordered full read after reopening; separate from reopenMs.',
    stagingMs: 'Inside transaction callback: sequential prepared INSERT/UPDATE/DELETE requests, including Worker round trips and validation.',
    commitMs: 'End of callback to transaction resolution; includes commit RPC, persistence, and coordination overhead.',
    resultMs: 'query() call to complete materialized rows on requesting page, including engine execution, structured clone and owner/follower routing. Not an isolated copy benchmark; JSON byte sizing is outside timing.',
    percentiles: `Nearest-rank p90 of ${repetitions} samples is the maximum; retain all samples and do not treat it as a tail-latency estimate.`,
  },
  notes: 'One local desktop and one installed Chromium build. Sequential samples; no competitor, real mobile, Firefox, WebKit, power/thermal control, network download, or long-lived fragmentation coverage. No timing gate. Row checks run outside timing.',
  results: [],
};
let resumeBrowserVersion;
if (resume) {
  const previous = JSON.parse(await readFile(output, 'utf8'));
  if (JSON.stringify(previous.runtime) !== JSON.stringify(runtime) || previous.packageVersion !== report.packageVersion || previous.repetitions !== repetitions || (previous.profile ?? 'full') !== report.profile || previous.schema !== report.schema || Object.entries(report.environment).some(([key, value]) => JSON.stringify(previous.environment[key]) !== JSON.stringify(value))) throw new Error('Cannot resume with different runtime, schema, repetitions, profile, or environment');
  if (previous.results.some(result => result.samples.length !== repetitions)) throw new Error('Cannot resume an incomplete result shape');
  resumeBrowserVersion = previous.environment.browserVersion;
  report.environment.userAgent = previous.environment.userAgent;
  report.measuredAt = previous.measuredAt;
  report.resumedAt = new Date().toISOString();
  report.results = previous.results;
}
report.seeding = 'Setup is outside all timings. Payloads of 1024 bytes use at most 1000 rows per seed transaction to stay below retained batch limits; 64-byte payloads seed the whole shape in one transaction.';
const save = async () => {if (output) await writeFile(output, JSON.stringify(report, null, 2) + '\n');};
async function page(context) {
  const result = await context.newPage();
  await result.goto(origin);
  await result.waitForFunction(() => window.tinyjoin);
  return result;
}
async function seed(owner, {rowCount, payloadBytes, name}) {
  return owner.evaluate(async ({rowCount, payloadBytes, name}) => {
    const start = performance.now();
    window.db = await window.tinyjoin.create(`opfs://${name}`);
    const coldOpenMs = performance.now() - start;
    await window.db.exec('CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT NOT NULL, payload TEXT NOT NULL); CREATE UNIQUE INDEX items_title ON items (title)');
    const insert = await window.db.prepare('INSERT INTO items VALUES ($1, $2, $3)');
    try {
      const batchSize = payloadBytes >= 1024 ? 1000 : rowCount;
      for (let offset = 0; offset < rowCount; offset += batchSize) {
        await window.db.transaction(async tx => {
          for (let id = offset; id < Math.min(offset + batchSize, rowCount); id++) await tx.execute(insert, [id, `item-${id}`, 'x'.repeat(payloadBytes)]);
        });
      }
    } finally {await insert.close();}
    return coldOpenMs;
  }, {rowCount, payloadBytes, name});
}
async function verifyAndReopen(owner, name, expected) {
  return owner.evaluate(async ({name, expected}) => {
    const before = (await window.db.query('SELECT * FROM items ORDER BY id')).rows;
    if (before.length !== expected) throw new Error(`Expected ${expected} rows, got ${before.length}`);
    const serialized = JSON.stringify(before);
    const resultJsonBytes = new TextEncoder().encode(serialized).length;
    await window.db.close();
    const start = performance.now();
    window.db = await window.tinyjoin.create(`opfs://${name}`);
    const opened = performance.now();
    const after = (await window.db.query('SELECT * FROM items ORDER BY id')).rows;
    const end = performance.now();
    if (JSON.stringify(after) !== serialized) throw new Error('Reopened rows differ');
    const root = await navigator.storage.getDirectory();
    let storageBytes = 0;
    const size = async directory => {
      for await (const entry of directory.values()) {
        if (entry.kind === 'directory') await size(entry);
        else storageBytes += (await entry.getFile()).size;
      }
    };
    await size(root);
    return {reopenMs: opened - start, reopenReadMs: end - opened, resultJsonBytes, storageBytes};
  }, {name, expected});
}
try {
  browser = await chromium.launch({headless: true});
  report.environment.browserVersion = browser.version();
  if (resumeBrowserVersion && resumeBrowserVersion !== browser.version()) throw new Error('Cannot resume with a different browser version');
  for (const kind of pointWrites ? ['mixed'] : ['mixed', 'result']) {
    const shapes = kind === 'mixed'
      ? pointWrites ? [{rowCount: 5000, payloadBytes: 64, operationCount: 25}] : (quick ? [250] : [25, 100, 250]).map(rowCount => ({rowCount, payloadBytes: 64}))
      : (quick ? [5000] : [100, 1000, 5000]).flatMap(rowCount => (quick ? [1024] : [64, 1024]).map(payloadBytes => ({rowCount, payloadBytes})));
    for (const shape of shapes) {
      if (report.results.some(result => result.kind === kind && result.rowCount === shape.rowCount && result.payloadBytes === shape.payloadBytes)) continue;
      const samples = [];
      for (let repetition = 0; repetition < repetitions; repetition++) {
        const context = await browser.newContext({viewport: report.environment.viewport});
        try {
          const owner = await page(context);
          report.environment.userAgent ??= await owner.evaluate(() => navigator.userAgent);
          const name = `workload-${kind}-${Date.now()}-${repetition}`;
          const sample = {repetition, coldOpenMs: await seed(owner, {...shape, name})};
          let expected = shape.rowCount;
          if (kind === 'mixed') {
            Object.assign(sample, await owner.evaluate(async ({rowCount, operationCount = rowCount}) => {
              const insert = await window.db.prepare('INSERT INTO items VALUES ($1, $2, $3)');
              const update = await window.db.prepare('UPDATE items SET title = $1 WHERE id = $2');
              const remove = await window.db.prepare('DELETE FROM items WHERE id = $1');
              let stagingMs, staged;
              const start = performance.now();
              try {
                await window.db.transaction(async tx => {
                  const beginning = performance.now();
                  for (let index = 0; index < operationCount; index++) {
                    if (index % 3 === 0) await tx.execute(insert, [rowCount + index, `insert-${index}`, 'x'.repeat(64)]);
                    else if (index % 3 === 1) await tx.execute(update, [`update-${index}`, index]);
                    else await tx.execute(remove, [index]);
                  }
                  staged = performance.now();
                  stagingMs = staged - beginning;
                });
                const end = performance.now();
                const actual = (await window.db.query('SELECT id, title FROM items ORDER BY id')).rows;
                const model = new Map(Array.from({length: rowCount}, (_, id) => [id, `item-${id}`]));
                for (let index = 0; index < operationCount; index++) {
                  if (index % 3 === 0) model.set(rowCount + index, `insert-${index}`);
                  else if (index % 3 === 1) model.set(index, `update-${index}`);
                  else model.delete(index);
                }
                if (actual.length !== model.size || actual.some(row => model.get(row.id) !== row.title)) throw new Error('Mixed transaction differs from row model');
                return {stagingMs, commitMs: end - staged, transactionMs: end - start, operationCount, finalRows: model.size};
              } finally {await Promise.all([insert.close(), update.close(), remove.close()]);}
            }, shape));
            expected = sample.finalRows;
          }
          Object.assign(sample, await verifyAndReopen(owner, name, expected));
          if (kind === 'result') {
            // First client completes reopen before the second joins, so it owns the database.
            const follower = await page(context);
            sample.followerOpenMs = await follower.evaluate(async name => {
              const start = performance.now();
              window.db = await window.tinyjoin.create(`opfs://${name}`);
              return performance.now() - start;
            }, name);
            const read = async target => target.evaluate(async () => {
              const start = performance.now();
              const rows = (await window.db.query('SELECT * FROM items ORDER BY id')).rows;
              const resultMs = performance.now() - start;
              return {resultMs, rows: rows.length, json: JSON.stringify(rows)};
            });
            // Warm one read on each route, then alternate measured order across repetitions.
            await read(owner); await read(follower);
            let local, remote;
            if (repetition % 2 === 0) {local = await read(owner); remote = await read(follower);}
            else {remote = await read(follower); local = await read(owner);}
            if (local.rows !== expected || local.json !== remote.json) throw new Error('Owner/follower result differs');
            Object.assign(sample, {ownerResultMs: local.resultMs, followerResultMs: remote.resultMs});
            await follower.evaluate(() => window.db.close());
          }
          await owner.evaluate(() => window.db.close());
          samples.push(sample);
          console.log(JSON.stringify({kind, ...shape, ...sample}));
        } finally {await context.close();}
      }
      const fields = ['coldOpenMs', 'reopenMs', 'reopenReadMs', ...(kind === 'mixed' ? ['stagingMs', 'commitMs', 'transactionMs'] : ['followerOpenMs', 'ownerResultMs', 'followerResultMs'])];
      report.results.push({kind, ...shape, samples, distributions: Object.fromEntries(fields.map(field => [field, distribution(samples.map(sample => sample[field]))]))});
      await save();
    }
  }
  report.completedAt = new Date().toISOString();
  await save();
  console.log(JSON.stringify({output, results: report.results.length}));
} finally {
  await browser?.close();
  server.close();
}
