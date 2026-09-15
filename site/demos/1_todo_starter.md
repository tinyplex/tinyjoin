# Todo starter

A useful first TinyJoin app needs one table, one read query, and small
parameterized mutations. The demo above is running on this page, in an iframe,
against a real in-memory database.

First, since the demo runs in a browser, register an import alias for
TinyJoin. The site serves its own copy, because a Worker cannot be constructed
from a cross-origin script:

```html
<script type="importmap">
  {"imports": {"tinyjoin": "/lib/index.js"}}
</script>
```

Start with the markup the app will fill in:

```html
<main>
  <form id="add">
    <input id="text" placeholder="What needs to be done?" autocomplete="off" />
    <button type="submit">Add</button>
  </form>
  <ul id="todos"></ul>
</main>
```

Opening the database is one call. This demo is intentionally memory-only:
`memory://` starts a fresh database on every reload, including the two sample
todos added below. For an app that keeps todos between visits, use the
[persistent Vite starter](/guides/getting-started/) with
`npm create tinyjoin@latest`. Its schema setup and sample data are designed for
reopening an existing database.

```js
import {create} from 'tinyjoin';

const db = await create('memory://');
```

One table is enough, and creating it is ordinary SQL:

```js
await db.exec(`
  CREATE TABLE todos (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT false
  )
`);
```

Every write is parameterized, so application values never reach the SQL text.
Row identifiers come from the browser, which avoids needing a sequence:

```js
const addTodo = (text) =>
  db.query('INSERT INTO todos (id, text) VALUES ($1, $2)', [
    crypto.randomUUID(),
    text,
  ]);

const setDone = (id, done) =>
  db.query('UPDATE todos SET done = $1 WHERE id = $2', [done, id]);

const deleteTodo = (id) => db.query('DELETE FROM todos WHERE id = $1', [id]);
```

Reading is one ordinary `SELECT`, and the result has the familiar `rows` shape:

```js
const listTodos = async () =>
  (await db.query('SELECT id, text, done FROM todos ORDER BY text, id')).rows;
```

The UI can stay equally ordinary. `render` replaces the visible list, each
checkbox calls `setDone`, and each button calls `deleteTodo`:

```js
const list = document.getElementById('todos');

const render = async () => {
  const todos = await listTodos();
  list.replaceChildren(
    ...todos.map(({id, text, done}) => {
      const item = document.createElement('li');
      item.className = done ? 'done' : '';

      const checkbox = document.createElement('input');
      checkbox.type = 'checkbox';
      checkbox.checked = done;
      checkbox.onchange = () => setDone(id, checkbox.checked);

      const label = document.createElement('span');
      label.textContent = text;

      const remove = document.createElement('button');
      remove.textContent = 'Delete';
      remove.onclick = () => deleteTodo(id);

      item.append(checkbox, label, remove);
      return item;
    }),
  );
};
```

Rather than re-rendering by hand after every mutation, subscribe() reports which
tables changed so the app can simply re-query:

```js
db.subscribe({tables: ['todos']}, render);
```

The form only has to write. The subscription takes care of the rest:

```js
document.getElementById('add').onsubmit = async (event) => {
  event.preventDefault();
  const input = document.getElementById('text');
  const text = input.value.trim();
  if (text !== '') {
    input.value = '';
    await addTodo(text);
  }
  input.focus();
};
```

Two rows to start with, and the app is complete:

```js
await addTodo('Learn TinyJoin');
await addTodo('Build an app');
```

Add a little styling, and we're done:

```less
@font-face {
  font-family: Inter;
  src: url(/fonts/inter.woff2) format('woff2');
  font-display: swap;
}

@accentColor: #7c3aed;
@spacing: 0.5rem;
@border: 1px solid #ccc;

body {
  box-sizing: border-box;
  font-family: Inter, sans-serif;
  letter-spacing: -0.04rem;
  margin: 0;
  min-height: 100vh;
  padding: @spacing * 2;

  * {
    box-sizing: border-box;
    outline-color: @accentColor;
  }
}

#add {
  display: flex;
  gap: @spacing;
}

#text {
  border: @border;
  flex: 1;
  font: inherit;
  letter-spacing: inherit;
  min-width: 0;
  padding: @spacing;
}

button {
  background: #fff;
  border: @border;
  cursor: pointer;
  font: inherit;
  letter-spacing: inherit;
  padding: @spacing;
}

#add button {
  background: @accentColor;
  border-color: @accentColor;
  color: #fff;
  padding: @spacing @spacing * 2;
}

#todos {
  list-style: none;
  margin: @spacing * 2 0 0;
  padding: 0;

  li {
    align-items: center;
    background: #fff;
    border: @border;
    display: flex;
    gap: @spacing;
    margin-bottom: @spacing;
    padding: @spacing;

    span {
      flex: 1;
      overflow: hidden;
      text-overflow: ellipsis;
      user-select: none;
      white-space: nowrap;
    }

    input[type='checkbox'] {
      accent-color: @accentColor;
      height: 1.1rem;
      width: 1.1rem;
    }

    &.done span {
      color: #ccc;
      text-decoration: line-through;
    }
  }
}
```
