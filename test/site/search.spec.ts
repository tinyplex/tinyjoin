import {expect, test} from '@playwright/test';

const pages = [
  {u: '/guides/storage-and-lifecycle/', n: 'Storage', s: 'Storage lifecycle'},
  {u: '/guides/caveats/', n: 'Storage', s: 'Storage caveats'},
  {u: '/guides/offline/', n: 'Offline', s: 'Storage offline'},
];

test.beforeEach(async ({page}) => {
  await page.route('**/pages.json', (route) => route.fulfill({json: pages}));
  await page.goto('/guides/');
});

test('search announces results and keeps keyboard selection related to the focused input', async ({page}) => {
  const input = page.getByRole('combobox', {name: 'Search the documentation'});
  await expect(input).toHaveAttribute('aria-expanded', 'false');
  await input.fill('storage');
  const list = page.getByRole('listbox', {name: 'Documentation results'});
  const options = list.getByRole('option');
  await expect(options).toHaveCount(3);
  await expect(input).toHaveAttribute('aria-controls', await list.getAttribute('id') as string);
  await expect(input).toHaveAttribute('aria-expanded', 'true');
  await expect(page.getByRole('status')).toContainText('3 results available');
  await expect(options.nth(0)).toHaveAttribute('aria-selected', 'true');
  await expect(input).toHaveAttribute('aria-activedescendant', await options.nth(0).getAttribute('id') as string);

  await input.press('ArrowDown');
  await expect(input).toBeFocused();
  await expect(options.nth(0)).toHaveAttribute('aria-selected', 'false');
  await expect(options.nth(1)).toHaveAttribute('aria-selected', 'true');
  await expect(input).toHaveAttribute('aria-activedescendant', await options.nth(1).getAttribute('id') as string);
  await input.press('ArrowUp');
  await expect(options.nth(0)).toHaveAttribute('aria-selected', 'true');
  await input.press('ArrowUp');
  await expect(options.nth(2)).toHaveAttribute('aria-selected', 'true');
  await input.press('ArrowDown');
  await input.press('ArrowDown');
  // Duplicate page names must still navigate to the selected URL.
  await input.press('Enter');
  await expect(page).toHaveURL('/guides/caveats/');
});

test('empty searches, dismissal, and refocusing never expose a stale active result', async ({page}) => {
  const input = page.getByRole('combobox', {name: 'Search the documentation'});
  await input.fill('storage');
  await expect(page.getByRole('option')).toHaveCount(3);
  const firstId = await page.getByRole('option').first().getAttribute('id');
  await input.fill('offline');
  await expect(page.getByRole('option')).toHaveCount(1);
  const offlineId = await page.getByRole('option').getAttribute('id');
  expect(offlineId).not.toBe(firstId);
  await input.fill('storage');
  await expect(page.getByRole('option').nth(2)).toHaveAttribute('id', offlineId as string);
  await input.fill('no-matching-document');
  await expect(page.getByRole('status')).toHaveText('No results found.');
  await expect(page.getByRole('option')).toHaveCount(0);
  await input.press('ArrowDown');
  await input.press('Enter');
  await expect(input).not.toHaveAttribute('aria-activedescendant');
  await expect(page).toHaveURL('/guides/');
  await input.fill('storage');
  await input.press('Escape');
  await expect(input).not.toBeFocused();
  await expect(input).toHaveAttribute('aria-expanded', 'false');
  await expect(input).not.toHaveAttribute('aria-activedescendant');
  await expect(page.getByRole('listbox')).toBeHidden();
  await input.focus();
  await expect(input).toHaveAttribute('aria-expanded', 'true');
  await expect(input).toHaveAttribute('aria-activedescendant', firstId as string);
  await input.press('Tab');
  await expect(input).toHaveAttribute('aria-expanded', 'false');
  await input.focus();
  await input.fill('');
  await expect(input).toHaveAttribute('aria-expanded', 'false');
  await expect(page.getByRole('status')).toHaveText('');
});

test('pointer activation opens the selected result', async ({page}) => {
  const input = page.getByRole('combobox', {name: 'Search the documentation'});
  await input.fill('storage');
  const option = page.getByRole('option').nth(1);
  await option.hover();
  await expect(option).toHaveAttribute('aria-selected', 'true');
  await option.click();
  await expect(page).toHaveURL('/guides/caveats/');
});

test('a query entered before the index arrives is populated when loading finishes', async ({page}) => {
  let release = () => {};
  const ready = new Promise<void>((resolve) => { release = resolve; });
  await page.route('**/pages.json', async (route) => {
    await ready;
    await route.fulfill({json: pages});
  });
  await page.goto('/guides/');
  const input = page.getByRole('combobox', {name: 'Search the documentation'});
  await input.fill('offline');
  release();
  await expect(page.getByRole('option')).toHaveCount(1);
  await expect(input).toHaveAttribute('aria-activedescendant', 'search-result-2');
});
