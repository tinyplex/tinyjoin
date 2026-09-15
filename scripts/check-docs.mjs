import {readFile, readdir} from 'node:fs/promises';
import {extname, join, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {getPackageDocumentation} from './package-documentation.mjs';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const requiredFiles = [
  '.nojekyll',
  'CNAME',
  'favicon.svg',
  'index.html',
  'api/index.html',
  'api/node/index.html',
  'guides/index.html',
  'guides/node/index.html',
  'demos/index.html',
  'llms.txt',
  'llms-full.txt',
  'css/index.css',
  'js/site.js',
];

export async function checkDocs(
  docs = resolve(root, 'docs'),
  {checkPackageCopies = true} = {},
) {
  const errors = [];
  const files = await getFiles(docs);
  const relativeFiles = new Set(files.map((file) => relative(docs, file)));

  for (const file of requiredFiles) {
    if (!relativeFiles.has(file)) {
      errors.push(`Missing generated documentation file: ${file}`);
    }
  }

  if (
    (await readFile(resolve(docs, 'CNAME'), 'utf8')).trim() !== 'tinyjoin.org'
  ) {
    errors.push('The generated CNAME must contain tinyjoin.org');
  }

  const homepage = await readFile(resolve(docs, 'index.html'), 'utf8');
  const releasePage = await readFile(
    resolve(docs, 'guides/releases/index.html'),
    'utf8',
  );
  const favicon = await readFile(resolve(docs, 'favicon.svg'), 'utf8');
  const stylesheet = await readFile(resolve(docs, 'css/index.css'), 'utf8');
  if (!homepage.includes('<nav id="actions" aria-label="Get started">')) {
    errors.push('The homepage must contain its explicitly scoped action links');
  }
  if (
    !homepage.includes(
      '<link rel="icon" type="image/svg+xml" href="/favicon.svg">',
    )
  ) {
    errors.push('The homepage must link to the generated SVG favicon');
  }
  if (
    !homepage.includes(
      '<a class="wordmark" href="/" aria-current="page"><img src="/favicon.svg" alt="TinyJoin logo"><span>Tiny<em>Join</em></span>',
    )
  ) {
    errors.push('The header must show the TinyJoin logo beside the wordmark');
  }
  if (
    !homepage.includes(
      '<em><img src="/favicon.svg" alt="Large TinyJoin logo" width="100%" height="100%"></em><section id="hero">',
    )
  ) {
    errors.push('The homepage must show the large TinyJoin logo before its hero');
  }
  if (/article#home\s*>\s*p\s*>\s*a/.test(stylesheet)) {
    errors.push('Homepage button styles must not target ordinary paragraphs');
  }
  if (/article#home\s*>\s*hr\s*~/.test(stylesheet)) {
    errors.push('Homepage content after a divider must retain the paired grid');
  }
  if (!favicon.includes('fill="#7C3AED"')) {
    errors.push('The generated favicon must use the TinyJoin purple');
  }
  if (!stylesheet.includes('--accent:#7c3aed')) {
    errors.push('The site accent must match the final TinyJoin logo');
  }
  const agentIndex = await readFile(resolve(docs, 'llms.txt'), 'utf8');
  const agentReference = await readFile(resolve(docs, 'llms-full.txt'), 'utf8');
  if (!agentIndex.includes('https://tinyjoin.org/llms-full.txt')) {
    errors.push('The agent index must link to the combined text reference');
  }
  for (const content of [
    '# SQL compatibility',
    '## Hard limits',
    'STORAGE_COMMIT_OUTCOME_UNKNOWN',
    'export interface Transaction',
    'export class Client',
    '# Public API: tinyjoin/node',
    "export function create(dataDir?: 'memory://'): Promise<Client>",
    'Source: https://tinyjoin.org/guides/node/',
    '# Public API: tinyjoin/worker',
    'export function startWorker',
    'Source: https://tinyjoin.org/guides/caveats/',
    '](https://tinyjoin.org/guides/caveats/#if-tinyjoin-is-not-the-right-fit)',
  ]) {
    if (!agentReference.includes(content)) {
      errors.push(`The combined agent reference is missing ${content}`);
    }
  }
  for (const [name, html] of [
    ['homepage', homepage],
    ['release page', releasePage],
  ]) {
    if (/<a\b[^>]*href="https:\/\/tinyjoin\.org\//.test(html)) {
      errors.push(`The ${name} must use root-relative internal links`);
    }
  }

  if (checkPackageCopies) {
    await checkMarkdownCopies(errors);
  }

  const htmlFiles = files.filter((file) => extname(file) === '.html');
  for (const file of htmlFiles) {
    const html = await readFile(file, 'utf8');
    const sourcePath = relative(docs, file);

    if (/\[<\/code>|\]\(\/api\//.test(html)) {
      errors.push(`Malformed TinyDocs API autolink in ${sourcePath}`);
    }

    for (const match of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
      const href = match[1];
      if (
        href.startsWith('http:') ||
        href.startsWith('https:') ||
        href.startsWith('mailto:') ||
        href.startsWith('//')
      ) {
        continue;
      }

      // Resolve document-local anchors against this HTML file, including the
      // independently served main.html fragments and generated demo pages.
      const url = new URL(href, new URL(sourcePath, 'https://tinyjoin.org/'));
      let target = decodeURIComponent(url.pathname).replace(/^\//, '');
      target = target === '' ? 'index.html' : target;
      if (target.endsWith('/')) {
        target += 'index.html';
      }
      if (!relativeFiles.has(target)) {
        errors.push(`${sourcePath} links to missing /${target}`);
        continue;
      }

      if (url.hash !== '' && extname(target) === '.html') {
        const targetHtml = await readFile(resolve(docs, target), 'utf8');
        const id = decodeURIComponent(url.hash.slice(1));
        if (!targetHtml.includes(`id="${id}"`)) {
          errors.push(`${sourcePath} links to missing #${id} in /${target}`);
        }
      }
    }
  }

  if (errors.length > 0) {
    throw new Error(`Documentation check failed:\n- ${errors.join('\n- ')}`);
  }

  console.log(
    `Checked ${htmlFiles.length} HTML pages and their internal documentation links.`,
  );
}

async function checkMarkdownCopies(errors) {
  const markdownCopies = [
    ['README.md', 'dist/README.md'],
    ['releases.md', 'dist/releases.md'],
    ['site/guides/7_agents.md', 'AGENTS.md'],
  ];
  for (const paths of markdownCopies) {
    const [source, ...copies] = await Promise.all(
      paths.map((path) => readFile(resolve(root, path), 'utf8')),
    );
    paths.slice(1).forEach((path, index) => {
      if (copies[index] !== source) {
        errors.push(`${path} is out of sync with ${paths[0]}`);
      }
    });
  }

  for (const [path, expected] of Object.entries(await getPackageDocumentation(root))) {
    if (await readFile(resolve(root, 'dist', path), 'utf8') !== expected) {
      errors.push(`dist/${path} is out of sync with its documentation source`);
    }
  }

  for (const path of ['README.md', 'releases.md']) {
    const markdown = await readFile(resolve(root, path), 'utf8');
    if (/(?:href|src)="\/|\]\(\//.test(markdown)) {
      errors.push(`${path} must use absolute internal links`);
    }
    if (!markdown.includes('https://tinyjoin.org/')) {
      errors.push(`${path} must contain an absolute tinyjoin.org link`);
    }
  }
}

export async function getFiles(directory) {
  const entries = await readdir(directory, {withFileTypes: true});
  const files = await Promise.all(
    entries.map((entry) => {
      const path = join(directory, entry.name);
      return entry.isDirectory() ? getFiles(path) : [path];
    }),
  );
  return files.flat();
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  await checkDocs();
}
