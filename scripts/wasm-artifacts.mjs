import {access} from 'node:fs/promises';
import {resolve} from 'node:path';

export const wasmArtifacts = [
  'wasm/tinygres_wasm.js',
  'wasm/tinygres_wasm_bg.wasm',
];

export async function requireWasmArtifacts(dist) {
  try {
    await Promise.all(
      wasmArtifacts.map((artifact) => access(resolve(dist, artifact))),
    );
  } catch {
    throw new Error(
      'Missing TinyGres WASM artifacts. Run npm run build first.',
    );
  }
}
