# Todo starter

A useful first TinyJoin app needs one table, one read query, and small
parameterized mutations.

```ts
import {create} from 'tinyjoin';

type Todo = {
  id: string;
  title: string;
  done: boolean;
};

const db = await create('opfs://tinyjoin-todos-v1');

await db.exec(`
  CREATE TABLE IF NOT EXISTS todos (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  )
`);

const listTodos = async (): Promise<Todo[]> =>
  (await db.query<Todo>('SELECT * FROM todos ORDER BY id')).rows;

const addTodo = async (title: string): Promise<void> => {
  await db.query('INSERT INTO todos (id, title) VALUES ($1, $2)', [
    crypto.randomUUID(),
    title,
  ]);
};

const toggleTodo = async (todo: Todo): Promise<void> => {
  await db.query('UPDATE todos SET done = $1 WHERE id = $2', [
    !todo.done,
    todo.id,
  ]);
};

const unsubscribe = db.subscribe({tables: ['todos']}, async () => {
  render(await listTodos());
});

render(await listTodos());

window.addEventListener(
  'pagehide',
  () => {
    unsubscribe();
    void db.close();
  },
  {once: true},
);
```

The UI can stay equally ordinary: a form calls `addTodo`, each checkbox calls
`toggleTodo`, and `render` replaces the visible list. A reload reopens the same
named database.

There are deliberately no seeded rows or generated SQL identifiers. A browser
UUID avoids sequences, and an empty initial list avoids upsert logic.
