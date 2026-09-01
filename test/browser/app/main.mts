import {Client, create} from 'tinyjoin';

type Post = {
  id: number;
  title: string;
  author: string;
  published: boolean;
};

const stateElement = element<HTMLElement>('[data-testid="state"]');
const revisionElement = element<HTMLElement>('[data-testid="revision"]');
const postsElement = element<HTMLUListElement>('[data-testid="posts"]');
const latencyElement = element<HTMLElement>('[data-testid="latency"]');
const invalidationsElement = element<HTMLElement>(
  '[data-testid="invalidations"]',
);
const statusElement = element<HTMLElement>('[data-testid="status"]');
const errorElement = element<HTMLElement>('[data-testid="error"]');
const applyButton = element<HTMLButtonElement>(
  '[data-testid="apply-change"]',
);

let invalidations = 0;
let simulatedChanges = 0;

async function boot(): Promise<void> {
  const database = await create();
  await database.exec(`
    CREATE TABLE posts (
      id INTEGER PRIMARY KEY,
      title TEXT NOT NULL,
      author TEXT NOT NULL,
      published BOOLEAN NOT NULL
    );
    INSERT INTO posts (id, title, author, published) VALUES
      (1, 'The worker owns the database', 'Ada', true),
      (2, 'Queries stay off the main thread', 'Linus', true),
      (3, 'Drafts remain filtered locally', 'Grace', false);
  `);
  await renderQuery(database);

  const unsubscribe = database.subscribe({tables: ['posts']}, (event) => {
    invalidations += 1;
    invalidationsElement.textContent = String(invalidations);
    statusElement.textContent = `Revision ${event.revision} invalidated ${event.tables.join(', ')}; re-querying…`;
    void renderQuery(database).then(() => {
      statusElement.textContent = `Re-queried after invalidation at revision ${event.revision}.`;
    }, showError);
  });

  applyButton.addEventListener('click', async () => {
    applyButton.disabled = true;
    simulatedChanges += 1;

    try {
      await database.query('UPDATE posts SET title = $1 WHERE id = $2', [
        `Worker invalidation #${simulatedChanges}`,
        2,
      ]);
    } catch (error) {
      showError(error);
    } finally {
      applyButton.disabled = false;
    }
  });

  window.__tinyjoinTest = {
    async benchmark(iterations) {
      if (!Number.isSafeInteger(iterations) || iterations < 1) {
        throw new TypeError('Benchmark iterations must be a positive integer');
      }
      const samples: number[] = [];
      for (let iteration = 0; iteration < iterations; iteration += 1) {
        const startedAt = performance.now();
        const result = await database.query<Post>(
          'SELECT id, title FROM posts WHERE published = $1',
          [true],
        );
        samples.push(performance.now() - startedAt);
        if (result.rows.length !== 2) {
          throw new Error('Benchmark query returned an unexpected row count');
        }
      }
      return samples;
    },
    joinDatabaseProbe,
    pageTransactionDdlProbe: () => pageTransactionDdlProbe(database),
    persistenceProbe,
    writableDatabaseProbe,
    readBrowserRestartFixture,
    writeBrowserRestartFixture,
  };

  window.addEventListener(
    'pagehide',
    () => {
      unsubscribe();
      delete window.__tinyjoinTest;
      void database.close();
    },
    {once: true},
  );

  stateElement.textContent = 'Ready';
  statusElement.textContent = 'Ready. The initial SQL data is queryable locally.';
  applyButton.disabled = false;
}

