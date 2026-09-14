import {createHash} from 'node:crypto';
import {mkdir, readdir, readFile, writeFile} from 'node:fs/promises';
import {posix, resolve} from 'node:path';
import type {Plugin, ResolvedConfig} from 'vite';

import {
  helperSource,
  registrationSource,
  serviceWorkerSource,
} from './offline.js';

export interface TinyjoinOfflineOptions {
  mode?: 'service-worker' | 'manifest';
  navigationFallback?: string | false;
}

const generatedFiles = [
  'tinyjoin-sw.js',
  'tinyjoin-register.js',
  'tinyjoin-precache.js',
  'tinyjoin-precache.json',
];

/** Adds complete, versioned production-build caching to a Vite application. */
export function tinyjoinOffline(options: TinyjoinOfflineOptions = {}): Plugin {
  const mode = options.mode ?? 'service-worker';
  const navigationFallback = options.navigationFallback ?? 'index.html';
  const reservedFiles =
    mode === 'manifest'
      ? ['tinyjoin-precache.js', 'tinyjoin-precache.json']
      : generatedFiles;
  if (mode !== 'service-worker' && mode !== 'manifest') {
    throw new Error('tinyjoinOffline: mode must be service-worker or manifest');
  }
  if (navigationFallback !== false && !isAssetPath(navigationFallback)) {
    throw new Error(
      'tinyjoinOffline: navigationFallback must be a relative emitted HTML path or false',
    );
  }
  let config: ResolvedConfig;
  let written = false;
  return {
    name: 'tinyjoin-offline',
    apply: (userConfig, environment) =>
      environment.command === 'build' && !userConfig.build?.ssr,
    enforce: 'post',
    async configResolved(resolved) {
      config = resolved;
      if (config.build.lib || !config.build.write) {
        throw new Error(
          'tinyjoinOffline requires a written Vite application build',
        );
      }
      if (
        config.base !== './' &&
        config.base !== '' &&
        !/^\/(?!\/)(?:[^?#]*\/)?$/.test(config.base)
      ) {
        throw new Error(
          'tinyjoinOffline requires a same-origin root-relative base or ./',
        );
      }
      if (config.publicDir) {
        for (const file of reservedFiles) {
          try {
            await readFile(resolve(config.publicDir, file));
          } catch (error) {
            if ((error as NodeJS.ErrnoException).code === 'ENOENT') continue;
            throw error;
          }
          throw new Error(
            `tinyjoinOffline reserves ${file}; remove the conflicting public file or use your own offline integration`,
          );
        }
      }
    },
    transformIndexHtml: {
      order: 'post',
      handler: (_html, context) =>
        mode === 'manifest'
          ? []
          : [
              {
                tag: 'script',
                attrs: {
                  defer: true,
                  src:
                    config.base === './' || config.base === ''
                      ? posix.relative(
                          posix.dirname(context.path),
                          '/tinyjoin-register.js',
                        )
                      : `${config.base}tinyjoin-register.js`,
                },
                injectTo: 'head',
              },
            ],
    },
    writeBundle() {
      written = true;
    },
    closeBundle: {
      order: 'post',
      sequential: true,
      async handler() {
        if (!written) return;
        const directory = resolve(config.root, config.build.outDir);
        await mkdir(directory, {recursive: true});
        if (mode === 'service-worker') {
          await writeFile(
            resolve(directory, 'tinyjoin-register.js'),
            registrationSource(),
          );
        }
        const files = (await listFiles(directory))
          .filter(
            (file) =>
              !reservedFiles.includes(file) ||
              (mode === 'service-worker' && file === 'tinyjoin-register.js'),
          )
          .sort();
        if (
          navigationFallback !== false &&
          !files.includes(navigationFallback)
        ) {
          throw new Error(
            `tinyjoinOffline navigation fallback ${navigationFallback} is not in the build; choose an emitted HTML file or set navigationFallback: false`,
          );
        }
        const assets = await Promise.all(
          files.map(async (file) => ({
            url: file.split('/').map(encodeURIComponent).join('/'),
            revision: createHash('sha256')
              .update(await readFile(resolve(directory, file)))
              .digest('hex'),
          })),
        );
        const manifest = {
          generator: 'tinyjoinOffline',
          version: '',
          base: config.base,
          navigationFallback,
          assets,
        };
        // A helper-only upgrade needs its own cache too: a failed replacement
        // installation must never delete the cache used by the active worker.
        manifest.version = createHash('sha256')
          .update(JSON.stringify(manifest))
          .update(helperSource(manifest))
          .update(serviceWorkerSource())
          .digest('hex')
          .slice(0, 24);
        await writeFile(
          resolve(directory, 'tinyjoin-precache.json'),
          `${JSON.stringify(manifest, null, 2)}\n`,
        );
        await writeFile(
          resolve(directory, 'tinyjoin-precache.js'),
          helperSource(manifest),
        );
        if (mode === 'service-worker') {
          await writeFile(
            resolve(directory, 'tinyjoin-sw.js'),
            serviceWorkerSource(),
          );
        }
      },
    },
  };
}

const isAssetPath = (value: string): boolean =>
  value !== '' &&
  !value.startsWith('/') &&
  !value.includes('\\') &&
  !value.includes('?') &&
  !value.includes('#') &&
  value
    .split('/')
    .every((part) => part !== '.' && part !== '..' && part !== '');

async function listFiles(directory: string, prefix = ''): Promise<string[]> {
  const paths: string[] = [];
  for (const entry of await readdir(resolve(directory, prefix), {
    withFileTypes: true,
  })) {
    const path = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) paths.push(...(await listFiles(directory, path)));
    else if (entry.isFile()) paths.push(path);
    else
      throw new Error(
        `tinyjoinOffline cannot precache non-regular build output: ${path}`,
      );
  }
  return paths;
}
