import {expect, test} from '@playwright/test';

for (const width of [390, 768]) {
  test(`navigation and search remain usable at ${width}px`, async ({page}) => {
    await page.setViewportSize({width, height: 844});
    await page.goto('/guides/caveats/');
    const primary = page.getByRole('navigation', {name: 'Primary', exact: true});
    for (const name of ['Guides', 'Demos', 'API', 'GitHub']) {
      await expect(primary.getByRole('link', {name, exact: true})).toBeVisible();
    }
    const search = page.getByRole('searchbox', {name: 'Search the documentation'});
    await expect(search).toBeVisible();
    await search.fill('storage');
    const result = page.locator('#search li').filter({hasText: 'Storage and lifecycle'}).first();
    await expect(result).toBeVisible();
    await result.click();
    await expect(page).toHaveURL(/\/guides\/storage-and-lifecycle\//);
    await expect(page.locator('article h1')).toHaveText('Storage and lifecycle');
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
  });
}
