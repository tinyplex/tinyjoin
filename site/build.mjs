import {readFileSync} from 'node:fs';
import {access} from 'node:fs/promises';
import {dirname, relative, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

import {createElement as h} from 'react';
import {
  createDocs,
  getSkippedChildren,
  getSorter,
  Markdown,
  NodeBreadcrumbs,
  NodeName,
  NodeNavigation,
  NodeSection,
  NodeSummary,
  useBaseUrl,
  usePageNode,
  useRootNode,
} from 'tinydocs';

const groups = ['Classes', 'Interfaces', 'Functions', '*', 'Type aliases'];
const categories = [
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
const reflections = [
  'tinyjoin',
  'worker',
  'create',
  'Client',
  'ClientError',
  '*',
];
const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const hideInheritedErrorMembers = (reflection) => {
  if (reflection.name !== 'ClientError' || reflection.children == null) {
    return;
  }

  const ownChildren = new Set(
    reflection.children.filter((child) => child.inheritedFrom == null),
  );
  reflection.children = [...ownChildren];
  for (const group of reflection.groups ?? []) {
    group.children = group.children.filter((child) => ownChildren.has(child));
    for (const category of group.categories ?? []) {
      category.children = category.children.filter((child) =>
        ownChildren.has(child),
      );
    }
    group.categories = group.categories?.filter(
      (category) => category.children.length > 0,
    );
  }
  reflection.groups = reflection.groups?.filter(
    (group) => group.children.length > 0,
  );
  for (const category of reflection.categories ?? []) {
    category.children = category.children.filter((child) =>
      ownChildren.has(child),
    );
  }
  reflection.categories = reflection.categories?.filter(
    (category) => category.children.length > 0,
  );
};

const PageContent = ({page}) => {
  if (page.reflection != null) {
    return h(NodeSection, {node: page});
  }

  const children = getSkippedChildren(page);
  return h(
    'section',
    {className: 's1 page-content', id: page.url, 'data-id': page.id},
    h('h1', null, h(NodeName, {node: page})),
    page.summary ? h(Markdown, {markdown: page.summary, html: true}) : null,
    page.body ? h(Markdown, {markdown: page.body, html: true}) : null,
    children.length === 0
      ? null
      : h(
          'ul',
          {className: 'page-list'},
          ...children.map((child) =>
            h(
              'li',
              {key: child.id},
              h('h2', null, h(NodeName, {node: child, link: true})),
              h(NodeSummary, {node: child, readMore: true}),
            ),
          ),
        ),
  );
};

const Wordmark = () => h('span', null, 'Tiny', h('em', null, 'Join'));

const Header = ({page}) => {
  const currentSection = page.url.split('/')[1];
  const sectionLink = (section, label) =>
    h(
      'a',
      {
        href: `/${section}/`,
        'aria-current': currentSection === section ? 'page' : undefined,
      },
      label,
    );

  return h(
    'header',
    null,
    h(
      'a',
      {
        className: 'wordmark',
        href: '/',
        'aria-current': page.url === '/' ? 'page' : undefined,
      },
      h('img', {src: '/favicon.svg', alt: 'TinyJoin logo'}),
      h(Wordmark),
    ),
    h(
      'nav',
      {'aria-label': 'Primary'},
      h(
        'ul',
        null,
        h('li', null, sectionLink('guides', 'Guides')),
        h('li', null, sectionLink('api', 'API')),
        h('li', null, sectionLink('demos', 'Demos')),
        h(
          'li',
          null,
          h(
            'a',
            {href: 'https://github.com/tinyplex/tinyjoin'},
            'GitHub',
          ),
        ),
      ),
    ),
    h('button', {
      id: 'dark',
      className: 'auto',
      type: 'button',
      'aria-label': 'Color theme: automatic; activate for dark',
    }),
  );
};

const Home = ({page}) =>
  h(
    'article',
    {id: 'home', tabIndex: -1},
    h(
      'em',
      null,
      h('img', {
        src: '/favicon.svg',
        alt: 'Large TinyJoin logo',
        width: '100%',
        height: '100%',
      }),
    ),
    page.summary ? h(Markdown, {markdown: page.summary, html: true}) : null,
    page.body ? h(Markdown, {markdown: page.body, html: true}) : null,
  );

const MarkdownPage = () => {
  const {summary, body} = usePageNode();
  return h(Markdown, {
    markdown: [summary, body].filter(Boolean).join('\n\n'),
    html: true,
    skipCode: true,
  });
};

const Footer = () =>
  h(
    'footer',
    null,
    h(
      'nav',
      null,
      h(
        'a',
        {
          id: 'gh',
          href: 'https://github.com/tinyplex/tinyjoin',
          target: '_blank',
          rel: 'noreferrer',
        },
        'GitHub',
      ),
    ),
    h(
      'nav',
      null,
      h('a', {href: '/'}, 'TinyJoin'),
      ' · MIT licensed',
    ),
  );

const Page = () => {
  const page = usePageNode();
  const root = useRootNode();
  const baseUrl = useBaseUrl();

  if (page.summary?.startsWith('->')) {
    return h('meta', {
      httpEquiv: 'refresh',
      content: `0;url=${page.summary.substring(2).trim()}`,
    });
  }

  const title = page === root ? 'TinyJoin' : `${page.name} | TinyJoin`;
  const description =
    'A tiny, worker-first relational database for browser apps.';
  const canonical = `${baseUrl}${page.url}`;

  return h(
    'html',
    {lang: 'en'},
    h(
      'head',
      null,
      h('meta', {charSet: 'utf-8'}),
      h('meta', {
        name: 'viewport',
        content: 'width=device-width, initial-scale=1',
      }),
      h('title', null, title),
      h('meta', {name: 'description', content: description}),
      h('meta', {property: 'og:type', content: 'website'}),
      h('meta', {property: 'og:title', content: title}),
      h('meta', {property: 'og:description', content: description}),
      h('meta', {property: 'og:url', content: canonical}),
      h('link', {rel: 'canonical', href: canonical}),
      h('link', {rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg'}),
      h('link', {rel: 'stylesheet', href: '/css/index.css'}),
      h('script', {src: '/js/site.js'}),
    ),
    h(
      'body',
      null,
      h(
        'a',
        {className: 'skip', href: page === root ? '#home' : '#content'},
        'Skip to content',
      ),
      h(Header, {page}),
      h(
        'main',
        null,
        page === root
          ? h(Home, {page})
          : [
              h(
                'nav',
                {key: 'navigation', 'aria-label': 'Documentation'},
                h('ul', null, h(NodeNavigation, {node: root})),
              ),
              h(
                'article',
                {key: 'article', id: 'content', tabIndex: -1},
                h(
                  'nav',
                  {className: 'breadcrumbs', 'aria-label': 'Breadcrumbs'},
                  h('ul', null, h(NodeBreadcrumbs, {node: root})),
                ),
                h(PageContent, {page}),
              ),
              h('aside', {key: 'aside', 'aria-hidden': 'true'}),
            ],
      ),
      h(Footer),
    ),
  );
};

export const build = async (
  outDir = 'docs',
  typesDir = 'dist/@types',
  publicMarkdownDir = repositoryRoot,
  packageDir = resolve(typesDir, '..'),
) => {
  const docs = createDocs('https://tinyjoin.org', outDir)
    .addJsFile('site/js/site.ts')
    .addLessFile('site/less/index.less')
    .addDir('site/extras')
    .addReflectionTransform(hideInheritedErrorMembers)
    .addApiFile(resolve(typesDir, 'index.d.ts'))
    .addApiFile(resolve(typesDir, 'worker/index.d.ts'))
    .addRootMarkdownFile('site/home/index.md')
    .addMarkdownDir('site/guides')
    .addMarkdownDir('site/demos')
    .addStringFile(
      readFileSync('site/guides/6_agents.md', 'utf8'),
      'llms-full.txt',
    );

  await docs.generateNodes({
    group: getSorter(groups),
    category: getSorter(categories),
    reflection: getSorter(reflections),
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
};

const getOutputPath = (outDir, nodeUrl, destination) =>
  relative(resolve(outDir, `.${nodeUrl}`), destination);

const waitForFile = async (file) => {
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

if (process.argv[1] === new URL(import.meta.url).pathname) {
  await build(process.argv[2] ?? 'docs', process.argv[3] ?? 'dist/@types');
}
