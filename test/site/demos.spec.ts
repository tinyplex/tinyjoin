import {expect, test} from '@playwright/test';

test('generated Todo demo adds, toggles, deletes, and resets on reload', async ({
  page,
}) => {
  const pageErrors: string[] = [];
  page.on('pageerror', (error) => pageErrors.push(error.message));
  await page.goto('/demos/todo-starter/');

  const demo = page.frameLocator('#content iframe');
  const todos = demo.locator('#todos > li');
  await expect(todos.locator('span')).toHaveText([
    'Build an app',
    'Learn TinyJoin',
  ]);

  await demo.getByPlaceholder('What needs to be done?').fill('Review launch docs');
  await demo.getByRole('button', {name: 'Add', exact: true}).click();
  await expect(todos).toHaveCount(3);
  const added = todos.filter({hasText: 'Review launch docs'});
  await added.getByRole('checkbox').check();
  await expect(added).toHaveClass('done');
  await added.getByRole('checkbox').uncheck();
  await expect(added).not.toHaveClass('done');
  await added.getByRole('button', {name: 'Delete', exact: true}).click();
  await expect(todos).toHaveCount(2);
  await expect(added).toHaveCount(0);

  await todos
    .filter({hasText: 'Learn TinyJoin'})
    .getByRole('button', {name: 'Delete', exact: true})
    .click();
  await expect(todos).toHaveCount(1);
  await page.reload();
  await expect(todos.locator('span')).toHaveText([
    'Build an app',
    'Learn TinyJoin',
  ]);
  expect(pageErrors).toEqual([]);
});

test('generated bank demo commits both balances and rolls back the staged debit', async ({
  page,
}) => {
  const pageErrors: string[] = [];
  page.on('pageerror', (error) => pageErrors.push(error.message));
  await page.goto('/demos/bank-transactions/');

  const demo = page.frameLocator('#content iframe');
  const balances = demo.locator('#accounts > li b');
  await expect(demo.locator('#accounts > li span')).toHaveText(['Ada', 'Grace']);
  await expect(balances).toHaveText(['100', '100']);

  await demo.getByRole('button', {name: 'Transfer 25', exact: true}).click();
  await expect(demo.locator('#status')).toHaveText('Transferred 25.');
  await expect(balances).toHaveText(['75', '125']);

  await demo.getByRole('button', {name: 'Transfer 500', exact: true}).click();
  // The expected-error branch re-queries committed rows before showing this
  // message, so these balance assertions cannot pass against a stale view.
  await expect(demo.locator('#status')).toHaveText(
    'Ada cannot afford 500. The staged debit was rolled back; both balances are unchanged.',
  );
  await expect(balances).toHaveText(['75', '125']);

  await demo.getByRole('button', {name: 'Transfer 25', exact: true}).click();
  await expect(demo.locator('#status')).toHaveText('Transferred 25.');
  await expect(balances).toHaveText(['50', '150']);
  expect(pageErrors).toEqual([]);
});
