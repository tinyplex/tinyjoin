import {mkdir, readFile, rm, writeFile} from 'node:fs/promises';
import {dirname, resolve} from 'node:path';

const TYPES_DOC_CODE_BLOCKS =
  /\/\/\/\s*(\S*)(.*?)(?=(\s*\/\/\/)|(\n\n)|(\n$))/gs;
const TYPES_DOC_BLOCKS = /(\/\*\*.*?\*\/)\s*\/\/\/\s*(\S*)/gs;
// How the declaration of a function, a constructor, or a method begins. Each
// of those is something an application calls, so its documentation shows a
// call.
const TYPES_DOC_CALLABLE =
  /^\s*(?:export\s+function\b|constructor\(|[\w$]+\??(?:<[^(]*>)?\()/;

const modules = ['', 'worker', 'vite', 'node', 'drizzle', 'kysely'];

export async function buildDefinitions(root, dist) {
  const typesDirectory = resolve(dist, '@types');
  await rm(typesDirectory, {force: true, recursive: true});

  const labels = new Set();
  await Promise.all(
    modules.map(async (module) => {
      const sourceDirectory = resolve(
        root,
        'src/@types',
        module,
      );
      const docs = await readFile(resolve(sourceDirectory, 'docs.js'), 'utf8');
      const blocks = new Map();
      const unexampled = [];

      for (const [, block, label] of docs.matchAll(TYPES_DOC_BLOCKS)) {
        if (labels.has(label)) {
          throw new Error(`Duplicate public documentation label: ${label}`);
        }
        labels.add(label);
        blocks.set(label, block);
      }

      const declaration = await readFile(
        resolve(sourceDirectory, 'index.d.ts'),
        'utf8',
      );
      const sourceEntrypoint = await readFile(
        resolve(root, 'src', ...(module ? [module, 'index.ts'] : ['index.ts'])),
        'utf8',
      );
      assertPublicExports(sourceEntrypoint, declaration, module || 'tinyjoin');
      const output = declaration.replace(
        TYPES_DOC_CODE_BLOCKS,
        (_, label, code) => {
          const block = blocks.get(label);
          if (block === undefined) {
            throw new Error(
              `Missing public documentation label ${label} in ${module || 'tinyjoin'}`,
            );
          }
          blocks.delete(label);
          if (TYPES_DOC_CALLABLE.test(code) && !block.includes('@example')) {
            unexampled.push(label);
          }
          return `${block}${code}`;
        },
      );

      if (unexampled.length > 0) {
        throw new Error(
          `Public functions and methods without an @example in ${
            module || 'tinyjoin'
          }: ${unexampled.join(', ')}`,
        );
      }

      if (blocks.size > 0) {
        throw new Error(
          `Unused public documentation labels in ${module || 'tinyjoin'}: ${[
            ...blocks.keys(),
          ].join(', ')}`,
        );
      }

      const destination = resolve(typesDirectory, module, 'index.d.ts');
      await mkdir(dirname(destination), {recursive: true});
      await writeFile(destination, output);
    }),
  );
}

function assertPublicExports(source, declaration, module) {
  const sourceExports = getExportNames(source, `${module} source`);
  const declarationExports = getExportNames(
    declaration,
    `${module} declaration`,
  );
  const missing = [...sourceExports].filter(
    (name) => !declarationExports.has(name),
  );
  const extra = [...declarationExports].filter(
    (name) => !sourceExports.has(name),
  );
  if (missing.length > 0 || extra.length > 0) {
    throw new Error(
      `Public export mismatch in ${module}: missing [${missing.sort().join(', ')}], extra [${extra.sort().join(', ')}]`,
    );
  }
}

function getExportNames(source, label) {
  if (/^export\s+(?:default|\*)/m.test(source)) {
    throw new Error(`Unsupported public export form in ${label}`);
  }

  const names = new Set(
    [...source.matchAll(
      /^export\s+(?:declare\s+)?(?:async\s+)?(?:type|interface|class|function|const|let|var|enum|namespace)\s+([A-Za-z_$][\w$]*)/gm,
    )].map(([, name]) => name),
  );
  for (const [, exports] of source.matchAll(
    /\bexport\s+(?:type\s+)?\{([^}]*)\}/gs,
  )) {
    for (const entry of exports.split(',')) {
      const name = entry
        .trim()
        .replace(/^type\s+/, '')
        .split(/\s+as\s+/)
        .at(-1);
      if (name) {
        names.add(name);
      }
    }
  }
  return names;
}
