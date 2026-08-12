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

  expect(report.aggregateRows).toEqual([
    {done: false, task_count: 2},
    {done: true, task_count: 1},
  ]);
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
  expect(report.orderedRows).toEqual([
    {id: 3, title: 'Rollback safely', done: false},
    {id: 2, title: 'Persist it', done: false},
  ]);
  expect(report.committedRevision).toBe(5);
  expect(report.reopenedRevision).toBe(5);
  expect(report.reopenedPriorities).toEqual([
    {id: 1, priority: 0},
    {id: 2, priority: 0},
    {id: 3, priority: 0},
  ]);
  expect(report.reopenedRows).toEqual(report.stagedRows);
  expect(report.invalidations).toEqual([
    {revision: 1, tables: ['tasks']},
    {revision: 2, tables: ['tasks']},
    {revision: 3, tables: ['tasks']},
    {revision: 4, tables: ['tasks']},
    {revision: 5, tables: ['tasks']},
  ]);
});
