import {spawnSync} from 'node:child_process';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const result = spawnSync('cargo', process.argv.slice(2), {
  cwd: root,
  env: {
    ...process.env,
    CARGO_TARGET_DIR: resolve(
      root,
      'node_modules/.cache/tinyjoin/cargo-target',
    ),
  },
  stdio: 'inherit',
});

if (result.error) {
  throw result.error;
}
process.exit(result.status ?? 1);
