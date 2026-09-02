import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {access, readFile, readdir, writeFile} from 'node:fs/promises';
import {dirname, isAbsolute, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const target = 'wasm32-unknown-unknown';
const noticePattern =
  /^(?:copying|copyright|license|notice|unlicense)(?:[._-].*)?$/i;

export const thirdPartyNoticeFiles = [
  'THIRD_PARTY_NOTICES.txt',
  'RUST_STANDARD_LIBRARY_NOTICES.html',
];
const rustNoticeHashes = new Map([
  [
    '1.91.1',
    '3aa41caccecaeddad6fcf2f36ce14146ab7baae57064b05b12ecc6b52d5e917f',
  ],
]);

export async function assertThirdPartyNotices() {
  const generated = await generateThirdPartyNotices(false);
  const stale = [];
  for (const [name, content] of generated) {
    try {
      if (!(await readFile(resolve(root, name))).equals(content)) {
        stale.push(name);
      }
    } catch {
      stale.push(name);
    }
  }
  if (stale.length > 0) {
    throw new Error(
      `Third-party notices are stale: ${stale.join(', ')}. Run node scripts/third-party-notices.mjs --write.`,
    );
  }
}

async function writeThirdPartyNotices() {
  const generated = await generateThirdPartyNotices(true);
  for (const [name, content] of generated) {
    await writeFile(resolve(root, name), content);
  }
  console.log(`Wrote ${[...generated.keys()].join(' and ')}.`);
}

async function generateThirdPartyNotices(requireLocalRustNotice) {
  const graph = JSON.parse(
    run(getRustupTool('cargo'), [
      'metadata',
      '--locked',
      '--format-version',
      '1',
      '--filter-platform',
      target,
    ]),
  );
  const packagesById = new Map(
    graph.packages.map((package_) => [package_.id, package_]),
  );
  const nodesById = new Map(graph.resolve.nodes.map((node) => [node.id, node]));
  const wasmPackage = graph.packages.find(
    (package_) => package_.name === 'tinyjoin-wasm' && package_.source === null,
  );
  if (!wasmPackage) {
    throw new Error('Cargo metadata is missing tinyjoin-wasm.');
  }
  const packages = [...collectDependencies(wasmPackage.id, nodesById)]
    .map((id) => packagesById.get(id))
    .filter((package_) => package_?.source !== null)
    .sort(
      (left, right) =>
        compare(left.name, right.name) || compare(left.version, right.version),
    );

  const groups = new Map();
  const inventory = [];
  for (const package_ of packages) {
    if (!package_.license && !package_.license_file) {
      throw new Error(
        `${package_.name} ${package_.version} has no declared license.`,
      );
    }
    const files = await findNoticeFiles(package_);
    if (files.length === 0) {
      throw new Error(
        `${package_.name} ${package_.version} has no license or notice file.`,
      );
    }
    const references = [];
    for (const file of files) {
      const text = `${(await readFile(file.path, 'utf8'))
        .replaceAll('\r\n', '\n')
        .trimEnd()}\n`;
      const hash = createHash('sha256').update(text).digest('hex');
      let group = groups.get(hash);
      if (!group) {
        group = {hash, number: groups.size + 1, text, uses: []};
        groups.set(hash, group);
      }
      group.uses.push(`${package_.name} ${package_.version} / ${file.name}`);
      references.push(`[${group.number}] ${file.name}`);
    }
    inventory.push({
      license: package_.license ?? `license-file: ${package_.license_file}`,
      name: package_.name,
      references,
      version: package_.version,
    });
  }

  const rustc = getRustupTool('rustc');
  const rustVersion = run(rustc, ['--version']).trim();
  const pinnedRustVersion = await getPinnedRustVersion();
  if (!rustVersion.startsWith(`rustc ${pinnedRustVersion} `)) {
    throw new Error(
      `Expected rustc ${pinnedRustVersion} from rust-toolchain.toml, received ${rustVersion}.`,
    );
  }
  const rustSysroot = run(rustc, ['--print', 'sysroot']).trim();
  const rustNotice = await readRustNotice(rustSysroot, requireLocalRustNotice);
  assertRustNoticeHash(rustNotice, pinnedRustVersion);
  return new Map([
    [
      'THIRD_PARTY_NOTICES.txt',
      Buffer.from(renderNotices(inventory, [...groups.values()], rustVersion)),
    ],
    ['RUST_STANDARD_LIBRARY_NOTICES.html', rustNotice],
  ]);
}

function collectDependencies(rootId, nodesById) {
  const found = new Set();
  const pending = [rootId];
  while (pending.length > 0) {
    const id = pending.pop();
    if (found.has(id)) continue;
    found.add(id);
    const node = nodesById.get(id);
    if (!node) throw new Error(`Cargo metadata is missing node ${id}.`);
    for (const dependency of node.deps) {
      if (dependency.dep_kinds.some(({kind}) => kind !== 'dev')) {
        pending.push(dependency.pkg);
      }
    }
  }
  found.delete(rootId);
  return found;
}

async function findNoticeFiles(package_) {
  const packageRoot = dirname(package_.manifest_path);
  const entries = await readdir(packageRoot, {withFileTypes: true});
  const paths = new Map(
    entries
      .filter((entry) => entry.isFile() && noticePattern.test(entry.name))
      .map((entry) => [entry.name, resolve(packageRoot, entry.name)]),
  );
  if (package_.license_file) {
    const path = isAbsolute(package_.license_file)
      ? package_.license_file
      : resolve(packageRoot, package_.license_file);
    await access(path);
    paths.set(relative(packageRoot, path).replaceAll('\\', '/'), path);
  }
  return [...paths]
    .map(([name, path]) => ({name, path}))
    .sort((left, right) => compare(left.name, right.name));
}

async function readRustNotice(sysroot, requireLocal) {
  for (const path of [
    resolve(sysroot, 'share/doc/rust/html/COPYRIGHT-library.html'),
    resolve(sysroot, 'share/doc/rust/COPYRIGHT-library.html'),
    resolve(sysroot, 'share/doc/rustc/COPYRIGHT-library.html'),
  ]) {
    try {
      return await readFile(path);
    } catch {
      // Try the alternate layout used by supported Rust distributions.
    }
  }
  if (!requireLocal) {
    return readFile(resolve(root, 'RUST_STANDARD_LIBRARY_NOTICES.html'));
  }
  throw new Error(
    `Rust standard-library notices are missing under ${sysroot}.`,
  );
}

async function getPinnedRustVersion() {
  const toolchain = await readFile(
    resolve(root, 'rust-toolchain.toml'),
    'utf8',
  );
  const match = toolchain.match(/^\s*channel\s*=\s*"([^"]+)"/m);
  if (!match) {
    throw new Error('rust-toolchain.toml has no pinned channel.');
  }
  return match[1];
}

