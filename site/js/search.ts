type Page = {u: string; n: string; s: string};

const MAX_RESULTS = 10;

const tokenize = (text: string): string[] =>
  text.toLowerCase().split(/[^a-z0-9]+/).filter(Boolean);

const weigh = (page: Page, word: string): number => {
  const name = page.n.toLowerCase();
  const summary = page.s.toLowerCase();
  const url = page.u.toLowerCase();
  if (name === word) {
    return 100;
  }
  if (name.startsWith(word)) {
    return 50;
  }
  if (tokenize(name).some((token) => token.startsWith(word))) {
    return 25;
  }
  if (url.includes(word)) {
    return 10;
  }
  if (tokenize(summary).some((token) => token.startsWith(word))) {
    return 5;
  }
  return 0;
};

const highlight = (element: HTMLElement, text: string, word: string) => {
  const at = word ? text.toLowerCase().indexOf(word) : -1;
  if (at < 0) {
    element.textContent = text;
    return;
  }
  element.textContent = '';
  element.append(
    text.slice(0, at),
    Object.assign(document.createElement('em'), {
      textContent: text.slice(at, at + word.length),
    }),
    text.slice(at + word.length),
  );
};

export const searchLoad = () => {
  const nav = document.querySelector('body > header > nav');
  if (nav == null) {
    return;
  }

  const search = document.createElement('div');
  search.id = 'search';
  const input = document.createElement('input');
  input.type = 'search';
  input.autocomplete = 'off';
  input.setAttribute('aria-label', 'Search the documentation');
  input.placeholder =
    (navigator.platform.startsWith('Mac') ? '⌘' : 'ctrl-') + 'K Search';
  const results = document.createElement('ol');
  search.append(input, results);

  let pages: Page[] = [];

  const show = (visible: boolean) =>
    results.classList.toggle('show', visible && input.value !== '');

  const hovered = () => results.querySelector('li.hover');

  const moveHover = (to: Element | null | undefined) => {
    if (to instanceof HTMLElement) {
      hovered()?.classList.remove('hover');
      to.classList.add('hover');
      to.scrollIntoView({block: 'nearest'});
    }
  };

  const populate = () => {
    const words = tokenize(input.value);
    const ranked = pages
      .map((page) => ({
        page,
        weight: words.reduce((total, word) => total + weigh(page, word), 0),
      }))
      .filter(({weight}) => weight > 0)
      .sort((one, two) => two.weight - one.weight)
      .slice(0, MAX_RESULTS);

    if (ranked.length === 0) {
      const empty = document.createElement('li');
      empty.textContent = 'No results found';
      results.replaceChildren(empty);
      return;
    }

    results.replaceChildren(
      ...ranked.map(({page}, index) => {
        const result = document.createElement('li');
        const name = document.createElement('b');
        const summary = document.createElement('span');
        highlight(name, page.n, words[0] ?? '');
        highlight(summary, page.s, words[0] ?? '');
        result.append(name, summary);
        result.title = page.s;
        result.addEventListener('mousedown', () => {
          location.href = page.u;
        });
        if (index === 0) {
          result.classList.add('hover');
        }
        return result;
      }),
    );
  };

  input.addEventListener('focus', () => show(true));
  input.addEventListener('blur', () => show(false));
  input.addEventListener('input', () => {
    show(true);
    populate();
  });

  addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === 'k') {
      event.preventDefault();
      input.focus();
      input.select();
      return;
    }
    if (document.activeElement !== input) {
      return;
    }
    const current = hovered();
    if (event.key === 'Escape') {
      input.blur();
    } else if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveHover(current?.nextElementSibling ?? results.firstElementChild);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveHover(current?.previousElementSibling ?? results.lastElementChild);
    } else if (event.key === 'Enter') {
      const to = current?.querySelector('b')?.textContent;
      const page = pages.find((candidate) => candidate.n === to);
      if (page != null) {
        location.href = page.u;
      }
    }
  });

  nav.prepend(search);

  fetch('/pages.json')
    .then((response) => response.json())
    .then((json: Page[]) => {
      pages = json;
    })
    .catch(() => {
      search.remove();
    });
};