async function pageTransactionDdlProbe(
  database: Awaited<ReturnType<typeof create>>,
): Promise<{
  committedRows: number;
  ddlCodes: string[];
  rejectedScriptRows: number;
  revisionAfter: number;
  revisionBefore: number;
  stagedRows: number;
}> {
  await database.exec(`
    CREATE TABLE page_tx_probe (
      id INTEGER PRIMARY KEY,
      note TEXT NOT NULL
    )
  `);
  const before = await database.query(
    'SELECT id FROM page_tx_probe WHERE id = $1',
    [99],
  );
  const ddlCodes: string[] = [];
  let stagedRows = 0;
  try {
    await database.transaction(async (transaction) => {
      await transaction.query(
        'INSERT INTO page_tx_probe (id, note) VALUES ($1, $2)',
        [99, 'Staged before rejected DDL'],
      );
      for (const sql of [
        `CREATE TABLE IF NOT EXISTS page_tx_probe (
           id INTEGER PRIMARY KEY,
           note TEXT NOT NULL
         )`,
        'DROP TABLE IF EXISTS absent_page_tx_probe',
        'CREATE INDEX IF NOT EXISTS page_tx_note ON page_tx_probe (note)',
        'DROP INDEX IF EXISTS absent_page_tx_index',
        'ALTER TABLE page_tx_probe ADD COLUMN extra TEXT',
        `INSERT INTO page_tx_probe (id, note)
           VALUES (100, 'Must not stage before rejected DDL');
         ALTER TABLE page_tx_probe ADD COLUMN rejected_extra TEXT`,
      ]) {
        try {
          await transaction.exec(sql);
        } catch (error) {
          ddlCodes.push(errorCode(error));
        }
      }
      stagedRows = (
        await transaction.query(
          'SELECT id FROM page_tx_probe WHERE id = $1',
          [99],
        )
      ).rows.length;
      const rejectedScriptRows = (
        await transaction.query(
          'SELECT id FROM page_tx_probe WHERE id = $1',
          [100],
        )
      ).rows.length;
      if (rejectedScriptRows !== 0) {
        throw new Error('Rejected transaction script staged its leading DML');
      }
      throw Object.assign(new Error('Roll back the browser DDL probe'), {
        code: 'EXPECTED_TEST_ROLLBACK',
      });
    });
  } catch (error) {
    if (errorCode(error) !== 'EXPECTED_TEST_ROLLBACK') {
      throw error;
    }
  }
  const after = await database.query(
    'SELECT id FROM page_tx_probe WHERE id = $1',
    [99],
  );
  return {
    committedRows: after.rows.length,
    ddlCodes,
    rejectedScriptRows: 0,
    revisionAfter: after.revision,
    revisionBefore: before.revision,
    stagedRows,
  };
}

