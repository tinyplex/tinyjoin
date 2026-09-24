import {existsSync, statSync} from 'node:fs';
import {cp, mkdir, mkdtemp, readdir, rm} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';
import {tmpdir} from 'node:os';
import {delimiter, dirname, join, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const pathKey =
  Object.keys(process.env).find((key) => key.toLowerCase() === 'path') ??
  'PATH';
const rustupExecutable = findExecutable(
  process.platform === 'win32' ? ['rustup.exe', 'rustup'] : ['rustup'],
);
// Prefer rustup's own cargo and rustc proxies: they select the pinned
// toolchain and set the library path its tools need, such as the shared
// libLLVM that rust-lld loads on macOS. The resolved toolchain binaries are the
// fallback when the proxies do not sit beside rustup.
const rustupProxyDirectories =
  rustupExecutable &&
  ['cargo', 'rustc'].every((tool) => isRustupProxy(tool, rustupExecutable))
    ? [dirname(rustupExecutable)]
    : [];
const rustupTools =
  rustupExecutable && rustupProxyDirectories.length === 0
    ? [
        capture(rustupExecutable, ['which', 'rustc']),
        capture(rustupExecutable, ['which', 'cargo']),
      ]
    : [];
const rustupToolchainDirectories =
  rustupTools.length === 2 &&
  rustupTools.every((tool) => tool.ok && existsSync(tool.output))
    ? [...new Set(rustupTools.map((tool) => dirname(tool.output)))]
    : [];
const rustDirectories = [
  ...rustupProxyDirectories,
  ...rustupToolchainDirectories,
];
const rustEnvironment = rustDirectories.length > 0
  ? {
      ...process.env,
      [pathKey]: [...rustDirectories, process.env[pathKey]]
        .filter(Boolean)
        .join(delimiter),
    }
  : process.env;

const target = 'wasm32-unknown-unknown';
const targetLibDir = capture(
  'rustc',
  ['--print', 'target-libdir', '--target', target],
  rustEnvironment,
);

if (!targetLibDir.ok || !existsSync(targetLibDir.output)) {
  const sysroot = capture('rustc', ['--print', 'sysroot'], rustEnvironment);
  const rustup = capture('rustup', ['--version'], rustEnvironment);
  const detected = sysroot.ok ? `\nDetected Rust sysroot: ${sysroot.output}` : '';

  console.error(`
TinyJoin cannot build WebAssembly because the ${target} standard library is
not installed for the active Rust compiler.${detected}

This repository uses rustup to install its pinned Rust version, Clippy,
rustfmt, and the WASM target from rust-toolchain.toml.
`);

  if (rustup.ok) {
    const cause =
      rustDirectories.length > 0
        ? "the pinned toolchain's target is missing"
        : 'the repository toolchain could not be resolved';
    console.error(`rustup is installed, but ${cause}.

Run:

  rustup target add ${target}
  npm run build:wasm
`);
  } else if (process.platform === 'win32') {
    console.error(`Install rustup from https://rustup.rs, open a new terminal in this repository,
then rerun npm run build:wasm.
`);
  } else {
    console.error(`Install rustup alongside the existing compiler:

  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --default-toolchain none -y
  . "$HOME/.cargo/env"
  npm run build:wasm

You do not need to uninstall Homebrew Rust. Keep the cargo-env line after any
Homebrew shell setup so rustup's proxies appear first in PATH.
`);
  }
  process.exit(1);
}

const wasmPack = process.platform === 'win32' ? 'wasm-pack.cmd' : 'wasm-pack';
const staging = await mkdtemp(join(tmpdir(), 'tinyjoin-wasm-'));
const distWasm = resolve(root, 'dist/wasm');
const cargoTargetDir = resolve(
  root,
  'node_modules/.cache/tinyjoin/cargo-target',
);
const wasmRustFlags = [
  '-Ctarget-feature=+bulk-memory,+nontrapping-fptoint,+sign-ext,+mutable-globals,+simd128',
  process.env.RUSTFLAGS,
]
  .filter(Boolean)
  .join(' ');

try {
  const build = spawnSync(
    wasmPack,
    [
      'build',
      'crates/tinyjoin-wasm',
      '--target',
      'web',
      '--out-dir',
      staging,
      '--out-name',
      'tinyjoin_wasm',
      '--release',
      '--no-pack',
      '--locked',
    ],
    {
      cwd: root,
      env: {
        ...rustEnvironment,
        CARGO_TARGET_DIR: cargoTargetDir,
        RUSTFLAGS: wasmRustFlags,
      },
      stdio: 'inherit',
    },
  );

  if (build.error) {
    console.error(`Could not start wasm-pack: ${build.error.message}`);
    process.exitCode = 1;
  } else if (build.status !== 0) {
    process.exitCode = build.status ?? 1;
  } else {
    const expected = new Set([
      '.gitignore',
      'snippets',
      'tinyjoin_wasm.d.ts',
      'tinyjoin_wasm.js',
      'tinyjoin_wasm_bg.wasm',
      'tinyjoin_wasm_bg.wasm.d.ts',
    ]);
    const unexpected = (await readdir(staging)).filter(
      (entry) => !expected.has(entry),
    );
    if (unexpected.length > 0) {
      throw new Error(
        `wasm-pack emitted unexpected files: ${unexpected.join(', ')}`,
      );
    }
    await rm(distWasm, {force: true, recursive: true});
    await mkdir(distWasm, {recursive: true});
    for (const file of ['tinyjoin_wasm.js', 'tinyjoin_wasm_bg.wasm']) {
      await cp(resolve(staging, file), resolve(distWasm, file));
    }
    if (existsSync(resolve(staging, 'snippets'))) {
      await cp(resolve(staging, 'snippets'), resolve(distWasm, 'snippets'), {
        recursive: true,
      });
    }
  }
} finally {
  await rm(staging, {force: true, recursive: true});
}

function capture(command, args, environment = process.env) {
  const result = spawnSync(command, args, {
    cwd: root,
    encoding: 'utf8',
    env: environment,
  });
  return {
    ok: result.status === 0,
    output: result.status === 0 ? result.stdout.trim() : '',
  };
}

function isRustupProxy(tool, rustup) {
  const name = process.platform === 'win32' ? `${tool}.exe` : tool;
  try {
    // Proxies are symbolic or hard links to rustup itself.
    const proxy = statSync(resolve(dirname(rustup), name));
    const target = statSync(rustup);
    return proxy.dev === target.dev && proxy.ino === target.ino;
  } catch {
    return false;
  }
}

function findExecutable(names) {
  const path = process.env[pathKey];
  if (!path) {
    return undefined;
  }
  for (const directory of path.split(delimiter)) {
    if (!directory) {
      continue;
    }
    for (const name of names) {
      const candidate = resolve(directory.replace(/^"|"$/g, ''), name);
      if (existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}
