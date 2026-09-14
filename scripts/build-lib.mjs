import {
  copyFile,
  mkdir,
  readdir,
  readFile,
  rm,
  writeFile,
} from 'node:fs/promises';
import {spawnSync} from 'node:child_process';
import {basename, dirname, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

import {build as esbuildBuild} from 'esbuild';
import {minify} from 'terser';
import {build as viteBuild} from 'vite';

import {buildDefinitions} from './build-definitions.mjs';
import {
  assertThirdPartyNotices,
  thirdPartyNoticeFiles,
} from './third-party-notices.mjs';
import {writeSizes} from './sizes.mjs';
import {requireWasmArtifacts} from './wasm-artifacts.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const dist = resolve(root, 'dist');

const WORKER_SHARED = {
  'protocol.js': '../protocol.js',
  // The WASM glue resolves the engine binary against itself, so it stays beside
  // it in wasm/ rather than being bundled in here.
  'tinyjoin_wasm.js': '../wasm/tinyjoin_wasm.js',
};

// The runtime is published as bundles that share protocol.js: the main-thread
// client, and the Worker host. Bundling lets the minifier rename everything that
// is not public API, and leaves the OPFS page device out of a Worker that only
// ever opens a memory database.
const RUNTIME_BUNDLES = [
  // Bundled first, so that both sides can then share it as one file.
  {entry: 'protocol.js', shared: {}},
  {entry: 'index.js', shared: {'protocol.js': './protocol.js'}},
  {entry: 'worker/index.js', shared: WORKER_SHARED},
  // The Worker the package ships is bundled a second time, with its startWorker
  // call, rather than importing worker/index.js. The two are never both
  // downloaded - an application either loads this entry or bundles the other
  // one into a Worker of its own - so the duplicate costs nobody any bytes, and
  // it saves the browser a round trip on a forty-five byte shim.
  {entry: 'worker/default-entry.js', shared: WORKER_SHARED},
  {entry: 'vite/index.js', shared: {}, platform: 'node'},
];

// Published JavaScript, including the Node-only Vite plugin. tsc emits one
// module per source file; unlisted modules are bundled before pruning. The
// size measurement separately excludes the Vite plugin from browser payload.
const RUNTIME_FILES = [
  'index.js',
  'protocol.js',
  'wasm/tinyjoin_wasm.js',
  'worker-opfs/tinyjoin_opfs_runtime.js',
  'worker/default-entry.js',
  'worker/index.js',
  'vite/index.js',
];

// Terser settings shared by every published file. Mangling top-level names is
// safe because each file is a module, so only its exports are observable.
const TERSER_OPTIONS = {
  compress: {passes: 3},
  ecma: 2022,
  format: {
    // An application's own bundler reads these from the dynamic import that
    // loads the OPFS runtime, and must still find them after minification.
    comments: (_node, {value}) => /@vite-ignore|webpackIgnore/.test(value),
  },
  mangle: true,
  module: true,
};

// Every reference a published file makes to a sibling has to name a published
// file. A private module that is still imported rather than bundled resolves
// during the build but not in a browser, which is a mistake worth failing on.
const RUNTIME_REFERENCE =
  /(?:\bfrom|\bimport|\bnew URL\s*\()\s*\(?\s*["'](\.[^"']+)["']/g;

// An application's bundler only emits the default Worker when it can see this
// exact shape in the client bundle, and only resolves it to the right file when
// the bundle sits beside the worker directory.
const DEFAULT_WORKER_MARKERS = [
  /new Worker\(\s*new URL\(\s*(['"])\.\/worker\/default-entry\.js\1/,
  /type:\s*(['"])module\1/,
];

// The Worker must reach the private OPFS runtime through a bundler-ignored
// dynamic import, so that an application build does not pull page storage into
// the Worker entry. Quoting is the minifier's choice, not ours.
const OPFS_LOADER_MARKERS = [
  /(['"])\.\.\/worker-opfs\/tinyjoin_opfs_runtime\.js\1/,
  /@vite-ignore/,
  /webpackIgnore:\s*true/,
];
try {
  await requireWasmArtifacts(dist);
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
await assertThirdPartyNotices();

const compiler = resolve(root, 'node_modules/typescript/bin/tsc');
const compile = spawnSync(
  process.execPath,
  [compiler, '-p', resolve(root, 'tsconfig.build.json')],
  {cwd: root, stdio: 'inherit'},
);
if (compile.status !== 0) {
  process.exit(compile.status ?? 1);
}

await buildPrivateWorkerRuntime();
await bundleRuntime();
await minifyRuntime();
await assertPublishedImports();
await assertMarkers('index.js', DEFAULT_WORKER_MARKERS, 'default Worker');
for (const entry of ['worker/index.js', 'worker/default-entry.js']) {
  await assertMarkers(entry, OPFS_LOADER_MARKERS, 'OPFS loader');
}
await buildDefinitions(root, dist);

const manifest = JSON.parse(
  await readFile(resolve(root, 'package.json'), 'utf8'),
);
delete manifest.private;
delete manifest.scripts;
delete manifest.devDependencies;

manifest.types = './@types/index.d.ts';
manifest.exports = {
  '.': {
    types: './@types/index.d.ts',
    import: './index.js',
  },
  './worker': {
    types: './@types/worker/index.d.ts',
    import: './worker/index.js',
  },
  './package.json': './package.json',
  './vite': {
    types: './@types/vite/index.d.ts',
    import: './vite/index.js',
  },
};

await writeFile(
  resolve(dist, 'package.json'),
  `${JSON.stringify(manifest, null, 2)}\n`,
);
await copyFile(resolve(root, 'LICENSE'), resolve(dist, 'LICENSE'));
for (const notice of thirdPartyNoticeFiles) {
  await copyFile(resolve(root, notice), resolve(dist, notice));
}
await copyPublicMarkdown();
await mkdir(resolve(dist, 'docs'), {recursive: true});
await copyFile(
  resolve(root, 'site/guides/3_sql_compatibility.md'),
  resolve(dist, 'docs/sql.md'),
);

// Measure the built runtime into committed metadata, which the site reads to
// fill in the download sizes it publishes.
await writeSizes(dist);

async function copyPublicMarkdown() {
  await copyFile(
    resolve(root, 'site/guides/7_agents.md'),
    resolve(root, 'AGENTS.md'),
  );
  const markdown = [
    ['README.md', 'README.md'],
    ['releases.md', 'releases.md'],
    ['AGENTS.md', 'agents.md'],
  ];
  for (const [source, packageFile] of markdown) {
    await copyFile(resolve(root, source), resolve(dist, packageFile));
  }
}

async function buildPrivateWorkerRuntime() {
  await buildOpfsRuntime();
}

async function bundleRuntime() {
  const {version} = JSON.parse(
    await readFile(resolve(root, 'package.json'), 'utf8'),
  );
  // Every bundle is built from the modules tsc emitted, and only written once
  // they all are: two of them share an entry, so writing as we went would build
  // the second from the first one's output rather than from the source.
  const bundled = [];
  for (const {entry, shared, platform} of RUNTIME_BUNDLES) {
    const {outputFiles} = await esbuildBuild({
      bundle: true,
      define: {__TINYJOIN_VERSION__: JSON.stringify(version)},
      entryPoints: [resolve(dist, entry)],
      format: 'esm',
      platform: platform ?? 'browser',
      plugins: [shareModules(shared)],
      target: 'es2022',
      write: false,
    });
    bundled.push([entry, outputFiles[0].text]);
  }
  for (const [entry, source] of bundled) {
    await writeFile(resolve(dist, entry), source);
  }
  await pruneBundledModules();
}

// esbuild keeps an external import exactly as the module that imported it wrote
// it, but a bundle sits at its entry's depth rather than that module's. Give
// each shared module the specifier the bundle itself needs.
function shareModules(shared) {
  return {
    name: 'tinyjoin-shared-modules',
    setup: (build) =>
      build.onResolve({filter: /\.js$/}, ({path}) => {
        const specifier = shared[basename(path)];
        return specifier === undefined ? null : {external: true, path: specifier};
      }),
  };
}

// Publish the bundles and the modules they share, but neither the private
// implementation modules now inside them nor the declarations tsc wrote beside
// them: the published types come from src/@types instead.
async function pruneBundledModules() {
  for (const path of await listFiles(dist)) {
    if (
      path.endsWith('.d.ts') ||
      (path.endsWith('.js') && !RUNTIME_FILES.includes(path))
    ) {
      await rm(resolve(dist, path));
    }
  }
  await pruneEmptyDirectories(dist);
}

async function assertPublishedImports() {
  for (const file of RUNTIME_FILES) {
    const path = resolve(dist, file);
    const source = await readFile(path, 'utf8');
    for (const [, specifier] of source.matchAll(RUNTIME_REFERENCE)) {
      const referenced = relative(dist, resolve(dirname(path), specifier));
      if (!RUNTIME_FILES.includes(referenced)) {
        throw new Error(
          `${file} references ${specifier}, which the package does not publish`,
        );
      }
    }
  }
}

async function minifyRuntime() {
  for (const file of RUNTIME_FILES) {
    const path = resolve(dist, file);
    const {code} = await minify(await readFile(path, 'utf8'), TERSER_OPTIONS);
    await writeFile(path, code);
  }
}

async function listFiles(directory, prefix = '') {
  const entries = await readdir(resolve(directory, prefix), {
    withFileTypes: true,
  });
  const files = await Promise.all(
    entries.map((entry) => {
      const path = prefix === '' ? entry.name : `${prefix}/${entry.name}`;
      return entry.isDirectory() ? listFiles(directory, path) : [path];
    }),
  );
  return files.flat();
}

async function pruneEmptyDirectories(directory) {
  for (const entry of await readdir(directory, {withFileTypes: true})) {
    if (entry.isDirectory()) {
      const child = resolve(directory, entry.name);
      await pruneEmptyDirectories(child);
      if ((await readdir(child)).length === 0) {
        await rm(child, {recursive: true});
      }
    }
  }
}

async function buildOpfsRuntime() {
  const entry = resolve(dist, 'worker/opfs-engine.js');
  const outputDirectory = resolve(dist, 'worker-opfs');
  const output = resolve(outputDirectory, 'tinyjoin_opfs_runtime.js');

  await viteBuild({
    build: {
      codeSplitting: false,
      copyPublicDir: false,
      emptyOutDir: true,
      lib: {
        entry,
        fileName: () => 'tinyjoin_opfs_runtime.js',
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
      'Another TinyJoin worker already has this OPFS database open',
    ],
    [
      [/\bimport\s*\(/, 'a dynamic import'],
      [/tinyjoin_wasm(?:_bg)?/i, 'default WASM glue'],
      [
        /WASM returned an invalid structured response envelope/,
        'the structured WASM adapter',
      ],
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

async function assertMarkers(file, markers, label) {
  const path = resolve(dist, file);
  const source = await readFile(path, 'utf8');
  for (const marker of markers) {
    if (!marker.test(source)) {
      throw new Error(`The ${label} is missing ${marker.source}: ${path}`);
    }
  }
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}