async function writableDatabaseProbe(databaseName: string): Promise<{
  aggregateRows: Array<{done: boolean; task_count: number}>;
  bootstrapCommands: Array<string | undefined>;
  bootstrapRevisions: number[];
  committedRevision: number;
  insertRows: Array<{done: boolean; id: number; title: string}>;
  invalidations: Array<{revision: number; tables: string[]}>;
  orderedRows: Array<{done: boolean; id: number; title: string}>;
  preparedClosed: boolean;
  preparedRows: Array<{done: boolean; id: number; title: string}>;
  reopenedPriorities: Array<{id: number; priority: number}>;
  reopenedRevision: number;
  reopenedRows: Array<{done: boolean; id: number; title: string}>;
  rollbackCode: string;
  scriptFailureCode: string;
  scriptTableCode: string;
  stagedRows: Array<{done: boolean; id: number; title: string}>;
}> {
  const connection = openOpfsClient(databaseName);
  const events: Array<{revision: number; tables: string[]}> = [];
  const unsubscribe = connection.client.subscribe({}, (event) => {
    events.push(structuredClone(event));
  });
  try {
    await connection.client.waitReady;
    const bootstrap = await connection.client.exec(`
      CREATE TABLE tasks (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL,
        done BOOLEAN DEFAULT false,
        metadata JSON
      );
      CREATE UNIQUE INDEX tasks_title ON tasks (title);
      ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
      INSERT INTO tasks (id, title, metadata) VALUES
        (1, 'Ship writable SQL', NULL),
        (2, 'Persist it', NULL)
      RETURNING id, title, done;
      SELECT id, title, done FROM tasks ORDER BY id;
    `);
    if (bootstrap.length !== 5) {
      throw new Error('Mixed bootstrap script returned the wrong result count');
    }
    const insertRows = bootstrap[3]!.rows as Array<{
      done: boolean;
      id: number;
      title: string;
    }>;
    const taskById = await connection.client.prepare<{
      done: boolean;
      id: number;
      title: string;
    }>('SELECT id, title, done FROM tasks WHERE id = $1');
    let preparedRows: Array<{done: boolean; id: number; title: string}> = [];
    try {
      preparedRows = [
        ...(await taskById.execute([1])).rows,
        ...(await taskById.execute([2])).rows,
      ];
    } finally {
      await taskById.close();
    }

    const stagedRows = await connection.client.transaction(
      async (transaction) => {
        await transaction.query(
          'UPDATE tasks SET done = true WHERE id = $1 RETURNING id',
          [1],
        );
        await transaction.query(
          'INSERT INTO tasks (id, title) VALUES ($1, $2)',
          [3, 'Rollback safely'],
        );
        const result = await transaction.query<{
          done: boolean;
          id: number;
          title: string;
        }>('SELECT id, title, done FROM tasks');
        return result.rows.sort((left, right) => left.id - right.id);
      },
    );

    let rollbackCode = '';
    try {
      await connection.client.transaction(async (transaction) => {
        await transaction.query(
          'UPDATE tasks SET title = $1 WHERE id = $2',
          ['must not persist', 1],
        );
        await transaction.query(
          'INSERT INTO tasks (id, title) VALUES ($1, $2)',
          [4, 'Persist it'],
        );
      });
    } catch (error) {
      rollbackCode = errorCode(error);
    }

    let scriptFailureCode = '';
    try {
      await connection.client.exec(`
        UPDATE tasks SET title = 'must not persist' WHERE id = 1;
        CREATE TABLE script_abort_probe (id INTEGER PRIMARY KEY);
        INSERT INTO tasks (id, title) VALUES (5, 'Persist it');
      `);
    } catch (error) {
      scriptFailureCode = errorCode(error);
    }
    let scriptTableCode = '';
    try {
      await connection.client.query('SELECT * FROM script_abort_probe');
    } catch (error) {
      scriptTableCode = errorCode(error);
    }

    const ordered = await connection.client.query<{
      done: boolean;
      id: number;
      title: string;
    }>(
      `SELECT id, title, done FROM tasks
       WHERE done IS NOT NULL AND (id >= $1 OR title = $2)
       ORDER BY done DESC, id DESC LIMIT 2 OFFSET 1`,
      [1, 'missing'],
    );
    const aggregateRows = await connection.client.query<{
      done: boolean;
      task_count: number;
    }>(
      `SELECT done, COUNT(*) AS task_count FROM tasks
       GROUP BY done ORDER BY task_count DESC`,
    );

    const committedRevision = connection.client.getRevision();
    unsubscribe();
    await connection.client.close();

    const reopened = openOpfsClient(databaseName);
    try {
      await reopened.client.waitReady;
      const result = await reopened.client.query<{
        done: boolean;
        id: number;
        title: string;
      }>('SELECT id, title, done FROM tasks');
      const priorities = await reopened.client.query<{
        id: number;
        priority: number;
      }>('SELECT id, priority FROM tasks ORDER BY id');
      return {
        aggregateRows: aggregateRows.rows,
        bootstrapCommands: bootstrap.map((result) => result.command),
        bootstrapRevisions: bootstrap.map((result) => result.revision),
        committedRevision,
        insertRows,
        invalidations: events,
        orderedRows: ordered.rows,
        preparedClosed: taskById.closed,
        preparedRows,
        reopenedRevision: result.revision,
        reopenedPriorities: priorities.rows,
        reopenedRows: result.rows.sort((left, right) => left.id - right.id),
        rollbackCode,
        scriptFailureCode,
        scriptTableCode,
        stagedRows,
      };
    } finally {
      await reopened.client.close();
    }
  } finally {
    unsubscribe();
    connection.worker.terminate();
  }
}

