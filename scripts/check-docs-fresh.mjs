import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

import {buildDefinitions} from './build-definitions.mjs';
import {checkDocs, getFiles} from './check-docs.mjs';
import {build as buildDocs} from '../site/build.mjs';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const temporaryRoot = await mkdtemp(join(tmpdir(), 'tinygres-docs-'));
const temporaryDist = resolve(temporaryRoot, 'dist');
const temporaryDocs = resolve(temporaryRoot, 'docs');

try {
  await writeFile(
    resolve(temporaryRoot, 'package.json'),
    '{"name":"tinygres","private":true,"type":"module"}\n',
  );
  await buildDefinitions(root, temporaryDist);
  await buildDocs(temporaryDocs, resolve(temporaryDist, '@types'));
  await checkDocs(temporaryDocs, {checkPackageCopies: false});
  await assertDirectoriesEqual(resolve(root, 'docs'), temporaryDocs);
  console.log('Committed docs match a fresh TinyDocs build.');
} finally {
  await rm(temporaryRoot, {force: true, recursive: true});
}

async function assertDirectoriesEqual(committed, generated) {
  const committedFiles = await getFiles(committed);
  const generatedFiles = await getFiles(generated);
  const committedByPath = new Map(
    committedFiles.map((file) => [relative(committed, file), file]),
  );
  const generatedByPath = new Map(
    generatedFiles.map((file) => [relative(generated, file), file]),
  );
  const missing = [...generatedByPath.keys()].filter(
    (path) => !committedByPath.has(path),
  );
  const extra = [...committedByPath.keys()].filter(
    (path) => !generatedByPath.has(path),
  );
  const changed = [];
  for (const [path, generatedFile] of generatedByPath) {
    const committedFile = committedByPath.get(path);
    if (
      committedFile &&
      !(await readFile(committedFile)).equals(await readFile(generatedFile))
    ) {
      changed.push(path);
    }
  }

  if (missing.length > 0 || extra.length > 0 || changed.length > 0) {
    throw new Error(
      [
        'Committed docs are stale. Run npm run build:docs.',
        `Missing: ${missing.sort().join(', ') || 'none'}`,
        `Extra: ${extra.sort().join(', ') || 'none'}`,
        `Changed: ${changed.sort().join(', ') || 'none'}`,
      ].join('\n'),
    );
  }
}
