import {existsSync, readFileSync} from 'node:fs';
import {access, cp, mkdir} from 'node:fs/promises';
import {dirname, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

import {createDocs, getSorter} from 'tinydocs';
import {MarkdownPage} from './ui/MarkdownPage.tsx';
import {Page} from './ui/Page.tsx';

const GROUPS = ['Classes', 'Interfaces', 'Functions', '*', 'Type aliases'];
const CATEGORIES = [
  'Lifecycle',
  'SQL',
  'Transactions',
  'Query results',
  'Data types',
  'Subscriptions',
  'Configuration',
  'Errors',
  'Workers',
  '*',
];
const REFLECTIONS = [
  'tinyjoin',
  'worker',
  'create',
  'Client',
  'ClientError',
  '*',
];

const RUNTIME_FILES = [
  'index.js',
  'protocol.js',
  'client',
  'worker',
  'wasm',
  'worker-opfs',
];

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const hideInheritedErrorMembers = (reflection: any): void => {
  if (reflection.name !== 'ClientError' || reflection.children == null) {
    return;
  }

  const ownChildren = new Set(
    reflection.children.filter((child: any) => child.inheritedFrom == null),
  );
  reflection.children = [...ownChildren];
  for (const group of reflection.groups ?? []) {
    group.children = group.children.filter((child: any) =>
      ownChildren.has(child),
    );
    for (const category of group.categories ?? []) {
      category.children = category.children.filter((child: any) =>
        ownChildren.has(child),
      );
    }
    group.categories = group.categories?.filter(
      (category: any) => category.children.length > 0,
    );
  }
  reflection.groups = reflection.groups?.filter(
    (group: any) => group.children.length > 0,
  );
  for (const category of reflection.categories ?? []) {
    category.children = category.children.filter((child: any) =>
      ownChildren.has(child),
    );
  }
  reflection.categories = reflection.categories?.filter(
    (category: any) => category.children.length > 0,
  );
};

export const build = async (
  outDir = 'docs',
  typesDir = 'dist/@types',
  publicMarkdownDir = repositoryRoot,
  packageDir = resolve(typesDir, '..'),
): Promise<void> => {
  const docs = createDocs('https://tinyjoin.org', outDir)
    .addJsFile('site/js/site.ts')
    .addLessFile('site/less/index.less')
    .addDir('site/fonts', 'fonts')
    .addDir('site/extras')
    .addReflectionTransform(hideInheritedErrorMembers)
    .addApiFile(resolve(typesDir, 'index.d.ts'))
    .addApiFile(resolve(typesDir, 'worker/index.d.ts'))
    .addRootMarkdownFile('site/home/index.md')
    .addMarkdownDir('site/guides')
    .addMarkdownDir('site/demos', true)
    .addStringFile(
      readFileSync('site/guides/6_agents.md', 'utf8'),
      'llms-full.txt',
    );

  await docs.generateNodes({
    group: getSorter(GROUPS),
    category: getSorter(CATEGORIES),
    reflection: getSorter(REFLECTIONS),
  });

  docs
    .addPageForEachNode('/', Page)
    .addPageForNode('/api/', Page, 'all.html', true)
    .addMarkdownForNode(
      '/',
      MarkdownPage,
      getOutputPath(outDir, '/', resolve(publicMarkdownDir, 'README.md')),
    )
    .addMarkdownForNode(
      '/guides/releases/',
      MarkdownPage,
      getOutputPath(
        outDir,
        '/guides/releases/',
        resolve(publicMarkdownDir, 'releases.md'),
      ),
    )
    .addMarkdownForNode(
      '/',
      MarkdownPage,
      getOutputPath(outDir, '/', resolve(packageDir, 'README.md')),
    )
    .addMarkdownForNode(
      '/guides/releases/',
      MarkdownPage,
      getOutputPath(
        outDir,
        '/guides/releases/',
        resolve(packageDir, 'releases.md'),
      ),
    )
    .publish();

  await waitForFile(resolve(outDir, 'css/index.css'));
  await copyRuntime(outDir, packageDir);
};

const copyRuntime = async (
  outDir: string,
  packageDir: string,
): Promise<boolean> => {
  const present = RUNTIME_FILES.filter((file) =>
    existsSync(resolve(packageDir, file)),
  );
  if (present.length !== RUNTIME_FILES.length) {
    return false;
  }
  const libDir = resolve(outDir, 'lib');
  await mkdir(libDir, {recursive: true});
  for (const file of present) {
    await cp(resolve(packageDir, file), resolve(libDir, file), {
      recursive: true,
    });
  }
  return true;
};

const getOutputPath = (
  outDir: string,
  nodeUrl: string,
  destination: string,
): string => relative(resolve(outDir, `.${nodeUrl}`), destination);

const waitForFile = async (file: string): Promise<void> => {
  for (let attempt = 0; attempt < 500; attempt++) {
    try {
      await access(file);
      return;
    } catch {
      await new Promise((resolveWait) => setTimeout(resolveWait, 10));
    }
  }
  throw new Error(`Timed out waiting for generated file: ${file}`);
};
