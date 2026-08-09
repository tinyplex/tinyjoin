import {
  access,
  copyFile,
  readFile,
  writeFile,
} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = resolve(root, 'dist');
const wasm = resolve(dist, 'wasm/tinygres_wasm_bg.wasm');

try {
  await access(wasm);
} catch {
  console.error('Missing dist/wasm. Run npm run build:wasm first.');
  process.exit(1);
}

const compiler = resolve(root, 'node_modules/typescript/bin/tsc');
const compile = spawnSync(
  process.execPath,
  [compiler, '-p', resolve(root, 'tsconfig.build.json')],
  {cwd: root, stdio: 'inherit'},
);
if (compile.status !== 0) {
  process.exit(compile.status ?? 1);
}

const manifest = JSON.parse(
  await readFile(resolve(root, 'package.json'), 'utf8'),
);
delete manifest.private;
delete manifest.scripts;
delete manifest.devDependencies;

manifest.types = './index.d.ts';
manifest.exports = {
  '.': {
    types: './index.d.ts',
    import: './index.js',
  },
  './worker': {
    types: './worker/index.d.ts',
    import: './worker/index.js',
  },
  './supabase': {
    types: './adapters/supabase/index.d.ts',
    import: './adapters/supabase/index.js',
  },
  './package.json': './package.json',
};

await writeFile(
  resolve(dist, 'package.json'),
  `${JSON.stringify(manifest, null, 2)}\n`,
);
await copyFile(resolve(root, 'LICENSE'), resolve(dist, 'LICENSE'));
await copyFile(resolve(root, 'README.md'), resolve(dist, 'README.md'));
