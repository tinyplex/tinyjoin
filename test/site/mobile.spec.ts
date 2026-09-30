import {expect, test} from '@playwright/test';

// Below 38rem the menu holds both search and links; below 55rem it holds only
// the links, with search staying beside it in the header.
for (const [width, searchInMenu] of [
  [390, true],
  [768, false],
] as const) {
  test(`navigation and search remain usable at ${width}px`, async ({page}) => {
    await page.setViewportSize({width, height: 844});
    await page.goto('/guides/caveats/');
    const primary = page.getByRole('navigation', {name: 'Primary', exact: true});
    const links = ['Guides', 'Demos', 'API', 'GitHub'].map((name) =>
      primary.getByRole('link', {name, exact: true}),
    );
    const menu = page.locator('#hamburger');
    const menuState = page.getByRole('checkbox', {name: 'Menu'});
    const search = page.getByRole('combobox', {name: 'Search the documentation'});

    for (const link of links) {
      await expect(link).toBeHidden();
    }
    await expect(search).toBeVisible({visible: !searchInMenu});

    await menu.click();
    for (const link of links) {
      await expect(link).toBeVisible();
    }
    await expect(search).toBeVisible();

    await primary.getByRole('link', {name: 'API', exact: true}).click();
    await expect(page).toHaveURL(/\/api\/$/);
    await expect(menuState).not.toBeChecked();
    await expect(links[0]!).toBeHidden();

    if (searchInMenu) {
      await menu.click();
    }
    await search.fill('storage');
    const result = page.locator('#search li').filter({hasText: 'Storage and lifecycle'}).first();
    await expect(result).toBeVisible();
    await result.click();
    await expect(page).toHaveURL(/\/guides\/storage-and-lifecycle\//);
    await expect(page.locator('article h1')).toHaveText('Storage and lifecycle');
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
  });
}

test('searching beside the menu closes it', async ({page}) => {
  await page.setViewportSize({width: 768, height: 844});
  await page.goto('/guides/caveats/');
  await page.locator('#hamburger').click();
  const menuState = page.getByRole('checkbox', {name: 'Menu'});
  await expect(menuState).toBeChecked();
  await page.getByRole('combobox', {name: 'Search the documentation'}).focus();
  await expect(menuState).not.toBeChecked();
});
