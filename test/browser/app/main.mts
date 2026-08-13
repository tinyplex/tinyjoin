import {
  createClient,
  type ChangeBatch,
  type SyncState,
  type TableSchema,
} from '../../../dist/index.js';

type Post = {
  id: number;
  title: string;
  author: string;
  published: boolean;
};

const postsSchema = {
  name: 'posts',
  primaryKey: ['id'],
} satisfies TableSchema;

const initialPosts: Post[] = [
  {
    id: 1,
    title: 'The worker owns the database',
    author: 'Ada',
    published: true,
  },
  {
    id: 2,
    title: 'Queries stay off the main thread',
    author: 'Linus',
    published: true,
  },
  {
    id: 3,
    title: 'Drafts remain filtered locally',
    author: 'Grace',
    published: false,
  },
];

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
let supabaseProbe:
  | {
      database: ReturnType<typeof createClient>;
      states: SyncState[];
      unsubscribe(): void;
    }
  | undefined;

async function boot(): Promise<void> {
  const database = createClient({schemas: [postsSchema]});
  await database.ready();
  await database.replaceTable(postsSchema, initialPosts);
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
    const batch = {
      sourceId: 'browser-test',
      transactionId: `test-change-${simulatedChanges}`,
      committedAt: new Date().toISOString(),
      changes: [
        {
          type: 'upsert',
          table: 'posts',
          row: {
            id: 2,
            title: `Worker invalidation #${simulatedChanges}`,
            author: 'Linus',
            published: true,
          },
        },
      ],
    } satisfies ChangeBatch;

    try {
      await database.applyBatch(batch);
    } catch (error) {
      showError(error);
    } finally {
      applyButton.disabled = false;
    }
  });

  window.__tinygresTest = {
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
    closeSupabaseProbe,
    joinDatabaseProbe,
    legacyTransactionDdlProbe,
    openSupabaseProbe,
    pageTransactionDdlProbe: () => pageTransactionDdlProbe(database),
    persistenceProbe,
    writableDatabaseProbe,
    readSupabaseProbe,
    readBrowserRestartFixture,
    waitForSupabaseProbe,
    writeBrowserRestartFixture,
  };

  window.addEventListener(
    'pagehide',
    () => {
      unsubscribe();
      delete window.__tinygresTest;
      void closeSupabaseProbe();
      void database.close();
    },
    {once: true},
  );

  stateElement.textContent = 'Ready';
  statusElement.textContent = 'Ready. The initial snapshot is queryable locally.';
  applyButton.disabled = false;
}

async function legacyTransactionDdlProbe(databaseName: string): Promise<{
  reopenedRevision: number;
  reopenedRows: number;
  stagedRows: number;
}> {
  let connection: ReturnType<typeof openOpfsClient> | undefined =
    openOpfsClient(databaseName, []);
  try {
    await connection.client.ready();
    const stagedRows = await connection.client.transaction(
      async (transaction) => {
        await transaction.exec(`
          CREATE TABLE legacy_tx_probe (
            id INTEGER PRIMARY KEY,
            note TEXT NOT NULL
          )
        `);
        await transaction.exec(
          'INSERT INTO legacy_tx_probe (id, note) VALUES ($1, $2)',
          [1, 'DDL and DML committed together'],
        );
        return (
          await transaction.query('SELECT id FROM legacy_tx_probe')
        ).rows.length;
      },
    );
    await connection.client.close();
    connection = openOpfsClient(databaseName, []);
    await connection.client.ready();
    const reopened = await connection.client.query(
      'SELECT id FROM legacy_tx_probe',
    );
    return {
      reopenedRevision: reopened.revision,
      reopenedRows: reopened.rows.length,
      stagedRows,
    };
  } finally {
    connection?.worker.terminate();
  }
}

async function pageTransactionDdlProbe(
  database: ReturnType<typeof createClient>,
): Promise<{
  committedRows: number;
  ddlCodes: string[];
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
      await transaction.exec(
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
    revisionAfter: after.revision,
    revisionBefore: before.revision,
    stagedRows,
  };
}

