import {resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {defineConfig} from 'vite';

const here = fileURLToPath(new URL('.', import.meta.url));

// A production build, as an application would ship: minified, hashed, and
// with WebAssembly and data files emitted as separate assets.
export default defineConfig({
  root: resolve(here, 'app'),
  base: '/',
  resolve: {alias: {tinyjoin: resolve(here, '../../dist/index.js')}},
  build: {
    outDir: resolve(here, '.build'),
    emptyOutDir: true,
    manifest: true,
    assetsInlineLimit: 0,
    modulePreload: false,
    target: 'es2022',
    // PGlite's Emscripten output uses direct eval; that is not ours to fix.
    rolldownOptions: {
      onLog: (level, log, handler) => log.code !== 'EVAL' && handler(level, log),
    },
  },
  worker: {format: 'es'},
});
