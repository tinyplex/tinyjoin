import {useBaseUrl, usePageNode, useRootNode} from 'tinydocs';
import {getBenchmarks} from '../benchmarks.ts';
import {Footer} from './Footer.tsx';
import {Header} from './Header.tsx';
import {Home} from './Home.tsx';
import {MainInner} from './MainInner.tsx';

const GTM_ID = 'G-40B96SPQX2';

// The site's description is the homepage's headline, whose first words are
// only as strong as the published benchmarks support. package.json carries the
// same sentence, which the build holds it to.
export const DESCRIPTION = getBenchmarks().renderText(
  '{{benchmarks.epithet}} relational database for your web app.',
);

// Every page shares one of the 1600x900 images that npm run build:card
// captures. The benchmarks guide has one of its own, the benchmark card, from
// its results. Every other page shares the site card, from the homepage's hero.
const BENCHMARKS_URL = '/guides/benchmarks/';
const BENCHMARKS_IMAGE_ALT =
  "A summary of TinyJoin's benchmark results against SQLite and PGlite";
const SITE_IMAGE_ALT = `TinyJoin: ${DESCRIPTION}`;

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
  const [image, imageAlt] =
    node.url === BENCHMARKS_URL
      ? ['benchmark-card', BENCHMARKS_IMAGE_ALT]
      : ['site-card', SITE_IMAGE_ALT];

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
        <meta property="og:image" content={`${baseUrl}/${image}.png`} />
        <meta property="og:image:width" content="1600" />
        <meta property="og:image:height" content="900" />
        <meta property="og:image:alt" content={imageAlt} />
        <meta name="twitter:card" content="summary_large_image" />
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