async function joinDatabaseProbe(): Promise<{
  innerRows: Array<{
    author_id: number;
    author_name: string;
    article_id: number;
    article_title: string;
  }>;
  leftRows: Array<{
    author_id: number;
    author_name: string;
    article_id: number | null;
  }>;
  manyToManyRows: Array<{
    post_id: number;
    post_title: string;
    tag_id: number;
    tag_name: string;
  }>;
}> {
  const database = await create();
  try {
    await database.exec(`
      CREATE TABLE authors (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL
      )
    `);
    await database.exec(`
      CREATE TABLE articles (
        id INTEGER PRIMARY KEY,
        author_id INTEGER,
        title TEXT NOT NULL,
        published BOOLEAN NOT NULL
      )
    `);
    await database.query(
      `INSERT INTO authors (id, name) VALUES
       (1, 'Ada'), (2, 'Linus'), (3, 'Grace')`,
    );
    await database.query(
      `INSERT INTO articles (id, author_id, title, published) VALUES
       (10, 1, 'Worker databases', true),
       (11, 1, 'A second article', false),
       (12, 2, 'Local queries', true),
       (13, NULL, 'Unassigned draft', true)`,
    );
    await database.exec(`
      CREATE TABLE posts (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL
      )
    `);
    await database.exec(`
      CREATE TABLE tags (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL
      )
    `);
    await database.exec(`
      CREATE TABLE post_tags (
        post_id INTEGER NOT NULL,
        tag_id INTEGER NOT NULL,
        PRIMARY KEY (post_id, tag_id)
      )
    `);
    await database.query(
      `INSERT INTO posts (id, title) VALUES
       (1, 'Worker databases'), (2, 'Local queries'), (3, 'Untagged')`,
    );
    await database.query(
      `INSERT INTO tags (id, name) VALUES
       (10, 'wasm'), (11, 'offline'), (12, 'unused')`,
    );
    await database.query(
      `INSERT INTO post_tags (post_id, tag_id) VALUES
       (1, 10), (1, 11), (2, 11)`,
    );

    const inner = await database.query<{
      author_id: number;
      author_name: string;
      article_id: number;
      article_title: string;
    }>(
      `SELECT a.id AS author_id, a.name AS author_name,
              article.id AS article_id, article.title AS article_title
       FROM authors AS a INNER JOIN articles AS article
         ON a.id = article.author_id
       WHERE article.published = $1 AND a.id >= $2
       ORDER BY article.id`,
      [true, 1],
    );
    const left = await database.query<{
      author_id: number;
      author_name: string;
      article_id: number | null;
    }>(
      `SELECT author.id AS author_id, author.name AS author_name,
              article.id AS article_id
       FROM authors author LEFT OUTER JOIN articles article
         ON author.id = article.author_id
       ORDER BY author_id, article_id NULLS LAST`,
    );
    const manyToMany = await database.query<{
      post_id: number;
      post_title: string;
      tag_id: number;
      tag_name: string;
    }>(
      `SELECT post.id AS post_id, post.title AS post_title,
              tag.id AS tag_id, tag.name AS tag_name
       FROM posts AS post
       INNER JOIN post_tags AS post_tag ON post.id = post_tag.post_id
       INNER JOIN tags AS tag ON post_tag.tag_id = tag.id
       ORDER BY post_id, tag_id`,
    );
    return {
      innerRows: inner.rows,
      leftRows: left.rows,
      manyToManyRows: manyToMany.rows,
    };
  } finally {
    await database.close();
  }
}

async function renderQuery(
  database: Awaited<ReturnType<typeof create>>,
): Promise<void> {
  const startedAt = performance.now();
  const result = await database.query<Post>(
    'SELECT id, title, author FROM posts WHERE published = $1',
    [true],
  );
  const elapsed = performance.now() - startedAt;

  revisionElement.textContent = String(result.revision);
  latencyElement.textContent = `${result.rows.length} rows returned in ${elapsed.toFixed(2)} ms round-trip`;
  postsElement.replaceChildren(
    ...result.rows.map((post) => {
      const item = document.createElement('li');
      item.dataset.postId = String(post.id);
      item.textContent = `${post.title} — ${post.author}`;
      return item;
    }),
  );
}

