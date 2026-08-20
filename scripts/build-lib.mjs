import {copyFile, readFile, rm, writeFile} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

import {build as viteBuild} from 'vite';

import {requireWasmArtifacts} from './wasm-artifacts.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = resolve(root, 'dist');
try {
  await requireWasmArtifacts(dist);
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}

const compiler = resolve(root, 'node_modules/typescript/bin/tsc');
const compile = spawnSync(
  process.execPath,
  [compiler, '-p', resolve(root, 'tsconfig.build.json')],
  {cwd: root, stdio: 'inherit'},
);
if (compile.status !== 0) {
  process.exit(compile.status ?? 1);
}

await buildPrivateOpfsRuntime();
await assertOpfsLoaderBoundary();

const manifest = JSON.parse(
  await readFile(resolve(root, 'package.json'), 'utf8'),
);
delete manifest.private;
delete manifest.scripts;
delete manifest.devDependencies;

manifest.types = './index.d.ts';
manifest.exports = {
  '.': {
    types: './index.d.ts',
    import: './index.js',
  },
  './worker': {
    types: './worker/index.d.ts',
    import: './worker/index.js',
  },
  './package.json': './package.json',
};

await writeFile(
  resolve(dist, 'package.json'),
  `${JSON.stringify(manifest, null, 2)}\n`,
);
await copyFile(resolve(root, 'LICENSE'), resolve(dist, 'LICENSE'));
await copyFile(resolve(root, 'README.md'), resolve(dist, 'README.md'));

async function buildPrivateOpfsRuntime() {
  await buildOpfsRuntime();

  // These page-storage modules are private build inputs. Only their
  // self-contained runtime asset is published, so memory-only sessions do not
  // pull OPFS code into their Worker bundle.
  await Promise.all(
    ['opfs-engine', 'page-storage'].flatMap((module) => [
      rm(resolve(dist, `worker/${module}.js`), {force: true}),
      rm(resolve(dist, `worker/${module}.d.ts`), {force: true}),
    ]),
  );
}

async function buildOpfsRuntime() {
  const entry = resolve(dist, 'worker/opfs-engine.js');
  const outputDirectory = resolve(dist, 'worker-opfs');
  const output = resolve(outputDirectory, 'tinygres_opfs_runtime.js');

  await viteBuild({
    build: {
      codeSplitting: false,
      copyPublicDir: false,
      emptyOutDir: true,
      lib: {
        entry,
        fileName: () => 'tinygres_opfs_runtime.js',
        formats: ['es'],
      },
      minify: 'oxc',
      outDir: outputDirectory,
    },
    configFile: false,
    logLevel: 'warn',
  });

  const generated = await readFile(output, 'utf8');
  const source = assertSelfContainedRuntime(
    generated,
    output,
    'OPFS runtime',
    [
      'createOpfsWasmEngine',
      'Another TinyGres worker already has this OPFS database open',
    ],
    [
      [/\bimport\s*\(/, 'a dynamic import'],
      [/tinygres_wasm(?:_bg)?/i, 'default WASM glue'],
      [/WASM returned an invalid binary response/, 'the WASM wire adapter'],
    ],
  );
  const rawBytes = Buffer.byteLength(source);
  const maximumRawBytes = 96 * 1024;
  if (rawBytes > maximumRawBytes) {
    throw new Error(
      `OPFS runtime is ${rawBytes} bytes, above the ${maximumRawBytes}-byte private-runtime gate: ${output}`,
    );
  }
  await writeFile(output, source);
}

function assertSelfContainedRuntime(
  generated,
  output,
  label,
  requiredMarkers,
  additionalForbidden = [],
) {
  // Rolldown's region comments include absolute source identifiers. They are
  // useful in debug bundles, but would make a published internal asset leak
  // and depend on the checkout path.
  const source = generated.replace(/^\/\/#(?:end)?region.*(?:\r?\n|$)/gm, '');
  const forbidden = [
    [
      /data:(?:application\/wasm|text\/javascript)/i,
      'an inlined runtime or WASM data URL',
    ],
    [/\bimport(?![\w$]|\s*(?:\(|\.))/, 'a static import'],
    [/\bfrom\s*["'][.]{0,2}\//, 'an unresolved relative import'],
    [/\bnew URL\s*\(/, 'an unresolved asset URL'],
    [/\bfile:\/\//, 'an absolute file URL'],
    [/(?:^|[^\w])\/(?:Users|private|tmp)\//, 'an absolute POSIX path'],
    [/(?:^|[^\w])[A-Za-z]:\\/, 'an absolute Windows path'],
    [new RegExp(escapeRegExp(root)), 'the source checkout path'],
    ...additionalForbidden,
  ];
  for (const [pattern, description] of forbidden) {
    if (pattern.test(source)) {
      throw new Error(`${label} contains ${description}: ${output}`);
    }
  }
  for (const marker of requiredMarkers) {
    if (!source.includes(marker)) {
      throw new Error(`${label} is missing ${marker}: ${output}`);
    }
  }
  return source;
}

async function assertOpfsLoaderBoundary() {
  const path = resolve(dist, 'worker/opfs-loader.js');
  const source = await readFile(path, 'utf8');
  for (const marker of [
    "'../worker-opfs/tinygres_opfs_runtime.js'",
    '/* @vite-ignore */',
    '/* webpackIgnore: true */',
  ]) {
    if (!source.includes(marker)) {
      throw new Error(`OPFS loader is missing ${marker}: ${path}`);
    }
  }
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}
