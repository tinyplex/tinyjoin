import {existsSync, readFileSync, writeFileSync} from 'node:fs';
import {access, cp, mkdir} from 'node:fs/promises';
import {relative, resolve} from 'node:path';

import type {Docs, Node} from 'tinydocs';
import {createDocs, getSorter} from 'tinydocs';
import {writePackageDocumentation} from '../scripts/package-documentation.mjs';
import {MainInner} from './ui/MainInner.tsx';
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
  'node',
  'worker',
  'create',
  'Client',
  'ClientError',
  '*',
];
const sortGuides = getSorter([
  'Getting started',
  'Caveats',
  'SQL compatibility',
  'Storage and lifecycle',
  'Transactions and changes',
  'Custom Workers',
  'Node',
  'Offline',
  '*',
  'Agents guide',
  'Releases',
]);

// Download sizes are measured from the built runtime into site/data/sizes.json
// during npm run build, and substituted into the markdown that publishes them.
// Reading committed metadata, rather than dist, keeps this build hermetic for
// the documentation freshness check and the site workflow.
type Sizes = {[group: string]: {raw: number; gzip: number; gzipLabel: string}};

const getSizeReplacers = (): [RegExp, string][] => {
  const sizes: Sizes = JSON.parse(readFileSync('site/data/sizes.json', 'utf8'));
  return Object.entries(sizes).map(([group, {gzipLabel}]) => [
    new RegExp(`\\{\\{sizes\\.${group}\\.gzip\\}\\}`, 'g'),
    gzipLabel,
  ]);
};

const RUNTIME_FILES = [
  'index.js',
  'protocol.js',
  'worker',
  'wasm',
  'worker-opfs',
];

// This module is bundled into a temporary directory before it runs, so the
// repository root comes from the working directory, like every other path here.
const repositoryRoot = process.cwd();

// TinyDocs groups @essential members under the text of their tag. There is only
// one such group, so give it empty markdown, which makes TinyDocs skip it and
// list its members directly under The Essentials, as it does any lone child.
const collapseLoneEssentialGroup = (node: Node): void => {
  if (node.name === 'The Essentials' && node.children.length === 1) {
    const [group] = node.children;
    group.summary = '';
    group.body = '';
  }
};

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

// TypeDoc copies create's shared documentation to each overload. Show the
// introduction and examples once, retaining distinct text and signature tags.
const deduplicateCreateDocumentation = (reflection: any): void => {
  if (reflection.name !== 'create' || reflection.signatures?.length < 2) {
    return;
  }
  const [first, ...overloads] = reflection.signatures ?? [];
  if (first?.comment == null) {
    return;
  }
  const summary = (comment: any): string =>
    JSON.stringify(comment.constructor.serializeDisplayParts(comment.summary));
  const firstSummary = summary(first.comment);
  const firstTags = new Set(
    first.comment.blockTags.map((tag: any) => JSON.stringify(tag.toObject())),
  );
  for (const overload of overloads) {
    if (overload.comment == null) {
      continue;
    }
    const comment = (overload.comment = overload.comment.clone());
    if (summary(comment) === firstSummary) {
      comment.summary = [];
    }
    comment.blockTags = comment.blockTags.filter(
      (tag: any) =>
        ['@param', '@returns', '@category', '@essential'].includes(tag.tag) ||
        !firstTags.has(JSON.stringify(tag.toObject())),
    );
  }
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
    .addReflectionTransform(deduplicateCreateDocumentation)
    .addNodeTransform(collapseLoneEssentialGroup)
    .addNodeTransform((node) => {
      if (node.url === '/guides/') {
        node.children.sort((left, right) => sortGuides(left.name, right.name));
      }
    })
    .addApiFile(resolve(typesDir, 'index.d.ts'))
    .addApiFile(resolve(typesDir, 'node/index.d.ts'))
    .addApiFile(resolve(typesDir, 'worker/index.d.ts'))
    .addApiFile(resolve(typesDir, 'vite/index.d.ts'))
    .addRootMarkdownFile('site/home/index.md')
    .addMarkdownDir('site/guides')
    .addMarkdownDir('site/demos', true);

  const sizeReplacers = getSizeReplacers();
  for (const [pattern, replacement] of sizeReplacers) {
    docs.addReplacer(pattern, replacement);
  }

  await docs.generateNodes({
    group: getSorter(GROUPS),
    category: getSorter(CATEGORIES),
    reflection: getSorter(REFLECTIONS),
  });

  docs
    .addStringFile(
      getFullReference(docs, typesDir, sizeReplacers),
      'llms-full.txt',
    )
    .addPageForEachNode('/', Page)
    .addPageForEachNode('/', MainInner, 'main.html')
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

  fixCombinedApiLinks(outDir);
  await waitForFile(resolve(outDir, 'css/index.css'));
  writeSearchIndex(docs, outDir);
  await writePackageDocumentation(repositoryRoot, packageDir);
  await copyRuntime(outDir, packageDir);
};

