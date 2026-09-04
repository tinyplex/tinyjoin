const SITE = 'TinyJoin';

const isInternal = (link: HTMLAnchorElement): boolean =>
  link.origin === location.origin &&
  !link.hasAttribute('download') &&
  link.target !== '_blank' &&
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

  const go = async (pathname: string, push: boolean) => {
    let html: string;
    try {
      const response = await fetch(`${pathname}main.html`);
      if (!response.ok) {
        throw new Error(String(response.status));
      }
      html = await response.text();
    } catch {
      location.href = pathname;
      return;
    }
    main.innerHTML = html;
    setTitle(main);
    if (push) {
      history.pushState({}, '', pathname);
    }
    scrollTo(0, 0);
    main.querySelector<HTMLElement>('#content')?.focus();
  };

  document.body.addEventListener('click', (event) => {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) {
      return;
    }
    const link = (event.target as Element | null)?.closest('a');
    if (link == null || !isInternal(link)) {
      return;
    }
    event.preventDefault();
    void go(link.pathname, true);
  });

  addEventListener('popstate', () => {
    if (location.pathname === '/') {
      location.reload();
    } else {
      void go(location.pathname, false);
    }
  });
};
