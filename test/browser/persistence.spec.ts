import {chromium, expect, test} from '@playwright/test';
import {mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

test('persists complete state across dedicated Worker restarts', async ({
  page,
}) => {
  const runtimeRequests: string[] = [];
  page.on('request', (request) => runtimeRequests.push(request.url()));
  await page.goto('/');
  await expect(page.getByTestId('state')).toHaveText('Ready');
  const databaseName = `playwright-${Date.now()}-${Math.floor(Math.random() * 1_000_000)}`;

  const report = await page.evaluate(
    ({name, rows}) => window.__tinygresTest!.persistenceProbe(name, rows),
    {name: databaseName, rows: 10_000},
  );

  await test.info().attach('opfs-persistence.json', {
    body: JSON.stringify(report, null, 2),
    contentType: 'application/json',
  });
  console.log(`TinyGres OPFS persistence: ${JSON.stringify(report)} ms`);

  expect(report.lockErrorCode).toBe('STORAGE_LOCKED');
  expect(report.conflictErrorCode).toBe('INVALID_SCHEMA');
  expect(report.differentNameOpened).toBe(true);
  expect(report.rowCount).toBe(10_000);
  expect(report.emptyTableRows).toBe(0);
  expect(report.revision).toBe(2);
  expect(report.updatedTitle).toBe('Persisted after forced termination');
  expect(report.journalWriteBytes).toBeLessThan(16_384);
  for (const timing of [
    report.initialCommitMs,
    report.gracefulReopenMs,
    report.mutationCommitMs,
    report.crashReopenMs,
  ]) {
    expect(Number.isFinite(timing) && timing >= 0).toBe(true);
  }
  expect(
    runtimeRequests.some((url) =>
      url.includes('/wasm-migration/tinygres_migration_wasm.js'),
    ),
  ).toBe(true);
});

test('rehydrates OPFS after a browser process restart', async ({}, testInfo) => {
  const baseURL = testInfo.project.use.baseURL;
  if (typeof baseURL !== 'string') {
    throw new TypeError('The browser restart test requires a string baseURL');
  }
  const profile = await mkdtemp(join(tmpdir(), 'tinygres-profile-'));
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
          window.__tinygresTest!.writeBrowserRestartFixture(name, rows),
        {name: databaseName, rows: 1_024},
      ),
    ).resolves.toEqual({revision: 1});

    await context.close();
    context = await chromium.launchPersistentContext(profile, {
      headless: true,
    });
    page = context.pages()[0] ?? (await context.newPage());
    await page.goto(baseURL);
    await expect(page.getByTestId('state')).toHaveText('Ready');
    await expect(
      page.evaluate((name) =>
        window.__tinygresTest!.readBrowserRestartFixture(name),
      databaseName),
    ).resolves.toEqual({
      emptyTableRows: 0,
      revision: 1,
      rowCount: 1_024,
    });
  } finally {
    await context.close().catch(() => undefined);
    await rm(profile, {force: true, recursive: true});
  }
});

test('keeps legacy transactional DDL available on the temporary OPFS path', async ({
  page,
}) => {
  await page.goto('/');
  await expect(page.getByTestId('state')).toHaveText('Ready');
  const databaseName = `legacy-ddl-${Date.now()}-${Math.floor(Math.random() * 1_000_000)}`;

  await expect(
    page.evaluate((name) =>
      window.__tinygresTest!.legacyTransactionDdlProbe(name),
    databaseName),
  ).resolves.toEqual({
    reopenedRevision: 1,
    reopenedRows: 1,
    stagedRows: 1,
  });
});
