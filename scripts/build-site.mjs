import {build as esbuild} from 'esbuild';
import {mkdir, mkdtemp, rm} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));

export async function withSiteBuild(callback) {
  await mkdir(resolve(root, 'tmp'), {recursive: true});
  const outDir = await mkdtemp(resolve(root, 'tmp/site-'));
  try {
    const outfile = join(outDir, 'build.mjs');
    await esbuild({
      absWorkingDir: root,
      entryPoints: [resolve(root, 'site/build.ts')],
      external: ['tinydocs', 'react', 'react/jsx-runtime', 'less', 'typedoc'],
      bundle: true,
      format: 'esm',
      platform: 'node',
      target: 'esnext',
      tsconfig: resolve(root, 'site/tsconfig.json'),
      outfile,
    });
    const {build} = await import(pathToFileURL(outfile).href);
    return await callback(build);
  } finally {
    await rm(outDir, {force: true, recursive: true});
  }
}
