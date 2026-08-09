import './style.css';

import {
  createTinygresClient,
  type ChangeBatch,
  type TableSchema,
} from '../src/index.ts';

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

async function boot(): Promise<void> {
  const database = createTinygresClient({schemas: [postsSchema]});
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
    statusElement.textContent = 'Applying a normalized fake server batch…';
    const batch = {
      sourceId: 'phase-1-demo',
      transactionId: `fake-server-change-${simulatedChanges}`,
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

  window.__tinygresDemo = {
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
  };

  window.addEventListener(
    'pagehide',
    () => {
      unsubscribe();
      delete window.__tinygresDemo;
      void database.close();
    },
    {once: true},
  );

  stateElement.textContent = 'Ready';
  stateElement.dataset.ready = 'true';
  statusElement.textContent = 'Ready. The initial snapshot is queryable locally.';
  applyButton.disabled = false;
}

async function renderQuery(
  database: ReturnType<typeof createTinygresClient>,
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

      const title = document.createElement('strong');
      title.textContent = post.title;
      const author = document.createElement('span');
      author.textContent = post.author;

      item.append(title, author);
      return item;
    }),
  );
}

function showError(error: unknown): void {
  const message = error instanceof Error ? error.message : String(error);
  stateElement.textContent = 'Failed';
  stateElement.dataset.ready = 'false';
  statusElement.textContent = 'The Phase-1 flow failed.';
  errorElement.hidden = false;
  errorElement.textContent = message;
  applyButton.disabled = true;
}

function element<ElementType extends Element>(selector: string): ElementType {
  const match = document.querySelector<ElementType>(selector);
  if (!match) {
    throw new Error(`Demo element not found: ${selector}`);
  }
  return match;
}

void boot().catch(showError);
