import {readFileSync, writeFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {arch, cpus, platform} from 'node:os';

// Build first. An optional output path retains the complete samples as JSON.
// This is a diagnostic benchmark, never a timing threshold in the test suite.
const root = fileURLToPath(new URL('../', import.meta.url));
const output = process.argv[2];
if (process.argv.length > 3) throw new Error('Usage: node scripts/benchmark-staging.mjs [output.json]');
const wasmPath = resolve(root, 'dist/wasm/tinyjoin_wasm_bg.wasm');
const wasm = readFileSync(wasmPath);
const {default: init, WasmEngine} = await import(pathToFileURL(resolve(root, 'dist/wasm/tinyjoin_wasm.js')).href);
await init({module_or_path: wasm});

class Device {
  pages = [];
  reads = 0;
  writes = 0;
  flushes = 0;
  pageCount() { return this.pages.length; }
  readPage(low, high, destination) {
    if (high !== 0) throw new Error('Unexpected high page word');
    destination.set(this.pages[low]);
    this.reads++;
    return destination.length;
  }
  writePage(low, high, source) {
    if (high !== 0) throw new Error('Unexpected high page word');
    this.pages[low] = source.slice();
    this.writes++;
    return source.length;
  }
  flush() { this.flushes++; }
  close() {}
}
const call = (database, operation, payload) => {
  const result = database.callStructured(2, operation, payload);
  if (result[1]) throw new Error(JSON.stringify({operation, result}));
  return result[3];
};
const query = (database, sql, params = []) => call(database, 1, {sql, params});

function measure(rows, uniqueIndex) {
  const device = new Device();
  const database = new WasmEngine(device);
  try {
    query(database, 'CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT NOT NULL)');
    if (uniqueIndex) query(database, 'CREATE UNIQUE INDEX items_title ON items (title)');
    const statementId = call(database, 3, 'INSERT INTO items VALUES ($1, $2)');
    call(database, 6);
    const reads = device.reads;
    const start = performance.now();
    for (let id = 0; id < rows; id++) call(database, 4, {statementId, params: [id, `item-${id}`]});
    const staged = performance.now();
    const stagingPageReads = device.reads - reads;
    call(database, 7);
    const committed = performance.now();
    const {rows: resultRows} = query(database, 'SELECT COUNT(*) AS count FROM items');
    if (resultRows[0]?.count !== rows) throw new Error('Committed row count differs from inserted rows');
    return {
      rows, uniqueIndex,
      stagingMs: staged - start,
      commitMs: committed - staged,
      stagingPageReads,
      pageCount: device.pages.length,
    };
  } finally {
    database.free();
  }
}

const report = {
  measuredAt: new Date().toISOString(),
  environment: 'Node in-memory structured WASM; no Worker, no OPFS; timings exclude initialization, schema setup, preparation, and validation read',
  node: process.version,
  platform: platform(),
  architecture: arch(),
  cpu: cpus()[0]?.model,
  packageVersion: JSON.parse(readFileSync(resolve(root, 'dist/package.json'), 'utf8')).version,
  wasmSha256: createHash('sha256').update(wasm).digest('hex'),
  wasmBytes: wasm.length,
  repetitions: 3,
  warmupRowsPerShape: 100,
  results: [],
};
console.log(JSON.stringify({...report, results: undefined}));
for (const uniqueIndex of [false, true]) {
  measure(100, uniqueIndex);
  for (const rows of [100, 500, 1000, 2000]) {
    const samples = Array.from({length: report.repetitions}, () => measure(rows, uniqueIndex));
    const median = key => samples.map(sample => sample[key]).sort((a, b) => a - b)[Math.floor(samples.length / 2)];
    const result = {
      rows, uniqueIndex,
      stagingMedianMs: median('stagingMs'),
      commitMedianMs: median('commitMs'),
      samples,
    };
    report.results.push(result);
    if (output) writeFileSync(output, JSON.stringify(report, null, 2) + '\n');
    console.log(JSON.stringify({rows, uniqueIndex, stagingMedianMs: result.stagingMedianMs, commitMedianMs: result.commitMedianMs}));
  }
}
if (output) console.log(`Saved ${output}`);
