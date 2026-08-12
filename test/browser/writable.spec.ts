import {expect, test} from '@playwright/test';

test('writes, transacts, rolls back, and reopens through Worker/WASM/OPFS', async ({
  page,
}) => {
  await page.goto('/');
  await expect(page.getByTestId('state')).toHaveText('Ready');
  const databaseName = `writable-${Date.now()}-${Math.floor(Math.random() * 1_000_000)}`;

  const report = await page.evaluate((name) =>
    window.__tinygresTest!.writableDatabaseProbe(name), databaseName,
  );

  expect(report.insertRows).toEqual([
    {id: 1, title: 'Ship writable SQL', done: false},
    {id: 2, title: 'Persist it', done: false},
  ]);
  expect(report.stagedRows).toEqual([
    {id: 1, title: 'Ship writable SQL', done: true},
    {id: 2, title: 'Persist it', done: false},
    {id: 3, title: 'Rollback safely', done: false},
  ]);
  expect(report.rollbackCode).toBe('CONSTRAINT_VIOLATION');
  expect(report.committedRevision).toBe(3);
  expect(report.reopenedRevision).toBe(3);
  expect(report.reopenedRows).toEqual(report.stagedRows);
  expect(report.invalidations).toEqual([
    {revision: 1, tables: ['tasks']},
    {revision: 2, tables: ['tasks']},
    {revision: 3, tables: ['tasks']},
  ]);
});
