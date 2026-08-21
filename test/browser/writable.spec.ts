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
  expect(report.bootstrapCommands).toEqual([
    'CREATE TABLE',
    'CREATE INDEX',
    'ALTER TABLE',
    'INSERT',
    'SELECT',
  ]);
  expect(report.bootstrapRevisions).toEqual(Array(5).fill(1));
  expect(report.stagedRows).toEqual([
    {id: 1, title: 'Ship writable SQL', done: true},
    {id: 2, title: 'Persist it', done: false},
    {id: 3, title: 'Rollback safely', done: false},
  ]);
  expect(report.rollbackCode).toBe('CONSTRAINT_VIOLATION');
  expect(report.scriptFailureCode).toBe('CONSTRAINT_VIOLATION');
  expect(report.scriptTableCode).toBe('TABLE_NOT_FOUND');
  expect(report.orderedRows).toEqual([
    {id: 3, title: 'Rollback safely', done: false},
    {id: 2, title: 'Persist it', done: false},
  ]);
  expect(report.preparedRows).toEqual([
    {id: 1, title: 'Ship writable SQL', done: false},
    {id: 2, title: 'Persist it', done: false},
  ]);
  expect(report.preparedClosed).toBe(true);
  expect(report.committedRevision).toBe(2);
  expect(report.reopenedRevision).toBe(2);
  expect(report.reopenedPriorities).toEqual([
    {id: 1, priority: 0},
    {id: 2, priority: 0},
    {id: 3, priority: 0},
  ]);
  expect(report.reopenedRows).toEqual(report.stagedRows);
  expect(report.invalidations).toEqual([
    {revision: 1, tables: ['tasks']},
    {revision: 2, tables: ['tasks']},
  ]);
});
