import {useBaseUrl, usePageNode, useRootNode} from 'tinydocs';
import {Footer} from './Footer.tsx';
import {Header} from './Header.tsx';
import {Home} from './Home.tsx';
import {MainInner} from './MainInner.tsx';

const GTM_ID = 'G-40B96SPQX2';

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
        <link rel="dns-prefetch" href="https://www.googletagmanager.com/" />
        <link
          href={`https://www.googletagmanager.com/gtag/js?id=${GTM_ID}`}
          rel="preload"
          as="script"
        />
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
      <body className={isHome ? 'home' : undefined}>
        <a className="skip" href={isHome ? '#home' : '#content'}>
          Skip to content
        </a>
        <Header />
        <main>
          {isHome ? <Home node={node} /> : <MainInner />}
        </main>
        <Footer />
      </body>
      <script
        async
        src={`https://www.googletagmanager.com/gtag/js?id=${GTM_ID}`}
      />
      <script
        dangerouslySetInnerHTML={{
          __html:
            `window.dataLayer=window.dataLayer||[];` +
            `function g(){dataLayer.push(arguments);}` +
            `g('js',new Date());` +
            `g('config','${GTM_ID}');`,
        }}
      />
    </html>
  );
};
