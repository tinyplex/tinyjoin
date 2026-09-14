import {createServer} from 'node:http';
import {readFile, writeFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {resolve, extname, sep} from 'node:path';
import {chromium} from '@playwright/test';
import {fileURLToPath} from 'node:url';
// Build first. Uses an isolated Chromium profile and closes every database.
// This diagnostic benchmark is not a timing gate.
const dist = fileURLToPath(new URL('../dist', import.meta.url));
const output = process.argv[2];
if (process.argv.length > 3) throw new Error('Usage: node scripts/benchmark-browser-inserts.mjs [output.json]');
const wasm = await readFile(resolve(dist, 'wasm/tinyjoin_wasm_bg.wasm'));
const wasmSha256 = createHash('sha256').update(wasm).digest('hex');
const manifest = JSON.parse(await readFile(resolve(dist, 'package.json'), 'utf8'));
const server = createServer(async (request, response) => {
  try {
    const pathname = new URL(request.url, 'http://127.0.0.1:4189').pathname;
    if (pathname === '/') {
      response.setHeader('Content-Type', 'text/html');
      response.end('<!doctype html><title>TinyJoin insert benchmark</title><script type="module">import * as tinyjoin from "/index.js";window.tinyjoin=tinyjoin;</script>');
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
await new Promise((resolve, reject) => {server.once('error', reject);server.listen(4189, '127.0.0.1', resolve);});
let browser;
try {
  browser = await chromium.launch({headless: true});
  const page = await browser.newPage();
  await page.goto('http://127.0.0.1:4189');
  await page.waitForFunction(() => window.tinyjoin);
  const samples = [];
  for (const mode of ['memory', 'opfs']) {
    for (let repetition = 0; repetition < 3; repetition++) {
      const sample = await page.evaluate(async ({mode, repetition}) => {
        const name = `insert-benchmark-${Date.now()}-${repetition}`;
        const url = mode === 'memory' ? 'memory://' : `opfs://${name}`;
        const openStarted = performance.now();
        const database = await window.tinyjoin.create(url);
        const openMs = performance.now() - openStarted;
        let statement;
        try {
          await database.exec('CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT NOT NULL)');
          statement = await database.prepare('INSERT INTO items (id, value) VALUES ($1, $2)');
          let stagingMs;
          let stagingEnded;
          const transactionStarted = performance.now();
          await database.transaction(async transaction => {
            const stagingStarted = performance.now();
            for (let id = 0; id < 1000; id++) {
              await transaction.execute(statement, [id, `row-${id}`]);
            }
            stagingEnded = performance.now();
            stagingMs = stagingEnded - stagingStarted;
          });
          const transactionEnded = performance.now();
          const {rows} = await database.query('SELECT COUNT(*) AS total FROM items');
          if (rows[0].total !== 1000) throw new Error(`Wrong row count: ${rows[0].total}`);
          return {mode, repetition, rowCount: rows[0].total, openMs, stagingMs, commitMs: transactionEnded - stagingEnded, transactionMs: transactionEnded - transactionStarted};
        } finally {
          await statement?.close().catch(() => undefined);
          await database.close();
        }
      }, {mode, repetition});
      samples.push(sample);
      console.log(JSON.stringify(sample));
    }
  }
  const median = values => values.sort((a,b)=>a-b)[Math.floor(values.length/2)];
  const medians = Object.fromEntries(['memory', 'opfs'].map(mode => {
    const values=samples.filter(sample=>sample.mode===mode);
    return [mode, Object.fromEntries(['openMs','stagingMs','commitMs','transactionMs'].map(field=>[field,median(values.map(value=>value[field]))]))];
  }));
  const result = {createdAt: new Date().toISOString(),packageVersion:manifest.version,wasmSha256,wasmBytes:wasm.length,browserVersion:browser.version(),rows:1000,indexes:'primary key only',notes:'Packaged default Worker; three sequential fresh databases per mode; commitMs measures callback completion to transaction resolution; local diagnostic, not controlled hardware or competitor benchmark.',samples,medians};
  if (output) await writeFile(output, JSON.stringify(result,null,2)+'\n');
  console.log(JSON.stringify({output,wasmSha256,medians}));
} finally {
  await browser?.close();
  server.close();
}
