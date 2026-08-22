import {readFileSync} from 'node:fs';
import {resolve} from 'node:path';

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
  'tinygres',
  'worker',
  'create',
  'Client',
  'ClientError',
  '*',
];

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
    {className: 'page-content', id: page.url, 'data-id': page.id},
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

  const title = page === root ? 'TinyGres' : `${page.name} | TinyGres`;
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
      h('link', {rel: 'stylesheet', href: '/style.css'}),
    ),
    h(
      'body',
      {className: page === root ? 'home' : undefined},
      h(
        'header',
        null,
        h('a', {className: 'wordmark', href: '/'}, 'TinyGres'),
        h(
          'nav',
          {'aria-label': 'Primary'},
          h('a', {href: '/guides/'}, 'Guides'),
          h('a', {href: '/api/'}, 'API'),
          h('a', {href: '/demos/'}, 'Demos'),
          h(
            'a',
            {href: 'https://github.com/tinyplex/tinygres'},
            'GitHub',
          ),
        ),
      ),
      h(
        'div',
        {className: 'layout'},
        h(
          'aside',
          null,
          h(
            'nav',
            {'aria-label': 'Documentation'},
            h('ul', null, h(NodeNavigation, {node: root})),
          ),
        ),
        h(
          'main',
          null,
          page === root
            ? null
            : h(
                'nav',
                {className: 'breadcrumbs', 'aria-label': 'Breadcrumbs'},
                h('ul', null, h(NodeBreadcrumbs, {node: root})),
              ),
          h(PageContent, {page}),
        ),
      ),
      h(
        'footer',
        null,
        h('span', null, 'TinyGres is MIT licensed.'),
        h(
          'a',
          {href: 'https://github.com/tinyplex'},
          'A TinyPlex project',
        ),
      ),
    ),
  );
};

export const build = async (outDir = 'docs', typesDir = 'dist/@types') => {
  const docs = createDocs('https://tinygres.org', outDir)
    .addFile('site/style.css', '')
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
    .publish();
};

if (process.argv[1] === new URL(import.meta.url).pathname) {
  await build(process.argv[2] ?? 'docs', process.argv[3] ?? 'dist/@types');
}
