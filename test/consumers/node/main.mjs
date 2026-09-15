import assert from 'node:assert/strict';

const originalWorker = Object.getOwnPropertyDescriptor(globalThis, 'Worker');
const originalFetch = Object.getOwnPropertyDescriptor(globalThis, 'fetch');
const {ClientError: BrowserClientError} = await import('tinyjoin');
const {ClientError, create} = await import('tinyjoin/node');
assert.equal(ClientError, BrowserClientError);

const clients = [];
const schema = `
  CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  )
`;
const isClientError = (code) => (error) =>
  error instanceof ClientError && error.code === code;

try {
  const database = await create();
  clients.push(database);
  const independent = await create('memory://');
  clients.push(independent);
  assert.equal(database.ready, true);
  assert.equal(database.closed, false);

  await database.exec(schema);
  await database.exec(schema);
  await independent.exec(schema);
  const title = "Node's $1; SELECT * FROM tasks";
  await database.query('INSERT INTO tasks (id, title) VALUES ($1, $2)', [
    'first',
    title,
  ]);
  await independent.query('INSERT INTO tasks (id, title) VALUES ($1, $2)', [
    'first',
    'independent',
  ]);
  const selectTask = await database.prepare(
    'SELECT id, title, done FROM tasks WHERE id = $1',
  );
  assert.deepEqual((await selectTask.execute(['first'])).rows, [
    {id: 'first', title, done: false},
  ]);

  const updateTask = await database.prepare(
    'UPDATE tasks SET title = $1, done = $2 WHERE id = $3',
  );
  let unsubscribe;
  const changed = new Promise((resolve, reject) => {
    unsubscribe = database.subscribe({tables: ['tasks']}, (event) => {
      unsubscribe();
      // A subscription identifies changed tables; application code reads rows.
      database
        .query('SELECT id, title, done FROM tasks ORDER BY id')
        .then((result) => resolve({event, result}), reject);
    });
  });
  const returned = await database.transaction(async (transaction) => {
    await transaction.execute(updateTask, ['committed', true, 'first']);
    await transaction.query(
      'INSERT INTO tasks (id, title) VALUES ($1, $2)',
      ['second', 'committed together'],
    );
    return 'transaction result';
  });
  assert.equal(returned, 'transaction result');
  const {event, result} = await changed;
  assert.deepEqual(event.tables, ['tasks']);
  assert.equal(event.revision, result.revision);
  assert.deepEqual(result.rows, [
    {id: 'first', title: 'committed', done: true},
    {id: 'second', title: 'committed together', done: false},
  ]);

  const rollbackError = new Error('discard staged writes');
  await assert.rejects(
    database.transaction(async (transaction) => {
      await transaction.execute(updateTask, ['discarded', false, 'first']);
      throw rollbackError;
    }),
    (error) => error === rollbackError,
  );
  await database.transaction(async (transaction) => {
    await transaction.query('DELETE FROM tasks WHERE id = $1', ['second']);
    await transaction.rollback();
  });
  const afterRollback = await database.query(
    'SELECT id, title, done FROM tasks ORDER BY id',
  );
  assert.deepEqual(afterRollback.rows, result.rows);
  assert.equal(afterRollback.revision, result.revision);

  await selectTask.close();
  assert.equal(selectTask.closed, true);
  await assert.rejects(
    selectTask.execute(['first']),
    isClientError('PREPARED_STATEMENT_CLOSED'),
  );
  await updateTask.close();
  await database.close();
  await database.close();
  assert.equal(database.closed, true);
  await assert.rejects(
    database.query('SELECT id FROM tasks'),
    isClientError('CLIENT_CLOSED'),
  );
  assert.deepEqual(
    (await independent.query('SELECT title FROM tasks')).rows,
    [{title: 'independent'}],
  );

  const reopened = await create('memory://');
  clients.push(reopened);
  await reopened.exec(schema);
  assert.deepEqual((await reopened.query('SELECT id FROM tasks')).rows, []);

  for (const unsupported of [
    'opfs://node-test',
    'file:///tmp/tinyjoin-node-test',
    './tinyjoin-node-test',
    '',
    'memory://named',
    null,
    1,
    {},
    {dataDir: 'memory://'},
  ]) {
    await assert.rejects(
      async () => create(unsupported),
      /memory:\/\//,
      `Accepted unsupported Node data directory: ${JSON.stringify(unsupported)}`,
    );
  }

  await assert.rejects(
    import('tinyjoin/node/worker-entry.js'),
    {code: 'ERR_PACKAGE_PATH_NOT_EXPORTED'},
  );
} finally {
  await Promise.all(clients.map((client) => client.close()));
}

assert.deepEqual(
  Object.getOwnPropertyDescriptor(globalThis, 'Worker'),
  originalWorker,
);
assert.deepEqual(
  Object.getOwnPropertyDescriptor(globalThis, 'fetch'),
  originalFetch,
);
// The parent requires a natural exit within its timeout. A leaked Worker keeps
// this process alive, even after the successful result has been printed.
console.log('NODE_MEMORY_CONSUMER_OK');
