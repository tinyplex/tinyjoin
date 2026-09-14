import {expect, test, type Page} from '@playwright/test';

const caveats = '/guides/caveats/';
const fitAnchor = '#if-tinyjoin-is-not-the-right-fit';
const limits = '/guides/sql-compatibility/';
const limitsAnchor = '#hard-limits';
const delayedPage = '/guides/getting-started/';

const expectAnchor = async (page: Page, anchor: string) => {
  await expect(page.locator(anchor)).toBeInViewport();
  expect(await page.locator('article').evaluate((article) => article.scrollTop)).toBeGreaterThan(0);
};

const deferred = () => {
  let resolve = () => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return {promise, resolve};
};

test('same-document section and skip links use native anchors without fetching fragments', async ({page}) => {
  const fragments: string[] = [];
  page.on('request', (request) => {
    if (new URL(request.url()).pathname.endsWith('/main.html')) {
      fragments.push(request.url());
    }
  });
  await page.goto(caveats);
  await page.locator(`article a[href="${fitAnchor}"]`).click();
  await expect(page).toHaveURL(`${caveats}${fitAnchor}`);
  await expectAnchor(page, fitAnchor);

  const skip = page.getByRole('link', {name: 'Skip to content', exact: true});
  await skip.focus();
  await skip.press('Enter');
  await expect(page).toHaveURL(`${caveats}#content`);
  await expect(page.locator('#content')).toBeFocused();
  expect(fragments).toEqual([]);
});

test('cross-page anchors preserve query strings and restore their targets through back and forward', async ({page}) => {
  const destination = `${limits}?from=caveats&view=limits${limitsAnchor}`;
  await page.goto(`${caveats}${fitAnchor}`);
  const link = page.locator('article').getByRole('link', {name: 'hard limits', exact: true});
  await link.evaluate((element, href) => element.setAttribute('href', href), destination);
  await link.click();

  await expect(page).toHaveURL(destination);
  await expect(page).toHaveTitle('SQL compatibility | TinyJoin');
  await expectAnchor(page, limitsAnchor);
  await expect(page.locator('#content')).toBeFocused();

  await page.goBack();
  await expect(page).toHaveURL(`${caveats}${fitAnchor}`);
  await expect(page).toHaveTitle('Caveats | TinyJoin');
  await expectAnchor(page, fitAnchor);

  await page.goForward();
  await expect(page).toHaveURL(destination);
  await expect(page).toHaveTitle('SQL compatibility | TinyJoin');
  await expectAnchor(page, limitsAnchor);
});

for (const status of [200, 503]) {
  for (const winner of ['page', 'anchor'] as const) {
    test(`a newer ${winner} wins over a delayed ${status === 200 ? 'success' : 'failure'}`, async ({page, request}) => {
      const delayedFragment = `${delayedPage}main.html`;
      const response = await request.get(delayedFragment);
      const html = await response.text();
      const started = deferred();
      const released = deferred();
      const errors: string[] = [];
      page.on('pageerror', (error) => errors.push(error.message));

      // Keep this request alive despite cancellation so the generation guard
      // is exercised against both a late response and a late fallback error.
      await page.addInitScript((pathname) => {
        const fetch = window.fetch.bind(window);
        window.fetch = (input, init) => {
          const url = input instanceof Request ? input.url : String(input);
          if (new URL(url, location.href).pathname === pathname) {
            const options = {...init};
            delete options.signal;
            return fetch(input, options);
          }
          return fetch(input, init);
        };
      }, delayedFragment);
      await page.route(`**${delayedFragment}`, async (route) => {
        started.resolve();
        await released.promise;
        await route.fulfill({status, contentType: 'text/html', body: html});
      });

      await page.goto(caveats);
      await page.locator(`main nav a[href="${delayedPage}"]`).click();
      await started.promise;
      const target = winner === 'page' ? limitsAnchor : fitAnchor;
      const destination = winner === 'page' ? `${limits}${limitsAnchor}` : `${caveats}${fitAnchor}`;
      const href = winner === 'page' ? `${limits}${limitsAnchor}` : fitAnchor;
      await page.locator(`article a[href="${href}"]`).click();
      await expect(page).toHaveURL(destination);
      await expectAnchor(page, target);

      const lateResponse = page.waitForResponse((result) => new URL(result.url()).pathname === delayedFragment);
      released.resolve();
      await (await lateResponse).finished();
      await page.waitForLoadState('networkidle');
      await expect(page).toHaveURL(destination);
      await expect(page.locator('article h1')).toHaveText(winner === 'page' ? 'SQL compatibility' : 'Caveats');
      await expectAnchor(page, target);
      expect(errors).toEqual([]);
    });
  }
}

test('an active fragment failure falls back to the complete URL and anchor', async ({page}) => {
  const destination = `${limits}?from=fallback${limitsAnchor}`;
  await page.route(`**${limits}main.html*`, (route) => route.fulfill({status: 503, body: 'Unavailable'}));
  await page.goto(caveats);
  const link = page.locator('article').getByRole('link', {name: 'hard limits', exact: true});
  await link.evaluate((element, href) => element.setAttribute('href', href), destination);
  const navigation = page.waitForRequest((request) =>
    request.isNavigationRequest() && new URL(request.url()).pathname === limits,
  );
  await link.click();
  await navigation;
  await expect(page).toHaveURL(destination);
  await expect(page).toHaveTitle('SQL compatibility | TinyJoin');
  await expectAnchor(page, limitsAnchor);
});

test('modified, download, and non-self clicks retain their native behavior', async ({page}) => {
  await page.goto(caveats);
  const intercepted = await page.evaluate((href) => {
    const cases: {event?: MouseEventInit; target?: string; download?: boolean}[] = [
      {event: {ctrlKey: true}},
      {event: {metaKey: true}},
      {event: {shiftKey: true}},
      {event: {altKey: true}},
      {event: {button: 1}},
      {target: '_blank'},
      {target: 'named-frame'},
      {download: true},
    ];
    return cases.map(({event, target, download}) => {
      const link = document.createElement('a');
      link.href = href;
      link.target = target ?? '';
      if (download) {
        link.download = 'guide.html';
      }
      document.body.append(link);
      let prevented = false;
      window.addEventListener('click', (click) => {
        prevented = click.defaultPrevented;
        // Observe the site's handler, then suppress browser side effects in
        // this test (new windows, downloads, or navigation in another frame).
        click.preventDefault();
      }, {once: true});
      link.dispatchEvent(new MouseEvent('click', {bubbles: true, cancelable: true, ...event}));
      link.remove();
      return prevented;
    });
  }, limits);
  expect(intercepted).toEqual(Array<boolean>(8).fill(false));
  await expect(page).toHaveURL(caveats);
});
