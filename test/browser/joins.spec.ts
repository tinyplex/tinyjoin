import {expect, test} from '@playwright/test';

test('joins typed tables, including many-to-many, through the real Worker/WASM engine', async ({page}) => {
  await page.goto('/');
  await expect(page.getByTestId('state')).toHaveText('Ready');

  const report = await page.evaluate(() =>
    window.__tinyjoinTest!.joinDatabaseProbe(),
  );

  expect(report.innerRows).toEqual([
    {
      author_id: 1,
      author_name: 'Ada',
      article_id: 10,
      article_title: 'Worker databases',
    },
    {
      author_id: 2,
      author_name: 'Linus',
      article_id: 12,
      article_title: 'Local queries',
    },
  ]);
  expect(report.leftRows).toEqual([
    {author_id: 1, author_name: 'Ada', article_id: 10},
    {author_id: 1, author_name: 'Ada', article_id: 11},
    {author_id: 2, author_name: 'Linus', article_id: 12},
    {author_id: 3, author_name: 'Grace', article_id: null},
  ]);
  expect(report.manyToManyRows).toEqual([
    {
      post_id: 1,
      post_title: 'Worker databases',
      tag_id: 10,
      tag_name: 'wasm',
    },
    {
      post_id: 1,
      post_title: 'Worker databases',
      tag_id: 11,
      tag_name: 'offline',
    },
    {
      post_id: 2,
      post_title: 'Local queries',
      tag_id: 11,
      tag_name: 'offline',
    },
  ]);
});
