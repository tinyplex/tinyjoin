import {chromium, expect, test} from '@playwright/test';
import {mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

test('persists complete state across dedicated Worker restarts', async ({
  page,
}) => {
  await page.goto('/');
  await expect(page.getByTestId('state')).toHaveText('Ready');
  const databaseName = `playwright-${Date.now()}-${Math.floor(Math.random() * 1_000_000)}`;

  const report = await page.evaluate(
    ({name, rows}) => window.__tinyjoinTest!.persistenceProbe(name, rows),
    {name: databaseName, rows: 10_000},
  );

  await test.info().attach('opfs-persistence.json', {
    body: JSON.stringify(report, null, 2),
    contentType: 'application/json',
  });
  console.log(`TinyJoin OPFS persistence: ${JSON.stringify(report)} ms`);

  expect(report.lockErrorCode).toBe('STORAGE_LOCKED');
  expect(report.differentNameOpened).toBe(true);
  expect(report.rowCount).toBe(10_000);
  expect(report.emptyTableRows).toBe(0);
  expect(report.revision).toBe(3);
  expect(report.updatedTitle).toBe('Persisted after forced termination');
  expect(report.schema).toEqual({
    version: 0,
    tables: [
      {
        name: 'empty_table',
        columns: [{name: 'id', type: 'integer', nullable: false}],
        primaryKey: ['id'],
        indexes: [],
        foreignKeys: [],
      },
      {
        name: 'posts',
        columns: [
          {name: 'id', type: 'integer', nullable: false},
          {name: 'title', type: 'text', nullable: false},
          {name: 'author', type: 'text', nullable: false},
          {name: 'published', type: 'boolean', nullable: false},
          {name: 'body', type: 'text', nullable: false},
        ],
        primaryKey: ['id'],
        indexes: [{name: 'posts_author', columns: ['author'], unique: false}],
        foreignKeys: [],
      },
    ],
  });
  for (const timing of [
    report.initialCommitMs,
    report.gracefulReopenMs,
    report.mutationCommitMs,
    report.crashReopenMs,
    report.checkMs,
  ]) {
    expect(Number.isFinite(timing) && timing >= 0).toBe(true);
  }
});

test('rehydrates OPFS after a browser process restart', async ({}, testInfo) => {
  const baseURL = testInfo.project.use.baseURL;
  if (typeof baseURL !== 'string') {
    throw new TypeError('The browser restart test requires a string baseURL');
  }
  const profile = await mkdtemp(join(tmpdir(), 'tinyjoin-profile-'));
  const databaseName = `browser-restart-${Date.now()}`;
  let context = await chromium.launchPersistentContext(profile, {
    headless: true,
  });
  try {
    let page = context.pages()[0] ?? (await context.newPage());
    await page.goto(baseURL);
    await expect(page.getByTestId('state')).toHaveText('Ready');
    await expect(
      page.evaluate(
        ({name, rows}) =>
          window.__tinyjoinTest!.writeBrowserRestartFixture(name, rows),
        {name: databaseName, rows: 1_024},
      ),
    ).resolves.toEqual({revision: 2});

    await context.close();
    context = await chromium.launchPersistentContext(profile, {
      headless: true,
    });
    page = context.pages()[0] ?? (await context.newPage());
    await page.goto(baseURL);
    await expect(page.getByTestId('state')).toHaveText('Ready');
    await expect(
      page.evaluate((name) =>
        window.__tinyjoinTest!.readBrowserRestartFixture(name),
      databaseName),
    ).resolves.toEqual({
      emptyTableRows: 0,
      revision: 2,
      rowCount: 1_024,
    });
  } finally {
    await context.close().catch(() => undefined);
    await rm(profile, {force: true, recursive: true});
  }
});
