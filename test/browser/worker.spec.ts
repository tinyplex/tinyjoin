import {expect, test} from '@playwright/test';

test('queries and invalidates through the real Worker/WASM engine', async ({
  page,
}) => {
  const opfsRuntimeRequests: string[] = [];
  page.on('request', (request) => {
    if (isOpfsRuntimeRequest(request.url())) {
      opfsRuntimeRequests.push(request.url());
    }
  });
  await page.route(
    /\/worker-opfs\/tinygres_opfs_runtime\.js(?:\?.*)?$/,
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
  expect(opfsRuntimeRequests).toEqual([]);
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
    ddlCodes: Array(6).fill('UNSUPPORTED_SQL'),
    rejectedScriptRows: 0,
    revisionAfter: 2,
    revisionBefore: 2,
    stagedRows: 1,
  });
});

function isOpfsRuntimeRequest(url: string): boolean {
  return /\/worker-opfs\/tinygres_opfs_runtime\.js(?:\?.*)?$/.test(url);
}
