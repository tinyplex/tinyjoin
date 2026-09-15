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
  input.setAttribute('role', 'combobox');
  input.setAttribute('aria-autocomplete', 'list');
  input.setAttribute('aria-controls', 'search-results');
  input.setAttribute('aria-expanded', 'false');
  input.setAttribute('aria-label', 'Search the documentation');
  input.placeholder =
    (navigator.platform.startsWith('Mac') ? '⌘' : 'ctrl-') + 'K Search';
  const results = document.createElement('ol');
  results.id = 'search-results';
  results.setAttribute('role', 'listbox');
  results.setAttribute('aria-label', 'Documentation results');
  const status = document.createElement('div');
  status.className = 'search-status';
  status.setAttribute('role', 'status');
  status.setAttribute('aria-atomic', 'true');
  search.append(input, results, status);

  let pages: Page[] = [];

  const hovered = () => results.querySelector<HTMLElement>('[role="option"].hover');

  const show = (visible: boolean) => {
    const expanded = visible && input.value.trim() !== '';
    results.classList.toggle('show', expanded);
    input.setAttribute('aria-expanded', String(expanded));
    const current = hovered();
    if (expanded && current) {
      input.setAttribute('aria-activedescendant', current.id);
    } else {
      input.removeAttribute('aria-activedescendant');
    }
    if (!expanded) status.textContent = '';
  };

  const moveHover = (to: Element | null | undefined) => {
    if (to instanceof HTMLElement) {
      const previous = hovered();
      previous?.classList.remove('hover');
      previous?.setAttribute('aria-selected', 'false');
      to.classList.add('hover');
      to.setAttribute('aria-selected', 'true');
      input.setAttribute('aria-activedescendant', to.id);
      to.scrollIntoView({block: 'nearest'});
    }
  };

  const populate = () => {
    const words = tokenize(input.value);
    const ranked = pages
      .map((page, id) => ({
        page,
        id,
        weight: words.reduce((total, word) => total + weigh(page, word), 0),
      }))
      .filter(({weight}) => weight > 0)
      .sort((one, two) => two.weight - one.weight)
      .slice(0, MAX_RESULTS);

    if (ranked.length === 0) {
      const empty = document.createElement('li');
      empty.textContent = 'No results found';
      empty.setAttribute('role', 'presentation');
      empty.setAttribute('aria-hidden', 'true');
      results.replaceChildren(empty);
      input.removeAttribute('aria-activedescendant');
      status.textContent = input.value.trim() ? 'No results found.' : '';
      return;
    }

    results.replaceChildren(
      ...ranked.map(({page, id}, index) => {
        const result = document.createElement('li');
        result.id = `search-result-${id}`;
        result.dataset.url = page.u;
        result.setAttribute('role', 'option');
        result.setAttribute('aria-selected', String(index === 0));
        const name = document.createElement('b');
        const summary = document.createElement('span');
        highlight(name, page.n, words[0] ?? '');
        highlight(summary, page.s, words[0] ?? '');
        result.append(name, summary);
        result.title = page.s;
        result.addEventListener('mousedown', (event) => event.preventDefault());
        result.addEventListener('mousemove', () => moveHover(result));
        result.addEventListener('click', () => {
          location.href = page.u;
        });
        if (index === 0) {
          result.classList.add('hover');
        }
        return result;
      }),
    );
    status.textContent = `${ranked.length} ${ranked.length === 1 ? 'result' : 'results'} available. Use Up and Down arrows to select, then Enter to open.`;
  };

  input.addEventListener('focus', () => {
    populate();
    show(true);
  });
  input.addEventListener('blur', () => show(false));
  input.addEventListener('input', () => {
    populate();
    show(true);
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
      event.preventDefault();
      input.blur();
    } else if (event.key === 'ArrowDown') {
      event.preventDefault();
      show(true);
      moveHover(current?.nextElementSibling ?? results.querySelector('[role="option"]'));
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      show(true);
      moveHover(current?.previousElementSibling ?? results.querySelector('[role="option"]:last-child'));
    } else if (event.key === 'Enter') {
      const url = current?.dataset.url;
      if (url != null && input.getAttribute('aria-expanded') === 'true') {
        event.preventDefault();
        location.href = url;
      }
    }
  });

  nav.prepend(search);

  fetch('/pages.json')
    .then((response) => response.json())
    .then((json: Page[]) => {
      pages = json;
      if (document.activeElement === input) {
        populate();
        show(true);
      }
    })
    .catch(() => {
      search.remove();
    });
};
