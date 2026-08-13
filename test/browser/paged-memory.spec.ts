import {expect, test} from '@playwright/test';

test('opens, writes, commits, and reopens the page-native WASM engine in a Worker', async ({
  page,
}) => {
  await page.goto('/paged-memory.html');

  await expect(page.getByTestId('state')).toHaveText('Ready');
  await expect(page.getByTestId('error')).toBeHidden();
  const report = JSON.parse(
    (await page.getByTestId('report').textContent()) ?? 'null',
  ) as {
    closes: number;
    committedRevision: number;
    defineRevision: number;
    pageCount: number;
    reopenedRevision: number;
    rows: Array<{id: number; title: string}>;
  };

  expect(report.defineRevision).toBe(0);
  expect(report.committedRevision).toBe(2);
  expect(report.reopenedRevision).toBe(2);
  expect(report.pageCount).toBeGreaterThanOrEqual(8);
  expect(report.closes).toBe(2);
  expect(report.rows).toEqual([
    {id: 1, title: 'default title'},
    {id: 3, title: 'committed'},
  ]);
});
