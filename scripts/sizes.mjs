import {readFile, readdir, writeFile, mkdir} from 'node:fs/promises';
import {dirname, join, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {gzipSync} from 'node:zlib';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// Committed metadata, so that the documentation build stays hermetic: it reads
// these measurements rather than the dist directory, which the documentation
// freshness check and the site workflow both run without.
export const sizesFile = resolve(root, 'site/data/sizes.json');

// The runtime files a browser downloads, grouped by the thread that loads them.
// protocol.js is imported by both sides and is counted once, with the
// main-thread client that also has to fetch it, so the groups sum to the total.
const GROUPS = [
  ['wasm', (path) => path.endsWith('.wasm')],
  ['client', (path) => path === 'index.js' || path === 'protocol.js'],
  ['worker', () => true],
];

// worker/index.js is the same bundle as worker/default-entry.js, for a Worker an
// application writes itself and builds with its own bundler. No page ever
// downloads both, so counting both would overstate what TinyJoin costs.
const ALTERNATIVE_ENTRIES = ['worker/index.js'];

export async function measureSizes(dist = resolve(root, 'dist')) {
  const totals = Object.fromEntries(
    [...GROUPS.map(([group]) => group), 'total'].map((group) => [
      group,
      {raw: 0, gzip: 0},
    ]),
  );

  for (const path of await getRuntimeFiles(dist)) {
    if (ALTERNATIVE_ENTRIES.includes(path)) {
      continue;
    }
    const contents = await readFile(resolve(dist, path));
    const group = GROUPS.find(([, matches]) => matches(path))[0];
    add(totals[group], contents);
    add(totals.total, contents);
  }

  return Object.fromEntries(
    Object.entries(totals).map(([group, {raw, gzip}]) => [
      group,
      {raw, gzip, gzipLabel: formatKib(gzip)},
    ]),
  );
}

export async function writeSizes(dist) {
  const sizes = await measureSizes(dist);
  await mkdir(dirname(sizesFile), {recursive: true});
  await writeFile(sizesFile, `${JSON.stringify(sizes, null, 2)}\n`, 'utf8');
  return sizes;
}

export async function readSizes() {
  return JSON.parse(await readFile(sizesFile, 'utf8'));
}

export function formatKib(bytes) {
  return `${Math.round(bytes / 1024).toLocaleString('en-US')} KiB`;
}

function add(total, contents) {
  total.raw += contents.byteLength;
  total.gzip += gzipSync(contents, {level: 9}).byteLength;
}

async function getRuntimeFiles(dist, prefix = '') {
  const entries = await readdir(resolve(dist, prefix), {withFileTypes: true});
  const files = await Promise.all(
    entries.map((entry) => {
      const path = prefix === '' ? entry.name : join(prefix, entry.name);
      if (entry.isDirectory()) {
        return getRuntimeFiles(dist, path);
      }
      return path.endsWith('.js') || path.endsWith('.wasm') ? [path] : [];
    }),
  );
  return files.flat().sort();
}
