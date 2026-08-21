import {existsSync, readFileSync} from 'node:fs';
import {brotliCompressSync, constants, gzipSync} from 'node:zlib';

const wasmPath = 'dist/wasm/tinygres_wasm_bg.wasm';
const limit = 1024 * 1024;

if (!existsSync(wasmPath)) {
  console.error(`Missing ${wasmPath}. Run npm run build:wasm first.`);
  process.exitCode = 1;
} else {
  const wasm = readFileSync(wasmPath);
  const gzipBytes = gzipSync(wasm, {level: 9}).byteLength;
  const brotliBytes = brotliCompressSync(wasm, {
    params: {[constants.BROTLI_PARAM_QUALITY]: 11},
  }).byteLength;
  const difference = wasm.byteLength - limit;

  console.log(`WASM: ${formatBytes(wasm.byteLength)} uncompressed`);
  console.log(`WASM: ${formatBytes(gzipBytes)} gzip -9`);
  console.log(`WASM: ${formatBytes(brotliBytes)} brotli -11`);
  console.log(`Gate: ${formatBytes(limit)} uncompressed`);

  if (difference > 0) {
    console.error(`Size gate failed by ${formatBytes(difference)}.`);
    process.exitCode = 1;
  } else {
    console.log(`Size gate passed with ${formatBytes(-difference)} to spare.`);
  }
}

function formatBytes(bytes) {
  return `${(bytes / 1024).toFixed(1)} KiB`;
}
