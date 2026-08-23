import {readFile, readdir} from 'node:fs/promises';
import {extname, join, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const requiredFiles = [
  '.nojekyll',
  'CNAME',
  'favicon.svg',
  'index.html',
  'api/index.html',
  'guides/index.html',
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
    (await readFile(resolve(docs, 'CNAME'), 'utf8')).trim() !== 'tinygres.org'
  ) {
    errors.push('The generated CNAME must contain tinygres.org');
  }

  const homepage = await readFile(resolve(docs, 'index.html'), 'utf8');
  const releasePage = await readFile(
    resolve(docs, 'guides/releases/index.html'),
    'utf8',
  );
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
      '<a class="wordmark" href="/" aria-current="page"><img src="/favicon.svg" alt="TinyGres logo">',
    )
  ) {
    errors.push('The header must show the TinyGres logo beside the wordmark');
  }
  if (
    !homepage.includes(
      '<em><img src="/favicon.svg" alt="Large TinyGres logo" width="100%" height="100%"></em><section id="hero">',
    )
  ) {
    errors.push('The homepage must show the large TinyGres logo before its hero');
  }
  if (/article#home\s*>\s*p\s*>\s*a/.test(stylesheet)) {
    errors.push('Homepage button styles must not target ordinary paragraphs');
  }
  if (/article#home\s*>\s*hr\s*~/.test(stylesheet)) {
    errors.push('Homepage content after a divider must retain the paired grid');
  }
  if (!stylesheet.includes('--accent:#336791')) {
    errors.push('The site accent must match the final TinyGres logo');
  }
  if (!/body>header>nav\{display:none/.test(stylesheet)) {
    errors.push('The primary navigation must be hidden on mobile');
  }
  if (
    !/@media \(min-width:38rem\)\{body>header>nav\{display:flex/.test(
      stylesheet,
    )
  ) {
    errors.push('The primary navigation must reappear at the desktop breakpoint');
  }
  for (const [name, html] of [
    ['homepage', homepage],
    ['release page', releasePage],
  ]) {
    if (/<a\b[^>]*href="https:\/\/tinygres\.org\//.test(html)) {
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
        href.startsWith('#') ||
        href.startsWith('http:') ||
        href.startsWith('https:') ||
        href.startsWith('mailto:') ||
        href.startsWith('//')
      ) {
        continue;
      }

      const url = new URL(href, 'https://tinygres.org/');
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
    ['site/guides/6_agents.md', 'AGENTS.md', 'dist/agents.md'],
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

  for (const path of ['README.md', 'releases.md']) {
    const markdown = await readFile(resolve(root, path), 'utf8');
    if (/(?:href|src)="\/|\]\(\//.test(markdown)) {
      errors.push(`${path} must use absolute internal links`);
    }
    if (!markdown.includes('https://tinygres.org/')) {
      errors.push(`${path} must contain an absolute tinygres.org link`);
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
