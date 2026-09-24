import {enhanceMain} from './enhance.ts';

const SITE = 'TinyJoin';

const isInternal = (link: HTMLAnchorElement): boolean =>
  link.origin === location.origin &&
  !link.hasAttribute('download') &&
  (link.target === '' || link.target.toLowerCase() === '_self') &&
  link.pathname !== '/' &&
  !link.pathname.includes('.');

const setTitle = (main: HTMLElement) => {
  const heading = main.querySelector('article h1');
  document.title =
    heading?.textContent == null ? SITE : `${heading.textContent} | ${SITE}`;
};

export const navLoad = () => {
  const main = document.querySelector('main');
  if (main == null || location.pathname === '/') {
    return;
  }

  let currentUrl = new URL(location.href);
  let requestId = 0;
  let controller: AbortController | undefined;

  const cancelNavigation = () => {
    requestId++;
    controller?.abort();
    controller = undefined;
  };

  const isCurrentDocument = (url: URL) =>
    url.pathname === currentUrl.pathname && url.search === currentUrl.search;

  const go = async (url: URL, push: boolean) => {
    cancelNavigation();
    const id = requestId;
    controller = new AbortController();
    let html: string;
    try {
      const response = await fetch(`${url.pathname}main.html${url.search}`, {
        signal: controller.signal,
      });
      if (!response.ok) {
        throw new Error(String(response.status));
      }
      html = await response.text();
    } catch {
      if (id === requestId) {
        location.href = url.href;
      }
      return;
    }
    if (id !== requestId) {
      return;
    }
    controller = undefined;
    main.innerHTML = html;
    currentUrl = url;
    setTitle(main);
    enhanceMain();
    if (push) {
      history.pushState({}, '', url.href);
    }
    let anchor = url.hash.slice(1);
    try {
      anchor = decodeURIComponent(anchor);
    } catch {
      // A malformed escape can still be a literal element identifier.
    }
    const target = anchor === '' ? null : document.getElementById(anchor);
    if (target != null) {
      target.scrollIntoView();
    } else {
      main.querySelector('article')?.scrollTo(0, 0);
    }
    main.querySelector<HTMLElement>('#content')?.focus({preventScroll: true});
  };

  document.body.addEventListener('click', (event) => {
    if (
      event.defaultPrevented ||
      event.button !== 0 ||
      event.metaKey ||
      event.ctrlKey ||
      event.shiftKey ||
      event.altKey
    ) {
      return;
    }
    const link = event.target instanceof Element ? event.target.closest('a') : null;
    if (link == null || !isInternal(link)) {
      return;
    }
    const url = new URL(link.href);
    if (isCurrentDocument(url)) {
      cancelNavigation();
      return;
    }
    event.preventDefault();
    void go(url, true);
  });

  addEventListener('popstate', () => {
    cancelNavigation();
    if (location.pathname === '/') {
      location.reload();
    } else if (!isCurrentDocument(new URL(location.href))) {
      void go(new URL(location.href), false);
    }
  });
};
