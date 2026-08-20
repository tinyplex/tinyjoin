import {create, type ClientOptions} from 'tinygres';

type Post = {
  id: number;
  title: string;
};

const body = document.body;
const resultElement = requiredElement<HTMLOutputElement>('#result');
const search = new URLSearchParams(location.search);
const mode = search.get('worker');
const persistence = search.get('persistence');
const databaseName = search.get('database');
const options: ClientOptions =
  mode === 'app-local'
    ? {
        worker: new Worker(new URL('./tinygres.worker.mts', import.meta.url), {
          name: 'tinygres-packed-consumer',
          type: 'module',
        }),
      }
    : {};

if (persistence && databaseName) {
  options.storage = {kind: 'opfs', name: databaseName};
}

run(options, mode ?? 'default', persistence).catch((error: unknown) => {
  body.dataset.status = 'failed';
  body.dataset.worker = mode ?? 'default';
  resultElement.textContent =
    error instanceof Error ? `${error.name}: ${error.message}` : String(error);
});

async function run(
  clientOptions: ClientOptions,
  workerMode: string,
  persistence: string | null,
): Promise<void> {
  const database = create(clientOptions);
  let succeeded = false;

  try {
    await database.ready();
    if (persistence === 'read') {
      const restored = await database.query<Post>(
        'SELECT id, title FROM posts WHERE id = $1',
        [1],
      );
      resultElement.textContent = JSON.stringify({
        worker: workerMode,
        revision: restored.revision,
        title: restored.rows[0]?.title,
      });
      body.dataset.worker = workerMode;
      succeeded = true;
      return;
    }

    await database.exec(`
      CREATE TABLE posts (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL
      )
    `);
    await database.exec('INSERT INTO posts (id, title) VALUES ($1, $2)', [
      1,
      'from packed insert',
    ]);
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
        const unsubscribe = database.subscribe({tables: ['posts']}, (event) => {
          void database
            .query<Post>('SELECT id, title FROM posts')
            .then((after) => {
              window.clearTimeout(timeout);
              unsubscribe();
              const changed = after.rows[0];
              if (!changed) {
                reject(new Error('Changed row was not returned'));
                return;
              }
              resolve({revision: event.revision, title: changed.title});
            }, reject);
        });
      },
    );

    await database.exec('UPDATE posts SET title = $1 WHERE id = $2', [
      'from packed update',
      1,
    ]);
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
    succeeded = true;
  } finally {
    await database.close();
    if (succeeded) {
      body.dataset.closed = 'true';
      body.dataset.status = 'passed';
    }
  }
}

function requiredElement<ElementType extends Element>(
  selector: string,
): ElementType {
  const element = document.querySelector<ElementType>(selector);
  if (!element) {
    throw new Error(`Missing packed-consumer element: ${selector}`);
  }
  return element;
}
