import {
  createClient,
  type ChangeBatch,
  type ClientOptions,
} from 'tinygres';

type Post = {
  id: number;
  title: string;
};

const body = document.body;
const resultElement = requiredElement<HTMLOutputElement>('#result');
const mode = new URLSearchParams(location.search).get('worker');
const options: ClientOptions =
  mode === 'app-local'
    ? {
        worker: new Worker(new URL('./tinygres.worker.mts', import.meta.url), {
          name: 'tinygres-packed-consumer',
          type: 'module',
        }),
      }
    : {};

run(options, mode ?? 'default').catch((error: unknown) => {
  body.dataset.status = 'failed';
  body.dataset.worker = mode ?? 'default';
  resultElement.textContent =
    error instanceof Error ? `${error.name}: ${error.message}` : String(error);
});

async function run(
  clientOptions: ClientOptions,
  workerMode: string,
): Promise<void> {
  const database = createClient({
    ...clientOptions,
    schemas: [{name: 'posts', primaryKey: ['id']}],
  });

  try {
    await database.ready();
    await database.replaceTable(
      {name: 'posts', primaryKey: ['id']},
      [{id: 1, title: 'from packed snapshot'}],
    );
    const before = await database.query<Post>(
      'SELECT id, title FROM posts WHERE id = $1',
      [1],
    );

    const invalidated = new Promise<{revision: number; title: string}>(
      (resolve, reject) => {
        const timeout = window.setTimeout(
          () => reject(new Error('Timed out waiting for table invalidation')),
          5_000,
        );
        const unsubscribe = database.subscribe(
          {tables: ['posts']},
          (event) => {
            void database.query<Post>('SELECT id, title FROM posts').then(
              (after) => {
                window.clearTimeout(timeout);
                unsubscribe();
                const changed = after.rows[0];
                if (!changed) {
                  reject(new Error('Changed row was not returned'));
                  return;
                }
                resolve({revision: event.revision, title: changed.title});
              },
              reject,
            );
          },
        );
      },
    );

    const batch = {
      sourceId: 'packed-vite-consumer',
      changes: [
        {
          type: 'upsert',
          table: 'posts',
          row: {id: 1, title: 'from packed change'},
        },
      ],
    } satisfies ChangeBatch;
    await database.applyBatch(batch);
    const after = await invalidated;

    const payload = {
      worker: workerMode,
      initialRevision: before.revision,
      initialTitle: before.rows[0]?.title,
      changedRevision: after.revision,
      changedTitle: after.title,
    };
    resultElement.textContent = JSON.stringify(payload);
    body.dataset.worker = workerMode;
    body.dataset.status = 'passed';
  } finally {
    await database.close();
  }
}

function requiredElement<ElementType extends Element>(selector: string): ElementType {
  const element = document.querySelector<ElementType>(selector);
  if (!element) {
    throw new Error(`Missing packed-consumer element: ${selector}`);
  }
  return element;
}
