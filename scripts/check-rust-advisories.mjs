import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {existsSync} from 'node:fs';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const cache = resolve(root, '.cache/tinyjoin');
const output = resolve(cache, 'security');
const auditor = resolve(
  cache,
  'audit-tools/bin',
  process.platform === 'win32' ? 'cargo-audit.exe' : 'cargo-audit',
);
if (!existsSync(auditor)) {
  throw new Error(
    'Install the pinned Rust auditor: npm run setup:rust:advisories',
  );
}
await mkdir(output, {recursive: true});
const version = capture(auditor, ['--version']).trim();
if (version !== 'cargo-audit 0.22.2') {
  throw new Error(`Expected cargo-audit 0.22.2, received ${version}.`);
}

// Audit every locked crate without target filtering. Host-side tools also
// matter; the separate inventory explains which dependencies enter the WASM.
const audit = spawnSync(
  auditor,
  [
    'audit',
    '--file',
    'Cargo.lock',
    '--db',
    resolve(cache, 'advisory-db'),
    '--deny',
    'warnings',
    '--json',
  ],
  {cwd: root, encoding: 'utf8', maxBuffer: 20 * 1024 * 1024},
);
await writeFile(resolve(output, 'cargo-audit.json'), audit.stdout ?? '');
await writeFile(resolve(output, 'cargo-audit.stderr.log'), audit.stderr ?? '');
if (audit.stderr) process.stderr.write(audit.stderr);
if (audit.error) throw audit.error;
if (!audit.stdout.trim()) {
  throw new Error(`Rust advisory audit failed with status ${audit.status}.`);
}
const report = JSON.parse(audit.stdout);
console.log(JSON.stringify(report, null, 2));
if (
  report.settings.ignore.length > 0 ||
  report.settings.target_arch.length > 0 ||
  report.settings.target_os.length > 0 ||
  report.settings.severity !== null
) {
  throw new Error('Rust advisory checks require an unfiltered audit with no ignored advisories.');
}

const metadata = JSON.parse(
  capture('cargo', [
    'metadata',
    '--locked',
    '--format-version',
    '1',
    '--filter-platform',
    'wasm32-unknown-unknown',
  ]),
);
const packages = new Map(metadata.packages.map((entry) => [entry.id, entry]));
const nodes = new Map(metadata.resolve.nodes.map((entry) => [entry.id, entry]));
const wasm = metadata.packages.find(
  (entry) => entry.name === 'tinyjoin-wasm' && entry.source === null,
);
if (!wasm) throw new Error('Cargo metadata is missing tinyjoin-wasm.');
const runtime = new Set();
const pending = [wasm.id];
while (pending.length > 0) {
  const id = pending.pop();
  if (runtime.has(id)) continue;
  runtime.add(id);
  for (const dependency of nodes.get(id).deps) {
    const entry = packages.get(dependency.pkg);
    if (
      dependency.dep_kinds.some(({kind}) => kind === null) &&
      !entry.targets.some(({kind}) => kind.includes('proc-macro'))
    ) {
      pending.push(entry.id);
    }
  }
}
const inventory = {
  checkedAt: new Date().toISOString(),
  auditor: version,
  lockfileSha256: createHash('sha256')
    .update(await readFile(resolve(root, 'Cargo.lock')))
    .digest('hex'),
  database: report.database,
  target: 'wasm32-unknown-unknown',
  wasmRuntimeDependencies: [],
  buildOrTestOnlyDependencies: [],
};
for (const entry of metadata.packages) {
  if (entry.source === null) continue;
  const group = runtime.has(entry.id)
    ? inventory.wasmRuntimeDependencies
    : inventory.buildOrTestOnlyDependencies;
  group.push(`${entry.name} ${entry.version}`);
}
inventory.wasmRuntimeDependencies.sort();
inventory.buildOrTestOnlyDependencies.sort();
await writeFile(
  resolve(output, 'rust-dependency-inventory.json'),
  `${JSON.stringify(inventory, null, 2)}\n`,
);
console.log(JSON.stringify(inventory, null, 2));
if (audit.status !== 0) process.exitCode = audit.status ?? 1;

function capture(command, args) {
  const result = spawnSync(command, args, {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 20 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(
      `${command} ${args.join(' ')} failed:\n${result.stdout}${result.stderr}`,
    );
  }
  return result.stdout;
}
