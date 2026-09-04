import {readFile} from 'node:fs/promises';
import {relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

import {getFiles} from './check-docs.mjs';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const committed = resolve(root, 'docs/lib');
const built = resolve(root, 'dist');

const RUNTIME_FILES = [
  'index.js',
  'protocol.js',
  'client',
  'worker',
  'wasm',
  'worker-opfs',
];

const isRuntimePath = (path) =>
  RUNTIME_FILES.some((file) => path === file || path.startsWith(`${file}/`));

const byPath = async (dir, filter = () => true) =>
  new Map(
    (await getFiles(dir))
      .map((file) => [relative(dir, file), file])
      .filter(([path]) => filter(path)),
  );

const committedFiles = await byPath(committed);
const builtFiles = await byPath(built, isRuntimePath);

if (builtFiles.size === 0) {
  throw new Error('No built runtime in dist. Run npm run build first.');
}

const missing = [...builtFiles.keys()].filter((path) => !committedFiles.has(path));
const extra = [...committedFiles.keys()].filter((path) => !builtFiles.has(path));
const changed = [];
for (const [path, builtFile] of builtFiles) {
  const committedFile = committedFiles.get(path);
  if (
    committedFile &&
    !(await readFile(committedFile)).equals(await readFile(builtFile))
  ) {
    changed.push(path);
  }
}

if (missing.length > 0 || extra.length > 0 || changed.length > 0) {
  const report = [
    missing.length > 0 ? `missing: ${missing.join(', ')}` : null,
    extra.length > 0 ? `unexpected: ${extra.join(', ')}` : null,
    changed.length > 0 ? `stale: ${changed.join(', ')}` : null,
  ]
    .filter(Boolean)
    .join('\n');
  throw new Error(
    `Committed docs/lib does not match dist. Run npm run build:docs.\n${report}`,
  );
}

console.log(`Committed docs/lib matches the built runtime (${builtFiles.size} files).`);