function showError(error: unknown): void {
  const message = error instanceof Error ? error.message : String(error);
  stateElement.textContent = 'Failed';
  statusElement.textContent = 'The browser test flow failed.';
  errorElement.hidden = false;
  errorElement.textContent = message;
  applyButton.disabled = true;
}

function element<ElementType extends Element>(selector: string): ElementType {
  const match = document.querySelector<ElementType>(selector);
  if (!match) {
    throw new Error(`Test fixture element not found: ${selector}`);
  }
  return match;
}

void boot().catch(showError);

async function persistenceProbe(
  databaseName: string,
  rowCount: number,
): Promise<{
  crashReopenMs: number;
  differentNameOpened: boolean;
  emptyTableRows: number;
  gracefulReopenMs: number;
  initialCommitMs: number;
  lockErrorCode: string;
  mutationCommitMs: number;
  revision: number;
  rowCount: number;
  updatedTitle: string;
}> {
  if (!Number.isSafeInteger(rowCount) || rowCount < 1 || rowCount > 10_000) {
    throw new TypeError('Persistence row count must be between 1 and 10,000');
  }
  const rows = Array.from({length: rowCount}, (_, id) => ({
    id,
    title: `Persisted post ${id}`,
    author: `Author ${id % 17}`,
    published: id % 2 === 0,
    body: `Bounded persistence fixture ${id} ${'x'.repeat(64)}`,
  }));

  let first: ReturnType<typeof openOpfsClient> | undefined =
    openOpfsClient(databaseName);
  let reopened: ReturnType<typeof openOpfsClient> | undefined;
  let afterCrash: ReturnType<typeof openOpfsClient> | undefined;
  try {
    await first.client.waitReady;
    await createPersistenceTables(first.client);
    const commitStartedAt = performance.now();
    await insertPersistencePosts(first.client, rows);
    const initialCommitMs = performance.now() - commitStartedAt;

    const competing = openOpfsClient(databaseName);
    let lockErrorCode = '';
    try {
      await competing.client.waitReady;
    } catch (error) {
      lockErrorCode = errorCode(error);
    } finally {
      competing.worker.terminate();
    }

    const independent = openOpfsClient(`${databaseName}-independent`);
    await independent.client.waitReady;
    await independent.client.close();

    await first.client.close();
    first = undefined;

    const reopenStartedAt = performance.now();
    reopened = openOpfsClient(databaseName);
    await reopened.client.waitReady;
    const gracefulReopenMs = performance.now() - reopenStartedAt;
    const restored = await reopened.client.query<{
      id: number;
      title: string;
    }>('SELECT id, title FROM posts');
    const empty = await reopened.client.query('SELECT * FROM empty_table');

    const mutationStartedAt = performance.now();
    await reopened.client.query('UPDATE posts SET title = $1 WHERE id = $2', [
      'Persisted after forced termination',
      rows[0]!.id,
    ]);
    const mutationCommitMs = performance.now() - mutationStartedAt;
    reopened.worker.terminate();
    reopened = undefined;

    const crashReopenStartedAt = performance.now();
    afterCrash = await reopenAfterTermination(databaseName);
    const crashReopenMs = performance.now() - crashReopenStartedAt;
    const afterCrashResult = await afterCrash.client.query<{
      id: number;
      title: string;
    }>('SELECT id, title FROM posts WHERE id = $1', [0]);

    const report = {
      crashReopenMs,
      differentNameOpened: true,
      emptyTableRows: empty.rows.length,
      gracefulReopenMs,
      initialCommitMs,
      lockErrorCode,
      mutationCommitMs,
      revision: afterCrashResult.revision,
      rowCount: restored.rows.length,
      updatedTitle: afterCrashResult.rows[0]?.title ?? '',
    };
    await afterCrash.client.close();
    afterCrash = undefined;
    return report;
  } finally {
    for (const connection of [first, reopened, afterCrash]) {
      if (connection) {
        connection.worker.terminate();
      }
    }
  }
}

