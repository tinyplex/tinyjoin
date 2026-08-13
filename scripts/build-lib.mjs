import {
  copyFile,
  readFile,
  rm,
  writeFile,
} from 'node:fs/promises';
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

await buildMigrationRuntime();

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
  './supabase': {
    types: './adapters/supabase/index.d.ts',
    import: './adapters/supabase/index.js',
  },
  './package.json': './package.json',
};

await writeFile(
  resolve(dist, 'package.json'),
  `${JSON.stringify(manifest, null, 2)}\n`,
);
await copyFile(resolve(root, 'LICENSE'), resolve(dist, 'LICENSE'));
await copyFile(resolve(root, 'README.md'), resolve(dist, 'README.md'));

async function buildMigrationRuntime() {
  const entry = resolve(dist, 'worker/migration-runtime.js');
  const declaration = resolve(dist, 'worker/migration-runtime.d.ts');
  const outputDirectory = resolve(dist, 'worker-migration');
  const output = resolve(
    outputDirectory,
    'tinygres_migration_runtime.js',
  );

  await viteBuild({
    build: {
      codeSplitting: false,
      copyPublicDir: false,
      emptyOutDir: true,
      lib: {
        entry,
        fileName: () => 'tinygres_migration_runtime.js',
        formats: ['es'],
      },
      minify: 'oxc',
      outDir: outputDirectory,
    },
    configFile: false,
    logLevel: 'warn',
  });

  const generated = await readFile(output, 'utf8');
  // Rolldown's region comments include absolute source identifiers. They are
  // useful in debug bundles, but would make the published internal asset leak
  // and depend on the checkout path.
  const source = generated.replace(/^\/\/#(?:end)?region.*(?:\r?\n|$)/gm, '');
  const forbidden = [
    [/data:application\/wasm/i, 'an inlined WASM data URL'],
    [/\bimport(?:\s+[\w{*]|\s*["'])/, 'a static import'],
    [/\bfrom\s*["'][.]{0,2}\//, 'an unresolved relative import'],
    [/\bnew URL\s*\(/, 'an unresolved asset URL'],
    [/\bfile:\/\//, 'an absolute file URL'],
    [/(?:^|[^\w])\/(?:Users|private|tmp)\//, 'an absolute POSIX path'],
    [/(?:^|[^\w])[A-Za-z]:\\/, 'an absolute Windows path'],
    [new RegExp(escapeRegExp(root)), 'the source checkout path'],
  ];
  for (const [pattern, description] of forbidden) {
    if (pattern.test(source)) {
      throw new Error(
        `Migration runtime contains ${description}: ${output}`,
      );
    }
  }
  if (!source.includes('createMigrationConfiguredEngine')) {
    throw new Error(
      `Migration runtime does not export createMigrationConfiguredEngine: ${output}`,
    );
  }
  await writeFile(output, source);

  // This source entry is private build input. Only the self-contained runtime
  // asset is published, so normal Worker bundles cannot pull migration code
  // into the memory-only path through the module graph.
  await Promise.all([
    rm(entry, {force: true}),
    rm(declaration, {force: true}),
    ...[
      'journal-codec',
      'journal-payload',
      'migration-engine',
      'persistent-engine',
      'snapshot-store',
    ].flatMap((module) => [
      rm(resolve(dist, `worker/${module}.js`), {force: true}),
      rm(resolve(dist, `worker/${module}.d.ts`), {force: true}),
    ]),
  ]);
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}
