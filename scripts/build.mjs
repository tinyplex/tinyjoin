import {rm} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

await rm(resolve(root, 'dist'), {force: true, recursive: true});
run(resolve(root, 'scripts/build-wasm.mjs'));
run(resolve(root, 'scripts/build-wasm-migration.mjs'));
run(resolve(root, 'scripts/build-lib.mjs'));

function run(script) {
  const result = spawnSync(process.execPath, [script], {
    cwd: root,
    stdio: 'inherit',
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}
