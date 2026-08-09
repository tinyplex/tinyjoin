import {cp, mkdir, readdir, rm} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = resolve(root, 'dist');
const generatedWasm = resolve(root, 'src/generated/wasm');
const distWasm = resolve(dist, 'generated/wasm');

await rm(dist, {force: true, recursive: true});

const compiler = resolve(root, 'node_modules/typescript/bin/tsc');
const compile = spawnSync(
  process.execPath,
  [compiler, '-p', resolve(root, 'tsconfig.build.json')],
  {cwd: root, stdio: 'inherit'},
);
if (compile.status !== 0) {
  process.exit(compile.status ?? 1);
}

await mkdir(distWasm, {recursive: true});
for (const entry of await readdir(generatedWasm, {withFileTypes: true})) {
  if (entry.name === '.gitignore') {
    continue;
  }
  await cp(
    resolve(generatedWasm, entry.name),
    resolve(distWasm, entry.name),
    {recursive: entry.isDirectory()},
  );
}