function assertRustNoticeHash(content, version) {
  const expected = rustNoticeHashes.get(version);
  if (!expected) {
    throw new Error(
      `No reviewed Rust standard-library notice hash is recorded for ${version}.`,
    );
  }
  const actual = createHash('sha256').update(content).digest('hex');
  if (actual !== expected) {
    throw new Error(
      `Rust ${version} standard-library notice has unexpected SHA-256 ${actual}.`,
    );
  }
}

function renderNotices(packages, groups, rustVersion) {
  const lines = [
    'TINYJOIN THIRD-PARTY NOTICES',
    '============================',
    '',
    `Generated from Cargo.lock for tinyjoin-wasm on ${target}.`,
    '',
    'Scope',
    '-----',
    '',
    'This inventory contains every third-party package reachable through a',
    'non-development dependency in the locked WebAssembly build graph. It',
    'conservatively includes build-time and procedural-macro packages; listing a',
    'package does not assert that it contributes bytes to the final WebAssembly.',
    '',
    'License expressions come from Cargo metadata. Every license, copying, and',
    'notice file shipped at the root of those crate archives is reproduced below;',
    'byte-identical texts are grouped once.',
    '',
    `TinyJoin is built with ${rustVersion}. Its official, release-specific Rust`,
    'standard-library copyright notice is included verbatim in',
    'RUST_STANDARD_LIBRARY_NOTICES.html. That upstream notice covers the standard',
    'library release as a whole, so it too is a conservative target superset.',
    '',
    'These notices are for attribution and reference. They do not alter the',
    'applicable upstream terms and are not legal advice.',
    '',
    `Packages (${packages.length})`,
    '-------------',
    '',
  ];
  for (const package_ of packages) {
    lines.push(
      `${package_.name} ${package_.version}`,
      `  Declared license: ${package_.license}`,
      `  Source: https://crates.io/crates/${package_.name}/${package_.version}`,
      `  Upstream files: ${package_.references.join('; ')}`,
      '',
    );
  }
  lines.push(
    'Upstream license and notice texts',
    '=================================',
    '',
  );
  for (const group of groups) {
    lines.push(
      `[${group.number}] SHA-256 ${group.hash}`,
      'Used by:',
      ...group.uses.map((use) => `  - ${use}`),
      '',
      '-------------------------------------------------------------------------------',
      group.text.trimEnd(),
      '-------------------------------------------------------------------------------',
      '',
    );
  }
  return `${lines.join('\n').trimEnd()}\n`;
}

function getRustupTool(tool) {
  const result = spawnSync('rustup', ['which', tool], {
    cwd: root,
    encoding: 'utf8',
  });
  return result.status === 0 && result.stdout.trim()
    ? result.stdout.trim()
    : tool;
}

function run(command, arguments_) {
  const result = spawnSync(command, arguments_, {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(
      `${command} ${arguments_.join(' ')} failed with ${result.status}:\n${result.stderr}`,
    );
  }
  return result.stdout;
}

function compare(left, right) {
  return left < right ? -1 : left > right ? 1 : 0;
}

const arguments_ = process.argv.slice(2);
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  if (
    arguments_.length !== 1 ||
    !['--check', '--write'].includes(arguments_[0])
  ) {
    throw new Error(
      'Usage: node scripts/third-party-notices.mjs --check|--write',
    );
  }
  if (arguments_[0] === '--write') {
    await writeThirdPartyNotices();
  } else {
    await assertThirdPartyNotices();
    console.log('Third-party notices match the locked WebAssembly graph.');
  }
}
