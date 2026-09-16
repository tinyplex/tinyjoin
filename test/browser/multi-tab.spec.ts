import {expect, test, type BrowserContext, type Page} from '@playwright/test';
import type {OperationState} from './app/multi-tab.mts';

const readAll = 'SELECT * FROM items ORDER BY id';
const databaseName = (): string =>
  `multi-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;

async function fixture(context: BrowserContext): Promise<Page> {
  const page = await context.newPage();
  await page.goto('/multi-tab.html');
  await expect(page.getByTestId('state')).toHaveText('Ready');
  return page;
}

async function open(page: Page, name: string, alias = 'db'): Promise<void> {
  await page.evaluate(
    ({alias, name}) => window.__tinyjoinMultiTab.open(alias, name),
    {alias, name},
  );
}

async function state(page: Page, operation: string): Promise<OperationState> {
  return page.evaluate(
    (operation) => window.__tinyjoinMultiTab.operation(operation),
    operation,
  );
}

test('shares writes, subscriptions, and client-scoped prepared handles across tabs', async ({
  context,
}) => {
  const first = await fixture(context);
  const second = await fixture(context);
  const name = databaseName();
  await open(first, name);
  await first.evaluate(() => window.__tinyjoinMultiTab.setup('db'));
  await open(second, name);
  await open(second, name, 'another');
  await first.evaluate(() => window.__tinyjoinMultiTab.subscribe('db'));
  await second.evaluate(() => {
    window.__tinyjoinMultiTab.subscribe('db');
    window.__tinyjoinMultiTab.subscribe('another');
  });
  await first.evaluate(() =>
    window.__tinyjoinMultiTab.query('db', 'INSERT INTO items VALUES (1, $1)', [
      'first',
    ]),
  );
  await second.evaluate(() =>
    window.__tinyjoinMultiTab.query('db', 'INSERT INTO items VALUES (2, $1)', [
      'second',
    ]),
  );
  for (const [page, alias] of [
    [first, 'db'],
    [second, 'db'],
    [second, 'another'],
  ] as const) {
    await expect
      .poll(() =>
        page.evaluate(
          (alias) => window.__tinyjoinMultiTab.events(alias).at(-1),
          alias,
        ),
      )
      // The other tab's write arrives here naming the row it changed, so a subscriber can
      // refresh that row instead of re-reading the table.
      .toEqual({revision: 3, tables: ['items'], keys: {items: [{id: 2}]}});
    expect(
      await page.evaluate(
        ({alias, sql}) => window.__tinyjoinMultiTab.query(alias, sql),
        {alias, sql: readAll},
      ),
    ).toEqual([
      {id: 1, value: 'first'},
      {id: 2, value: 'second'},
    ]);
  }
  await second.evaluate(async () => {
    await window.__tinyjoinMultiTab.prepare(
      'db',
      'first-query',
      'SELECT id FROM items WHERE id = 1',
    );
    await window.__tinyjoinMultiTab.prepare(
      'another',
      'second-query',
      'SELECT id FROM items WHERE id = 2',
    );
  });
  expect(
    await second.evaluate(() =>
      window.__tinyjoinMultiTab.execute('first-query'),
    ),
  ).toEqual([{id: 1}]);
  expect(
    await second.evaluate(() =>
      window.__tinyjoinMultiTab.execute('second-query'),
    ),
  ).toEqual([{id: 2}]);
  expect(
    await second.evaluate(() =>
      window.__tinyjoinMultiTab.executeForeign('another', 'first-query'),
    ),
  ).toBe('PREPARED_STATEMENT_CLIENT_MISMATCH');
  await second.evaluate(() => window.__tinyjoinMultiTab.close('db'));
  expect(
    await second.evaluate(() =>
      window.__tinyjoinMultiTab.execute('second-query'),
    ),
  ).toEqual([{id: 2}]);
  await first.evaluate(() =>
    window.__tinyjoinMultiTab.query('db', 'INSERT INTO items VALUES (3, $1)', [
      'after-detach',
    ]),
  );
  await expect
    .poll(() =>
      second.evaluate(() => window.__tinyjoinMultiTab.events('another').at(-1)),
    )
    .toEqual({revision: 4, tables: ['items'], keys: {items: [{id: 3}]}});
});

test('serializes other clients around a complete transaction callback', async ({
  context,
}) => {
  const owner = await fixture(context);
  const follower = await fixture(context);
  const name = databaseName();
  await open(owner, name);
  await owner.evaluate(() => window.__tinyjoinMultiTab.setup('db'));
  await open(follower, name);
  await owner.evaluate(() =>
    window.__tinyjoinMultiTab.startTransaction('db', 'owner-tx', 1, 'owner'),
  );
  await expect
    .poll(() => state(owner, 'owner-tx'))
    .toMatchObject({entered: true, status: 'pending'});
  await follower.evaluate((sql) => {
    window.__tinyjoinMultiTab.startQuery('db', 'queued-read', sql);
    window.__tinyjoinMultiTab.startTransaction(
      'db',
      'follower-tx',
      2,
      'follower',
    );
  }, readAll);
  await follower.waitForTimeout(150);
  expect(await state(follower, 'queued-read')).toMatchObject({
    status: 'pending',
  });
  expect(await state(follower, 'follower-tx')).toMatchObject({
    entered: false,
    status: 'pending',
  });
  await owner.evaluate(() => window.__tinyjoinMultiTab.release('owner-tx'));
  await expect
    .poll(() => state(owner, 'owner-tx'))
    .toMatchObject({status: 'resolved'});
  await expect
    .poll(() => state(follower, 'queued-read'))
    .toMatchObject({status: 'resolved', rows: [{id: 1, value: 'owner'}]});
  await expect
    .poll(() => state(follower, 'follower-tx'))
    .toMatchObject({entered: true, status: 'pending'});
  await follower.evaluate(() =>
    window.__tinyjoinMultiTab.release('follower-tx'),
  );
  await expect
    .poll(() => state(follower, 'follower-tx'))
    .toMatchObject({
      status: 'resolved',
      rows: [
        {id: 1, value: 'owner'},
        {id: 2, value: 'follower'},
      ],
    });
});

test('hands off after leader close and rebinds existing prepared statements', async ({
  context,
}) => {
  const owner = await fixture(context);
  const follower = await fixture(context);
  const name = databaseName();
  await open(owner, name);
  await owner.evaluate(async () => {
    await window.__tinyjoinMultiTab.setup('db');
    await window.__tinyjoinMultiTab.query(
      'db',
      'INSERT INTO items VALUES (1, $1)',
      ['durable'],
    );
  });
  await open(follower, name);
  await follower.evaluate(() =>
    window.__tinyjoinMultiTab.prepare(
      'db',
      'lookup',
      'SELECT value FROM items WHERE id = $1',
    ),
  );
  expect(
    await follower.evaluate(() =>
      window.__tinyjoinMultiTab.execute('lookup', [1]),
    ),
  ).toEqual([{value: 'durable'}]);
  await owner.evaluate(() => window.__tinyjoinMultiTab.close('db'));
  expect(
    await follower.evaluate(() =>
      window.__tinyjoinMultiTab.executeInTransaction('db', 'lookup', [1]),
    ),
  ).toEqual([{value: 'durable'}]);
  expect(
    await follower.evaluate(() =>
      window.__tinyjoinMultiTab.execute('lookup', [1]),
    ),
  ).toEqual([{value: 'durable'}]);
  await follower.evaluate(() =>
    window.__tinyjoinMultiTab.query('db', 'INSERT INTO items VALUES (2, $1)', [
      'new-owner',
    ]),
  );
  await follower.evaluate(() =>
    window.__tinyjoinMultiTab.closePrepared('lookup'),
  );
  await follower.evaluate(() => window.__tinyjoinMultiTab.close('db'));
  const reopened = await fixture(context);
  await open(reopened, name);
  expect(
    await reopened.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('db', sql),
      readAll,
    ),
  ).toEqual([
    {id: 1, value: 'durable'},
    {id: 2, value: 'new-owner'},
  ]);
});

test('rejects a pending write on owner crash without replaying it after failover', async ({
  context,
}) => {
  const owner = await fixture(context);
  const follower = await fixture(context);
  const name = databaseName();
  await open(owner, name);
  await owner.evaluate(async () => {
    await window.__tinyjoinMultiTab.setup('db');
    await window.__tinyjoinMultiTab.query(
      'db',
      'INSERT INTO items VALUES (1, $1)',
      ['durable'],
    );
  });
  await open(follower, name);
  await owner.evaluate(() =>
    window.__tinyjoinMultiTab.startTransaction('db', 'unfinished', 2, 'staged'),
  );
  await expect
    .poll(() => state(owner, 'unfinished'))
    .toMatchObject({entered: true});
  await follower.evaluate(() =>
    window.__tinyjoinMultiTab.startQuery(
      'db',
      'pending-write',
      'INSERT INTO items VALUES (3, $1)',
      ['must-not-replay'],
    ),
  );
  await follower.waitForTimeout(150);
  expect(await state(follower, 'pending-write')).toMatchObject({
    status: 'pending',
  });
  await owner.close();
  await expect
    .poll(() => state(follower, 'pending-write'))
    .toMatchObject({status: 'rejected', code: 'LEADER_CHANGED'});
  expect(
    await follower.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('db', sql),
      readAll,
    ),
  ).toEqual([{id: 1, value: 'durable'}]);
  await follower.evaluate(() =>
    window.__tinyjoinMultiTab.query('db', 'INSERT INTO items VALUES (4, $1)', [
      'explicit-new-write',
    ]),
  );
  expect(
    await follower.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('db', sql),
      readAll,
    ),
  ).toEqual([
    {id: 1, value: 'durable'},
    {id: 4, value: 'explicit-new-write'},
  ]);
});

test('rejects a stale follower transaction token after owner crash', async ({
  context,
}) => {
  const owner = await fixture(context);
  const follower = await fixture(context);
  await follower.evaluate(() => window.__tinyjoinMultiTab.captureResyncs());
  const name = databaseName();
  await open(owner, name);
  await owner.evaluate(async () => {
    await window.__tinyjoinMultiTab.setup('db');
    await window.__tinyjoinMultiTab.query(
      'db',
      'INSERT INTO items VALUES (1, $1)',
      ['durable'],
    );
  });
  await open(follower, name);
  await follower.evaluate(() => {
    window.__tinyjoinMultiTab.subscribe('db');
    window.__tinyjoinMultiTab.startTransaction('db', 'lost-tx', 2, 'staged');
  });
  await expect
    .poll(() => state(follower, 'lost-tx'))
    .toMatchObject({entered: true});
  const before = await follower.evaluate(() =>
    window.__tinyjoinMultiTab.receivedResyncs(),
  );
  await owner.close();
  // Handover reaches the Worker while the callback is paused, but public
  // subscription delivery waits until that callback settles so listeners can
  // safely re-query. Observe the transport here, not the deferred subscription.
  await expect
    .poll(() =>
      follower.evaluate(() => window.__tinyjoinMultiTab.receivedResyncs()),
    )
    .toBeGreaterThan(before);
  expect(
    await follower.evaluate(() =>
      window.__tinyjoinMultiTab.events('db').some(event => event.tables.length === 0),
    ),
  ).toBe(false);
  await follower.evaluate(() => window.__tinyjoinMultiTab.release('lost-tx'));
  await expect
    .poll(() => state(follower, 'lost-tx'))
    .toMatchObject({status: 'rejected', code: 'TRANSACTION_LOST'});
  await expect
    .poll(() =>
      follower.evaluate(() =>
        window.__tinyjoinMultiTab.events('db').some(event => event.tables.length === 0),
      ),
    )
    .toBe(true);
  expect(
    await follower.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('db', sql),
      readAll,
    ),
  ).toEqual([{id: 1, value: 'durable'}]);
});

for (const disappearance of ['client-close', 'page-close'] as const) {
  test(`rolls back a follower transaction after ${disappearance}`, async ({
    context,
  }) => {
    const owner = await fixture(context);
    const follower = await fixture(context);
    const name = databaseName();
    await open(owner, name);
    await owner.evaluate(() => window.__tinyjoinMultiTab.setup('db'));
    await open(follower, name);
    await follower.evaluate(() =>
      window.__tinyjoinMultiTab.startTransaction(
        'db',
        'unfinished',
        1,
        'discard',
      ),
    );
    await expect
      .poll(() => state(follower, 'unfinished'))
      .toMatchObject({entered: true});
    await owner.evaluate(
      (sql) => window.__tinyjoinMultiTab.startQuery('db', 'waiting', sql),
      readAll,
    );
    await owner.waitForTimeout(150);
    expect(await state(owner, 'waiting')).toMatchObject({status: 'pending'});
    if (disappearance === 'client-close') {
      await follower.evaluate(() => window.__tinyjoinMultiTab.close('db'));
      await follower.evaluate(() =>
        window.__tinyjoinMultiTab.release('unfinished'),
      );
      await expect
        .poll(() => state(follower, 'unfinished'))
        .toMatchObject({status: 'rejected'});
    } else {
      await follower.close();
    }
    await expect
      .poll(() => state(owner, 'waiting'))
      .toMatchObject({status: 'resolved', rows: []});
    await owner.evaluate(() =>
      window.__tinyjoinMultiTab.query(
        'db',
        'INSERT INTO items VALUES (2, $1)',
        ['continued'],
      ),
    );
    expect(
      await owner.evaluate(
        (sql) => window.__tinyjoinMultiTab.query('db', sql),
        readAll,
      ),
    ).toEqual([{id: 2, value: 'continued'}]);
  });
}

test('keeps different OPFS names independent while a transaction is held', async ({
  context,
}) => {
  const page = await fixture(context);
  const name = databaseName();
  await open(page, name);
  await open(page, `${name}-independent`, 'independent');
  await page.evaluate(async () => {
    await window.__tinyjoinMultiTab.setup('db');
    await window.__tinyjoinMultiTab.setup('independent');
    window.__tinyjoinMultiTab.startTransaction('db', 'held', 1, 'isolated');
  });
  await expect.poll(() => state(page, 'held')).toMatchObject({entered: true});
  expect(
    await page.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('independent', sql),
      readAll,
    ),
  ).toEqual([]);
  await page.evaluate(() =>
    window.__tinyjoinMultiTab.query(
      'independent',
      'INSERT INTO items VALUES (2, $1)',
      ['independent'],
    ),
  );
  await page.evaluate(() => window.__tinyjoinMultiTab.release('held'));
  await expect
    .poll(() => state(page, 'held'))
    .toMatchObject({status: 'resolved'});
  expect(
    await page.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('db', sql),
      readAll,
    ),
  ).toEqual([{id: 1, value: 'isolated'}]);
});

test('rejects an incompatible owner instead of joining its protocol', async ({
  context,
}) => {
  const incompatible = await fixture(context);
  const newcomer = await fixture(context);
  const name = databaseName();
  await incompatible.evaluate(
    (name) => window.__tinyjoinMultiTab.advertiseIncompatible(name),
    name,
  );
  const code = await newcomer.evaluate(async (name) => {
    try {
      await window.__tinyjoinMultiTab.open('db', name);
      return '';
    } catch (error) {
      return typeof error === 'object' && error !== null && 'code' in error
        ? String(error.code)
        : String(error);
    }
  }, name);
  expect(code).toBe('DATABASE_VERSION_MISMATCH');
});

test('ignores an obsolete owner announcement after handoff', async ({
  context,
}) => {
  const observer = await fixture(context);
  const owner = await fixture(context);
  const follower = await fixture(context);
  const name = databaseName();
  await observer.evaluate(
    (name) => window.__tinyjoinMultiTab.observeLeader(name),
    name,
  );
  await open(owner, name);
  await owner.evaluate(() => window.__tinyjoinMultiTab.setup('db'));
  await expect
    .poll(() =>
      observer.evaluate(
        (name) => window.__tinyjoinMultiTab.leaderObserved(name),
        name,
      ),
    )
    .toBe(true);
  await open(follower, name);
  await owner.evaluate(() => window.__tinyjoinMultiTab.close('db'));
  expect(
    await follower.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('db', sql),
      readAll,
    ),
  ).toEqual([]);
  await observer.evaluate(
    (name) => window.__tinyjoinMultiTab.replayLeader(name),
    name,
  );
  await follower.waitForTimeout(100);
  await follower.evaluate(() =>
    window.__tinyjoinMultiTab.query('db', 'INSERT INTO items VALUES (1, $1)', [
      'current-owner',
    ]),
  );
  expect(
    await follower.evaluate(
      (sql) => window.__tinyjoinMultiTab.query('db', sql),
      readAll,
    ),
  ).toEqual([{id: 1, value: 'current-owner'}]);
});

test('defers resume notifications until their listener can re-query after a transaction', async ({context}) => {
  const page = await fixture(context);
  await page.evaluate(() => window.__tinyjoinMultiTab.captureResyncs());
  await open(page, databaseName());
  await page.evaluate(async () => {
    await window.__tinyjoinMultiTab.setup('db');
    window.__tinyjoinMultiTab.startTransaction('db', 'held', 1, 'committed');
  });
  await expect.poll(() => state(page, 'held')).toMatchObject({entered: true});
  await page.evaluate(() => window.__tinyjoinMultiTab.subscribeAndRequery('db'));
  const before = await page.evaluate(() => window.__tinyjoinMultiTab.receivedResyncs());
  await page.evaluate(() => {
    // Exercise lifecycle listeners deterministically; this does not claim actual
    // BFCache admission. Observe real Worker resync messages before assertions.
    window.dispatchEvent(new PageTransitionEvent('pageshow', {persisted: true}));
    document.dispatchEvent(new Event('resume'));
  });
  await expect.poll(() => page.evaluate(() => window.__tinyjoinMultiTab.receivedResyncs())).toBeGreaterThanOrEqual(before + 2);
  expect(await page.evaluate(() => window.__tinyjoinMultiTab.requeries('db'))).toEqual({calls: 0, resets: 0, errors: [], rows: []});
  await page.evaluate(() => window.__tinyjoinMultiTab.release('held'));
  await expect.poll(() => state(page, 'held')).toMatchObject({status: 'resolved'});
  await expect.poll(() => page.evaluate(() => window.__tinyjoinMultiTab.requeries('db').rows.length)).toBeGreaterThan(0);
  const observed = await page.evaluate(() => window.__tinyjoinMultiTab.requeries('db'));
  expect(observed.resets).toBe(1);
  expect(observed.errors).toEqual([]);
  expect(observed.rows.every(rows => JSON.stringify(rows) === JSON.stringify([{id: 1, value: 'committed'}]))).toBe(true);
});
