# Bank transactions

Callback transactions group related parameterized mutations. An error thrown
out of the callback rolls back its staged changes. The demo above moves money
between two accounts; the second button deliberately throws after staging the
debit, and you can watch both balances stay exactly where they were.

First, since the demo runs in a browser, register an import alias for
TinyJoin. The site serves its own copy, because a Worker cannot be constructed
from a cross-origin script:

```html
<script type="importmap">
  {"imports": {"tinyjoin": "/lib/index.js"}}
</script>
```

Two accounts and a running log:

```html
<main>
  <ul id="accounts"></ul>
  <div id="buttons">
    <button id="transfer">Transfer 25</button>
    <button id="overdraw">Transfer 500</button>
  </div>
  <p id="status">Ready.</p>
</main>
```

The table is small, and both accounts start with a balance:

```js
import {create} from 'tinyjoin';

const db = await create('memory://');

await db.exec(`
  CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    balance INTEGER NOT NULL DEFAULT 0
  )
`);

await db.query(
  'INSERT INTO accounts (id, name, balance) VALUES ($1, $2, $3), ($4, $5, $6)',
  ['sender', 'Ada', 100, 'recipient', 'Grace', 100],
);
```

A transfer is two updates that must both happen, or neither. transaction()
stages them and publishes them once, and reads through the transaction object
see the staged rows before the commit. An application would normally check
available funds before writing. To demonstrate rollback of an actual write,
this demo deliberately checks after staging Ada's debit and before crediting
Grace:

```js
class InsufficientFundsError extends Error {}

const transfer = async (amount) => {
  await db.transaction(async (tx) => {
    const {rows} = await tx.query(
      'SELECT id, balance FROM accounts ORDER BY id',
    );
    const balances = Object.fromEntries(
      rows.map(({id, balance}) => [id, balance]),
    );

    await tx.query('UPDATE accounts SET balance = $1 WHERE id = $2', [
      balances.sender - amount,
      'sender',
    ]);

    if (balances.sender < amount) {
      throw new InsufficientFundsError(`Ada cannot afford ${amount}`);
    }

    await tx.query('UPDATE accounts SET balance = $1 WHERE id = $2', [
      balances.recipient + amount,
      'recipient',
    ]);
  });
};
```

Letting the error escape the callback rolls the transaction back. Nothing the
callback had already staged is installed, so the first `UPDATE` never becomes
visible outside the transaction even though it ran. After this expected funds
error, re-query the committed rows to show that both balances are unchanged.
Unexpected errors get their own message:

```js
const status = document.getElementById('status');

const attempt = async (amount) => {
  try {
    await transfer(amount);
    status.textContent = `Transferred ${amount}.`;
    status.className = '';
  } catch (error) {
    if (error instanceof InsufficientFundsError) {
      await render();
      status.textContent = `${error.message}. The staged debit was rolled back; both balances are unchanged.`;
    } else {
      status.textContent = `Transfer failed: ${error.message}`;
    }
    status.className = 'failed';
  }
};

document.getElementById('transfer').onclick = () => attempt(25);
document.getElementById('overdraw').onclick = () => attempt(500);
```

The rest is the same shape as any other TinyJoin app: one query to read, and a
subscription so the view follows the data:

```js
const accounts = document.getElementById('accounts');

const render = async () => {
  const {rows} = await db.query(
    'SELECT name, balance FROM accounts ORDER BY name',
  );
  accounts.replaceChildren(
    ...rows.map(({name, balance}) => {
      const item = document.createElement('li');
      const who = document.createElement('span');
      who.textContent = name;
      const amount = document.createElement('b');
      amount.textContent = balance;
      item.append(who, amount);
      return item;
    }),
  );
};

db.subscribe({tables: ['accounts']}, render);
await render();
```

A little styling, and the demo is complete:

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

#accounts {
  list-style: none;
  margin: 0;
  padding: 0;

  li {
    align-items: center;
    background: #fff;
    border: @border;
    display: flex;
    justify-content: space-between;
    margin-bottom: @spacing;
    padding: @spacing;

    b {
      font-variant-numeric: tabular-nums;
    }
  }
}

#buttons {
  display: flex;
  gap: @spacing;
  margin-top: @spacing * 2;
}

button {
  background: @accentColor;
  border: 1px solid @accentColor;
  color: #fff;
  cursor: pointer;
  font: inherit;
  letter-spacing: inherit;
  padding: @spacing @spacing * 2;
}

#overdraw {
  background: #fff;
  border: @border;
  color: inherit;
}

#status {
  color: #666;

  &.failed {
    color: #a0382e;
  }
}
```