async function writableDatabaseProbe(databaseName: string): Promise<{
  aggregateRows: Array<{done: boolean; task_count: number}>;
  committedRevision: number;
  insertRows: Array<{done: boolean; id: number; title: string}>;
  invalidations: Array<{revision: number; tables: string[]}>;
  orderedRows: Array<{done: boolean; id: number; title: string}>;
  reopenedPriorities: Array<{id: number; priority: number}>;
  reopenedRevision: number;
  reopenedRows: Array<{done: boolean; id: number; title: string}>;
  rollbackCode: string;
  stagedRows: Array<{done: boolean; id: number; title: string}>;
}> {
  const connection = openOpfsClient(databaseName, []);
  const events: Array<{revision: number; tables: string[]}> = [];
  const unsubscribe = connection.client.subscribe({}, (event) => {
    events.push(structuredClone(event));
  });
  try {
    await connection.client.ready();
    await connection.client.exec(`
      CREATE TABLE tasks (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL,
        done BOOLEAN DEFAULT false,
        metadata JSON
      )
    `);
    await connection.client.exec(
      'CREATE UNIQUE INDEX tasks_title ON tasks (title)',
    );
    await connection.client.exec(
      'ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0',
    );
    const inserted = await connection.client.exec<{
      done: boolean;
      id: number;
      title: string;
    }>(
      'INSERT INTO tasks (id, title, metadata) VALUES ($1, $2, $3), ($4, $5, $6) RETURNING id, title, done',
      [1, 'Ship writable SQL', {owner: 'worker'}, 2, 'Persist it', null],
    );

    const stagedRows = await connection.client.transaction(
      async (transaction) => {
        await transaction.exec(
          'UPDATE tasks SET done = true WHERE id = $1 RETURNING id',
          [1],
        );
        await transaction.exec(
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
        await transaction.exec(
          'UPDATE tasks SET title = $1 WHERE id = $2',
          ['must not persist', 1],
        );
        await transaction.exec(
          'INSERT INTO tasks (id, title) VALUES ($1, $2)',
          [4, 'Persist it'],
        );
      });
    } catch (error) {
      rollbackCode = errorCode(error);
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
    const built = await connection.client
      .from<{done: boolean; id: number; title: string}>('tasks')
      .select('id, title, done')
      .gte('id', 1)
      .order('done', {ascending: false})
      .order('id', {ascending: false})
      .range(1, 2);
    if (built.error || JSON.stringify(built.data) !== JSON.stringify(ordered.rows)) {
      throw built.error ?? new Error('Structured query did not match SQL query');
    }
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

    const reopened = openOpfsClient(databaseName, []);
    try {
      await reopened.client.ready();
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
        committedRevision,
        insertRows: inserted.rows,
        invalidations: events,
        orderedRows: ordered.rows,
        reopenedRevision: result.revision,
        reopenedPriorities: priorities.rows,
        reopenedRows: result.rows.sort((left, right) => left.id - right.id),
        rollbackCode,
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
}> {
  const database = createClient();
  try {
    await database.ready();
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
    await database.exec(
      `INSERT INTO authors (id, name) VALUES
       (1, 'Ada'), (2, 'Linus'), (3, 'Grace')`,
    );
    await database.exec(
      `INSERT INTO articles (id, author_id, title, published) VALUES
       (10, 1, 'Worker databases', true),
       (11, 1, 'A second article', false),
       (12, 2, 'Local queries', true),
       (13, NULL, 'Unassigned draft', true)`,
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
    return {innerRows: inner.rows, leftRows: left.rows};
  } finally {
    await database.close();
  }
}

async function openSupabaseProbe(options: {
  databaseName?: string;
  publishableKey: string;
  url: string;
}): Promise<{
  revision: number;
  rows: Array<{id: number; title: string}>;
  state: SyncState;
  states: SyncState[];
}> {
  await closeSupabaseProbe();
  const database = createClient({
    ...(options.databaseName
      ? {storage: {kind: 'opfs' as const, name: options.databaseName}}
      : {}),
    source: {
      kind: 'supabase',
      url: options.url,
      publishableKey: options.publishableKey,
      tables: [
        {
          table: 'posts',
          primaryKey: ['id'],
          columns: ['id', 'title'],
        },
      ],
    },
  });
  const states: SyncState[] = [];
  const unsubscribe = database.subscribeToSyncState((state) => {
    states.push(state);
  });
  supabaseProbe = {database, states, unsubscribe};
  await database.ready();
  return readSupabaseProbe();
}

async function waitForSupabaseProbe(): Promise<{
  revision: number;
  rows: Array<{id: number; title: string}>;
  state: SyncState;
  states: SyncState[];
}> {
  const probe = requireSupabaseProbe();
  await probe.database.whenSynced({timeoutMs: 15_000});
  return readSupabaseProbe();
}

async function readSupabaseProbe(): Promise<{
  revision: number;
  rows: Array<{id: number; title: string}>;
  state: SyncState;
  states: SyncState[];
}> {
  const probe = requireSupabaseProbe();
  const result = await probe.database.query<{id: number; title: string}>(
    'SELECT id, title FROM posts',
  );
  return {
    revision: result.revision,
    rows: result.rows,
    state: probe.database.getSyncState(),
    states: probe.states.map((state) => structuredClone(state)),
  };
}

async function closeSupabaseProbe(): Promise<void> {
  const probe = supabaseProbe;
  supabaseProbe = undefined;
  if (!probe) {
    return;
  }
  probe.unsubscribe();
  await probe.database.close();
}

function requireSupabaseProbe(): NonNullable<typeof supabaseProbe> {
  if (!supabaseProbe) {
    throw new Error('The Supabase browser probe is not open');
  }
  return supabaseProbe;
}

async function renderQuery(
  database: ReturnType<typeof createClient>,
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
  conflictErrorCode: string;
  differentNameOpened: boolean;
  emptyTableRows: number;
  gracefulReopenMs: number;
  initialCommitMs: number;
  lockErrorCode: string;
  mutationCommitMs: number;
  journalWriteBytes: number;
  revision: number;
  rowCount: number;
  updatedTitle: string;
}> {
  if (!Number.isSafeInteger(rowCount) || rowCount < 1 || rowCount > 10_000) {
    throw new TypeError('Persistence row count must be between 1 and 10,000');
  }
  const emptySchema = {
    name: 'empty_table',
    primaryKey: ['id'],
  } satisfies TableSchema;
  const rows = Array.from({length: rowCount}, (_, id) => ({
    id,
    title: `Persisted post ${id}`,
    author: `Author ${id % 17}`,
    published: id % 2 === 0,
    body: `Bounded persistence fixture ${id} ${'x'.repeat(96)}`,
  }));

  let first: ReturnType<typeof openOpfsClient> | undefined = openOpfsClient(
    databaseName,
    [postsSchema, emptySchema],
  );
  let reopened: ReturnType<typeof openOpfsClient> | undefined;
  let afterCrash: ReturnType<typeof openOpfsClient> | undefined;
  let conflicting: ReturnType<typeof openOpfsClient> | undefined;
  try {
    await first.client.ready();
    const commitStartedAt = performance.now();
    await first.client.replaceTable(postsSchema, rows);
    const initialCommitMs = performance.now() - commitStartedAt;

    const competing = openOpfsClient(databaseName, []);
    let lockErrorCode = '';
    try {
      await competing.client.ready();
    } catch (error) {
      lockErrorCode = errorCode(error);
    } finally {
      competing.worker.terminate();
    }

    const independent = openOpfsClient(`${databaseName}-independent`, []);
    await independent.client.ready();
    await independent.client.close();

    await first.client.close();
    first = undefined;

    conflicting = openOpfsClient(databaseName, [
      {name: 'posts', primaryKey: ['slug']},
    ]);
    let conflictErrorCode = '';
    try {
      await conflicting.client.ready();
    } catch (error) {
      conflictErrorCode = errorCode(error);
    }

    const reopenStartedAt = performance.now();
    reopened = openOpfsClient(databaseName, []);
    await reopened.client.ready();
    conflicting.worker.terminate();
    conflicting = undefined;
    const gracefulReopenMs = performance.now() - reopenStartedAt;
    const restored = await reopened.client.query<{
      id: number;
      title: string;
    }>('SELECT id, title FROM posts');
    const empty = await reopened.client.query('SELECT * FROM empty_table');

    const journalBytesBefore = await opfsJournalBytes(databaseName);
    const mutationStartedAt = performance.now();
    await reopened.client.applyBatch({
      changes: [
        {
          type: 'upsert',
          table: 'posts',
          row: {...rows[0]!, title: 'Persisted after forced termination'},
        },
      ],
    });
    const mutationCommitMs = performance.now() - mutationStartedAt;
    const journalWriteBytes =
      (await opfsJournalBytes(databaseName)) - journalBytesBefore;
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
      conflictErrorCode,
      differentNameOpened: true,
      emptyTableRows: empty.rows.length,
      gracefulReopenMs,
      initialCommitMs,
      lockErrorCode,
      mutationCommitMs,
      journalWriteBytes,
      revision: afterCrashResult.revision,
      rowCount: restored.rows.length,
      updatedTitle: afterCrashResult.rows[0]?.title ?? '',
    };
    await afterCrash.client.close();
    afterCrash = undefined;
    return report;
  } finally {
    for (const connection of [first, reopened, afterCrash, conflicting]) {
      if (connection) {
        connection.worker.terminate();
      }
    }
  }
}

async function opfsFileSize(databaseName: string, fileName: string): Promise<number> {
  const root = await navigator.storage.getDirectory();
  const tinygres = await root.getDirectoryHandle('tinygres-v1');
  const database = await tinygres.getDirectoryHandle(`db-${databaseName}`);
  const file = await database.getFileHandle(fileName);
  return (await file.getFile()).size;
}

async function opfsJournalBytes(databaseName: string): Promise<number> {
  return (
    (await opfsFileSize(databaseName, 'journal-a.bin')) +
    (await opfsFileSize(databaseName, 'journal-b.bin'))
  );
}

function openOpfsClient(databaseName: string, schemas: TableSchema[]) {
  const worker = new Worker(
    new URL('../../../dist/worker/default-entry.js', import.meta.url),
    {name: `tinygres-${databaseName}`, type: 'module'},
  );
  return {
    client: createClient({
      worker,
      schemas,
      storage: {kind: 'opfs', name: databaseName},
    }),
    worker,
  };
}

async function reopenAfterTermination(databaseName: string) {
  const deadline = performance.now() + 3_000;
  let lastError: unknown;
  while (performance.now() < deadline) {
    const connection = openOpfsClient(databaseName, []);
    try {
      await connection.client.ready();
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
  const emptySchema = {
    name: 'empty_table',
    primaryKey: ['id'],
  } satisfies TableSchema;
  browserRestartConnection = openOpfsClient(databaseName, [
    postsSchema,
    emptySchema,
  ]);
  await browserRestartConnection.client.ready();
  const outcome = await browserRestartConnection.client.replaceTable(
    postsSchema,
    Array.from({length: rowCount}, (_, id) => ({
      id,
      title: `Browser restart post ${id}`,
      author: `Author ${id % 17}`,
      published: id % 2 === 0,
    })),
  );
  // Deliberately leave the Worker and sync handles open. The browser process
  // shutdown is responsible for releasing them before the next launch.
  return {revision: outcome.revision};
}

async function readBrowserRestartFixture(databaseName: string): Promise<{
  emptyTableRows: number;
  revision: number;
  rowCount: number;
}> {
  const connection = openOpfsClient(databaseName, []);
  try {
    await connection.client.ready();
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