async function createPersistenceTables(database: Client): Promise<void> {
  await database.exec(`
    CREATE TABLE posts (
      id INTEGER PRIMARY KEY,
      title TEXT NOT NULL,
      author TEXT NOT NULL,
      published BOOLEAN NOT NULL,
      body TEXT NOT NULL
    );
    CREATE TABLE empty_table (
      id INTEGER PRIMARY KEY
    );
  `);
}

async function insertPersistencePosts(
  database: Client,
  rows: ReadonlyArray<Post & {body: string}>,
): Promise<void> {
  const columnsPerRow = 5;
  const rowsPerStatement = Math.floor(1_024 / columnsPerRow);
  await database.transaction(async (transaction) => {
    for (let start = 0; start < rows.length; start += rowsPerStatement) {
      const chunk = rows.slice(start, start + rowsPerStatement);
      const params = chunk.flatMap((row) => [
        row.id,
        row.title,
        row.author,
        row.published,
        row.body,
      ]);
      const values = chunk
        .map((_, index) => {
          const first = index * columnsPerRow + 1;
          return `($${first}, $${first + 1}, $${first + 2}, $${first + 3}, $${first + 4})`;
        })
        .join(', ');
      await transaction.query(
        `INSERT INTO posts (id, title, author, published, body) VALUES ${values}`,
        params,
      );
    }
  });
}

function openOpfsClient(databaseName: string) {
  const worker = new Worker(
    new URL('../../../dist/worker/default-entry.js', import.meta.url),
    {name: `tinyjoin-${databaseName}`, type: 'module'},
  );
  return {
    client: new Client({
      worker,
      dataDir: `opfs://${databaseName}`,
    }),
    worker,
  };
}

async function reopenAfterTermination(databaseName: string) {
  const deadline = performance.now() + 3_000;
  let lastError: unknown;
  while (performance.now() < deadline) {
    const connection = openOpfsClient(databaseName);
    try {
      await connection.client.waitReady;
      return connection;
    } catch (error) {
      connection.worker.terminate();
      if (errorCode(error) !== 'STORAGE_LOCKED') {
        throw error;
      }
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  }
  throw lastError ?? new Error('Timed out reopening the terminated OPFS worker');
}

function errorCode(error: unknown): string {
  return typeof error === 'object' &&
    error !== null &&
    'code' in error &&
    typeof error.code === 'string'
    ? error.code
    : '';
}

let browserRestartConnection:
  | ReturnType<typeof openOpfsClient>
  | undefined;

async function writeBrowserRestartFixture(
  databaseName: string,
  rowCount: number,
): Promise<{revision: number}> {
  if (!Number.isSafeInteger(rowCount) || rowCount < 1 || rowCount > 10_000) {
    throw new TypeError('Browser restart row count must be between 1 and 10,000');
  }
  browserRestartConnection = openOpfsClient(databaseName);
  await browserRestartConnection.client.waitReady;
  await createPersistenceTables(browserRestartConnection.client);
  await insertPersistencePosts(
    browserRestartConnection.client,
    Array.from({length: rowCount}, (_, id) => ({
      id,
      title: `Browser restart post ${id}`,
      author: `Author ${id % 17}`,
      published: id % 2 === 0,
      body: `Browser restart fixture ${id}`,
    })),
  );
  // Deliberately leave the Worker and sync handles open. The browser process
  // shutdown is responsible for releasing them before the next launch.
  return {revision: browserRestartConnection.client.getRevision()};
}

async function readBrowserRestartFixture(databaseName: string): Promise<{
  emptyTableRows: number;
  revision: number;
  rowCount: number;
}> {
  const connection = openOpfsClient(databaseName);
  try {
    await connection.client.waitReady;
    const posts = await connection.client.query('SELECT * FROM posts');
    const empty = await connection.client.query('SELECT * FROM empty_table');
    return {
      emptyTableRows: empty.rows.length,
      revision: posts.revision,
      rowCount: posts.rows.length,
    };
  } finally {
    await connection.client.close();
  }
}
