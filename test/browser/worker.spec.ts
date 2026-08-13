import {expect, test} from '@playwright/test';

test('queries and invalidates through the real Worker/WASM engine', async ({
  page,
}) => {
  const legacyRequests: string[] = [];
  page.on('request', (request) => {
    if (isLegacyRuntimeRequest(request.url())) {
      legacyRequests.push(request.url());
    }
  });
  await page.route(
    /(?:wasm-migration|worker-migration|tinygres_migration_runtime|migration-engine|persistent-engine|snapshot-store)/,
    (route) => route.abort(),
  );
  await page.goto('/');

  await expect(page.getByTestId('state')).toHaveText('Ready');
  await expect(page.getByTestId('status')).toContainText(
    'initial snapshot is queryable locally',
  );
  await expect(page.getByTestId('revision')).toHaveText('1');
  await expect(page.locator('[data-post-id]')).toHaveCount(2);
  await expect(page.locator('[data-post-id="2"]')).toContainText(
    'Queries stay off the main thread',
  );
  await expect(page.getByTestId('latency')).toContainText('2 rows returned');

  await page.getByTestId('apply-change').click();

  await expect(page.getByTestId('invalidations')).toHaveText('1');
  await expect(page.getByTestId('revision')).toHaveText('2');
  await expect(page.locator('[data-post-id="2"]')).toContainText(
    'Worker invalidation #1',
  );
  await expect(page.getByTestId('status')).toContainText(
    'Re-queried after invalidation at revision 2',
  );
  await expect(page.getByTestId('error')).toBeHidden();
  expect(legacyRequests).toEqual([]);
});

test('rejects DDL in a page-native transaction without losing rollback', async ({
  page,
}) => {
  await page.goto('/');
  await expect(page.getByTestId('state')).toHaveText('Ready');

  await expect(
    page.evaluate(() => window.__tinygresTest!.pageTransactionDdlProbe()),
  ).resolves.toEqual({
    committedRows: 0,
    ddlCodes: Array(5).fill('UNSUPPORTED_SQL'),
    revisionAfter: 2,
    revisionBefore: 2,
    stagedRows: 1,
  });
});

function isLegacyRuntimeRequest(url: string): boolean {
  return [
    '/wasm-migration/',
    '/worker-migration/',
    'tinygres_migration_runtime',
    '/worker/migration-engine.js',
    '/worker/persistent-engine.js',
    '/worker/snapshot-store.js',
  ].some((part) => url.includes(part));
}
