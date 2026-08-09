import {existsSync} from 'node:fs';
import {spawnSync} from 'node:child_process';

const target = 'wasm32-unknown-unknown';
const targetLibDir = capture('rustc', [
  '--print',
  'target-libdir',
  '--target',
  target,
]);

if (!targetLibDir.ok || !existsSync(targetLibDir.output)) {
  const sysroot = capture('rustc', ['--print', 'sysroot']);
  const rustup = capture('rustup', ['--version']);
  const detected = sysroot.ok ? `\nDetected Rust sysroot: ${sysroot.output}` : '';

  console.error(`
Tinygres cannot build WebAssembly because the ${target} standard library is
not installed for the active Rust compiler.${detected}

This repository uses rustup to install its pinned Rust version, Clippy,
rustfmt, and the WASM target from rust-toolchain.toml.
`);

  if (rustup.ok) {
    console.error(`rustup is installed, but its compiler is not active or its target is missing.

Make sure the rustup proxy directory comes before Homebrew or system Rust in
PATH, then run:

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
const build = spawnSync(
  wasmPack,
  [
    'build',
    'crates/tinygres-wasm',
    '--target',
    'web',
    '--out-dir',
    '../../src/generated/wasm',
    '--out-name',
    'tinygres_wasm',
    '--release',
  ],
  {stdio: 'inherit'},
);

if (build.error) {
  console.error(`Could not start wasm-pack: ${build.error.message}`);
  process.exit(1);
}
process.exit(build.status ?? 1);

function capture(command, args) {
  const result = spawnSync(command, args, {encoding: 'utf8'});
  return {
    ok: result.status === 0,
    output: result.status === 0 ? result.stdout.trim() : '',
  };
}
