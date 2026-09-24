// Terminal examples may mark their commands with a '> ' prompt; copy just
// those.
const getCopyText = (code: HTMLElement): string => {
  const lines = code.innerText.trimEnd().split('\n');
  const commands = lines.filter((line) => line.startsWith('> '));
  return commands.length > 0
    ? commands.map((line) => line.substring(2)).join('\n')
    : lines.join('\n');
};

const addCopyButtons = () =>
  document.querySelectorAll('main pre').forEach((pre) => {
    const code = pre.querySelector<HTMLElement>(':scope > code');
    if (code == null || pre.querySelector(':scope > button') != null) {
      return;
    }
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'copy';
    button.textContent = 'Copy';
    button.addEventListener('click', () =>
      navigator.clipboard.writeText(getCopyText(code)).then(() => {
        button.textContent = 'Copied';
        setTimeout(() => (button.textContent = 'Copy'), 1500);
      }),
    );
    pre.append(button);
  });

let tocHeadings: HTMLElement[] = [];

const getArticle = () =>
  document.querySelector<HTMLElement>('body > main > article');

const updateToc = () => {
  const aside = document.querySelector('body > main > aside');
  const article = getArticle();
  if (aside == null || article == null) {
    return;
  }
  aside.replaceChildren();
  tocHeadings = Array.from(
    article.querySelectorAll<HTMLElement>('h2[id], h3[id]'),
  );
  if (tocHeadings.length < 2) {
    return;
  }
  const title = document.createElement('p');
  title.textContent = 'On this page';
  const list = document.createElement('ul');
  tocHeadings.forEach((heading) => {
    const item = document.createElement('li');
    item.className = heading.tagName.toLowerCase();
    const link = document.createElement('a');
    link.href = `#${heading.id}`;
    link.title = link.textContent = heading.innerText;
    item.append(link);
    list.append(item);
  });
  aside.append(title, list);
  highlightToc();
};

// The current heading is the last to have scrolled near the top of the
// article, or the very last one if the article cannot scroll any further.
const highlightToc = () => {
  const article = getArticle();
  const items = document.querySelectorAll('body > main > aside li');
  if (article == null || items.length === 0) {
    return;
  }
  const top = article.getBoundingClientRect().top + 96;
  let current = 0;
  if (article.scrollTop + article.clientHeight >= article.scrollHeight - 1) {
    current = tocHeadings.length - 1;
  } else {
    tocHeadings.forEach((heading, index) => {
      if (heading.getBoundingClientRect().top <= top) {
        current = index;
      }
    });
  }
  items.forEach((item, index) =>
    item.classList.toggle('current', index === current),
  );
};

// Show the full name of a sidebar link that is truncated with an ellipsis.
const addNavTitles = () =>
  document
    .querySelectorAll<HTMLElement>('body > main > nav li > a')
    .forEach((link) => link.setAttribute('title', link.innerText));

// Run on first load and again whenever navigation replaces the main element.
export const enhanceMain = () => {
  addCopyButtons();
  addNavTitles();
  updateToc();
};

export const enhanceLoad = () => {
  enhanceMain();
  // Scroll events do not bubble, but they can be captured from the main
  // element, which outlives each article that navigation swaps in.
  document
    .querySelector('main')
    ?.addEventListener(
      'scroll',
      () => requestAnimationFrame(highlightToc),
      true,
    );
};
