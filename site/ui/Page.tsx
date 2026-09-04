import {
  NodeBreadcrumbs,
  NodeNavigation,
  NodeSection,
  useBaseUrl,
  usePageNode,
  useRootNode,
} from 'tinydocs';
import {Footer} from './Footer.tsx';
import {Header} from './Header.tsx';
import {Home} from './Home.tsx';

const DESCRIPTION =
  'A tiny, worker-first relational database for browser apps.';

export const Page = () => {
  const node = usePageNode();
  const root = useRootNode();
  const baseUrl = useBaseUrl();
  const isHome = node === root;

  if (node.summary?.startsWith('->')) {
    return (
      <meta
        httpEquiv="refresh"
        content={`0;url=${node.summary.substring(2).trim()}`}
      />
    );
  }

  const title = isHome ? 'TinyJoin' : `${node.name} | TinyJoin`;
  const canonical = `${baseUrl}${node.url}`;

  return (
    <html lang="en">
      <head>
        <meta charSet="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        {['inter', 'inconsolata'].map((font) => (
          <link
            key={font}
            rel="preload"
            as="font"
            href={`/fonts/${font}.woff2`}
            type="font/woff2"
            crossOrigin="anonymous"
          />
        ))}
        <title>{title}</title>
        <meta name="description" content={DESCRIPTION} />
        <meta property="og:type" content="website" />
        <meta property="og:title" content={title} />
        <meta property="og:description" content={DESCRIPTION} />
        <meta property="og:url" content={canonical} />
        <link rel="canonical" href={canonical} />
        <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
        <link rel="stylesheet" href="/css/index.css" />
        {isHome ? (
          <script
            type="application/ld+json"
            dangerouslySetInnerHTML={{
              __html: JSON.stringify({
                '@context': 'https://schema.org',
                '@type': 'SoftwareSourceCode',
                name: 'TinyJoin',
                description: DESCRIPTION,
                url: baseUrl,
                codeRepository: 'https://github.com/tinyplex/tinyjoin',
                license: 'https://opensource.org/license/mit',
                programmingLanguage: ['JavaScript', 'TypeScript'],
                runtimePlatform: 'JavaScript',
                keywords: [
                  'local-first',
                  'relational database',
                  'SQL',
                  'browser database',
                  'PostgreSQL',
                ],
              }),
            }}
          />
        ) : null}
        <script src="/js/site.js" />
      </head>
      <body>
        <a className="skip" href={isHome ? '#home' : '#content'}>
          Skip to content
        </a>
        <Header />
        <main>
          {isHome ? (
            <Home node={node} />
          ) : (
            <>
              <nav aria-label="Documentation">
                <ul>
                  <NodeNavigation node={root} />
                </ul>
              </nav>
              <article id="content" tabIndex={-1}>
                <nav className="breadcrumbs" aria-label="Breadcrumbs">
                  <ul>
                    <NodeBreadcrumbs node={root} />
                  </ul>
                </nav>
                <NodeSection node={node} />
              </article>
              <aside aria-hidden="true" />
            </>
          )}
        </main>
        <Footer />
      </body>
    </html>
  );
};
