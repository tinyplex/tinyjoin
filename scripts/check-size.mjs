import {existsSync, readFileSync} from 'node:fs';
import {brotliCompressSync, constants, gzipSync} from 'node:zlib';

import {measureSizes, readSizes, sizesFile} from './sizes.mjs';

const wasmPath = 'dist/wasm/tinyjoin_wasm_bg.wasm';
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

  await checkPublishedSizes();
}

// The site publishes these download sizes from committed metadata, so that the
// documentation build does not need a dist directory. Compare the labels the
// site would render rather than raw byte counts, which drift harmlessly with
// the zlib build in use.
async function checkPublishedSizes() {
  const measured = await measureSizes();
  for (const [group, {raw, gzip, gzipLabel}] of Object.entries(measured)) {
    console.log(
      `${group.padEnd(6)}: ${formatBytes(raw)} uncompressed, ` +
        `${formatBytes(gzip)} gzip -9 (published as ${gzipLabel})`,
    );
  }

  const published = existsSync(sizesFile) ? await readSizes() : {};
  const stale = Object.entries(measured).filter(
    ([group, {gzipLabel}]) => published[group]?.gzipLabel !== gzipLabel,
  );
  if (stale.length > 0) {
    console.error(
      `Published sizes are stale: ${stale
        .map(([group]) => group)
        .join(', ')}. Run npm run build:docs and commit the result.`,
    );
    process.exitCode = 1;
  } else {
    console.log('Published sizes match the built runtime.');
  }
}

function formatBytes(bytes) {
  return `${(bytes / 1024).toFixed(1)} KiB`;
}
