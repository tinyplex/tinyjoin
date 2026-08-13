import {existsSync, readFileSync} from 'node:fs';
import {brotliCompressSync, constants, gzipSync} from 'node:zlib';

const wasmPath = 'dist/wasm-migration/tinygres_migration_wasm_bg.wasm';
const runtimePath =
  'dist/worker-migration/tinygres_migration_runtime.js';
// The migration engine is lazy-only, but it remains bounded so legacy snapshot
// compatibility cannot grow without an explicit release decision.
const limit = 700 * 1024;
const runtimeLimit = 64 * 1024;

if (!existsSync(wasmPath)) {
  console.error(`Missing ${wasmPath}. Run npm run build:wasm:migration first.`);
  process.exitCode = 1;
} else {
  const wasm = readFileSync(wasmPath);
  const gzipBytes = gzipSync(wasm, {level: 9}).byteLength;
  const brotliBytes = brotliCompressSync(wasm, {
    params: {[constants.BROTLI_PARAM_QUALITY]: 11},
  }).byteLength;
  const difference = wasm.byteLength - limit;

  console.log(`Migration WASM: ${formatBytes(wasm.byteLength)} uncompressed`);
  console.log(`Migration WASM: ${formatBytes(gzipBytes)} gzip -9`);
  console.log(`Migration WASM: ${formatBytes(brotliBytes)} brotli -11`);
  console.log(`Gate: ${formatBytes(limit)} uncompressed`);

  if (difference > 0) {
    console.error(`Size gate failed by ${formatBytes(difference)}.`);
    process.exitCode = 1;
  } else {
    console.log(`Size gate passed with ${formatBytes(-difference)} to spare.`);
  }
}

if (!existsSync(runtimePath)) {
  console.error(`Missing ${runtimePath}. Run npm run build first.`);
  process.exitCode = 1;
} else {
  const runtime = readFileSync(runtimePath);
  const gzipBytes = gzipSync(runtime, {level: 9}).byteLength;
  const difference = runtime.byteLength - runtimeLimit;

  console.log(
    `Migration runtime: ${formatBytes(runtime.byteLength)} uncompressed`,
  );
  console.log(`Migration runtime: ${formatBytes(gzipBytes)} gzip -9`);
  console.log(`Runtime gate: ${formatBytes(runtimeLimit)} uncompressed`);

  if (difference > 0) {
    console.error(
      `Migration runtime size gate failed by ${formatBytes(difference)}.`,
    );
    process.exitCode = 1;
  } else {
    console.log(
      `Migration runtime size gate passed with ${formatBytes(-difference)} to spare.`,
    );
  }
}

function formatBytes(bytes) {
  return `${(bytes / 1024).toFixed(1)} KiB`;
}
