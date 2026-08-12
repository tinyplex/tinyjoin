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
    openSupabaseProbe,
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

async function writableDatabaseProbe(databaseName: string): Promise<{
  committedRevision: number;
  insertRows: Array<{done: boolean; id: number; title: string}>;
  invalidations: Array<{revision: number; tables: string[]}>;
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
          [1, 'duplicate'],
        );
      });
    } catch (error) {
      rollbackCode = errorCode(error);
    }

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
      return {
        committedRevision,
        insertRows: inserted.rows,
        invalidations: events,
        reopenedRevision: result.revision,
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
