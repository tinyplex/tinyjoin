import {existsSync} from 'node:fs';
import {cp, mkdir, mkdtemp, readdir, rm} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';
import {tmpdir} from 'node:os';
import {dirname, join, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {resolveRustEnvironment} from './rust-environment.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const {rustDirectories, rustEnvironment} = resolveRustEnvironment(root);

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