const fixCombinedApiLinks = (outDir: string): void => {
  // TinyDocs single-page mode turns every node link into a local fragment,
  // including navigation outside the rendered API tree. Keep API anchors local
  // and let links to the homepage, guides, and demos navigate to their pages.
  const file = resolve(outDir, 'api/all.html');
  const html = readFileSync(file, 'utf8');
  writeFileSync(
    file,
    html.replaceAll(/href="#(\/(?!api\/)[^"]*)"/g, 'href="$1"'),
  );
};

// Use the same guide nodes and generated declarations as this build, including
// fresh temporary declarations in check:docs:committed. Never read the previous
// docs output or embed local paths/timestamps in this portable reference.
const getFullReference = (
  docs: Docs,
  typesDir: string,
  sizeReplacers: [RegExp, string][],
): string => {
  const sections = [
    '# TinyJoin full reference',
    'All guides and documented public TypeScript declarations from this build. ' +
      'Source links identify the corresponding website pages.',
  ];
  docs.forEachNode((node) => {
    if (node.publish && node.url.startsWith('/guides/')) {
      const source = `https://tinyjoin.org${node.url}`;
      sections.push(
        `# ${node.name}\n\nSource: ${source}\n\n` +
          absoluteReferenceLinks(
            [node.summary, node.body].filter(Boolean).join('\n\n'),
            source,
          ),
      );
    }
  });
  for (const [module, declaration, source] of [
    ['tinyjoin', 'index.d.ts', 'https://tinyjoin.org/api/tinyjoin/'],
    ['tinyjoin/node', 'node/index.d.ts', 'https://tinyjoin.org/api/node/'],
    ['tinyjoin/worker', 'worker/index.d.ts', 'https://tinyjoin.org/api/worker/'],
    ['tinyjoin/vite', 'vite/index.d.ts', 'https://tinyjoin.org/api/vite/'],
  ]) {
    sections.push(
      `# Public API: ${module}\n\nSource: ${source}\n\n` +
        '````ts\n' +
        absoluteReferenceLinks(
          readFileSync(resolve(typesDir, declaration), 'utf8').trim(),
          source,
        ) +
        '\n````',
    );
  }
  let reference = sections.join('\n\n') + '\n';
  for (const [pattern, replacement] of sizeReplacers) {
    reference = reference.replaceAll(pattern, replacement);
  }
  return reference;
};

// A fragment belongs to its original guide, not to the combined document.
// Preserve external URLs and source code while resolving authored Markdown and
// HTML links that begin at the site root or the current page's fragment.
const absoluteReferenceLinks = (markdown: string, source: string): string =>
  markdown.replaceAll(
    /(\]\(|(?:href|src)=["'])(\/(?!\/)[^)\s"']*|#[^)\s"']*)/g,
    (_match, prefix: string, target: string) =>
      prefix + new URL(target, source).href,
  );

const writeSearchIndex = (docs: Docs, outDir: string): void => {
  const pages: {u: string; n: string; s: string}[] = [];
  docs.forEachNode((node) => {
    const summary = (node.summary ?? '')
      .replaceAll(/<[^>]*>/g, '')
      .replaceAll(/\s+/g, ' ')
      .trim();
    if (node.publish && node.url !== '/' && summary && !summary.startsWith('->')) {
      pages.push({u: node.url, n: node.name, s: summary});
    }
  });
  writeFileSync(resolve(outDir, 'pages.json'), JSON.stringify(pages), 'utf-8');
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
