# Transactions and changes

Each TinyJoin query is atomic. Use a callback transaction when several related
row mutations must commit together.

```ts
await db.transaction(async (tx) => {
  await tx.query('UPDATE accounts SET balance = $1 WHERE id = $2', [40, 'a']);
  await tx.query('UPDATE accounts SET balance = $1 WHERE id = $2', [60, 'b']);
});
```

Reads inside the callback see staged rows. Use the transaction object for all
database work until its callback finishes; direct Client operations fail while
it is active, and a later transaction() call waits its turn. Letting an error
escape before commit discards the staged transaction.

TinyJoin does not put a callback transaction into PostgreSQL's aborted state
after a statement failure. If application code catches that failure, earlier
staged writes may still commit. Call tx.rollback() or rethrow when the whole
unit should be discarded.

Run schema DDL such as `CREATE`, `ALTER`, and `DROP` outside the callback, using
a standalone query() or an atomic exec() script.

## Composing transaction helpers

Nested callback transactions are not supported. In particular, do not await
db.transaction() from inside that same Client's transaction callback: the
inner call queues behind the outer call, while the outer callback waits for
the inner call. Neither can finish, and there is no deadlock detector.

Pass the active Transaction to helpers instead of having each helper open a
transaction:

```ts
import type {Transaction} from 'tinyjoin';

const setBalance = async (tx: Transaction, id: string, balance: number) => {
  await tx.query('UPDATE accounts SET balance = $1 WHERE id = $2', [balance, id]);
};

await db.transaction(async (tx) => {
  await setBalance(tx, 'a', 40);
  await setBalance(tx, 'b', 60);
});
```

Use tx.query(), tx.exec(), or tx.execute() throughout the callback. Prepare
handles before entering it. tx.exec() gives a read/DML script its own atomic
failure boundary within the active transaction; it does not open a nested
transaction or expose general-purpose savepoints.

## Errors and cancellation

An ordinary statement failure changes none of that statement's staged rows.
Rethrow it or call tx.rollback() to discard earlier staged work too. A failure
while publishing a commit has a different boundary: `RECOVERY_REQUIRED`,
`STORAGE_COMMIT_OUTCOME_UNKNOWN`, or `STORAGE_ENGINE_POISONED` requires closing
the Client, reopening the same OPFS name, and reconciling the operation before
replay. Rejection of transaction() alone is not proof that a commit did not
happen. Follow the [recovery procedure](/guides/storage-and-lifecycle/#recovering-after-an-uncertain-write).

There is no AbortSignal, query timeout, or transaction timeout option.
Promise.race() with a timer only stops the caller waiting; the callback and
queued database work can continue and may commit. It is not cancellation or
evidence of rollback. Avoid waiting for network requests, user input, or a
nested transaction inside a callback. Keep the callback bounded, and request
an explicit tx.rollback() while it is active when application logic decides
to abandon staged work. Closing a Client is teardown, not a safe way to infer
the outcome of an in-flight write.

## Inserting many rows

Prepare a parameterized `INSERT` once and execute it through the transaction
object. Transactions that only append new primary keys validate each new
statement incrementally, including unique-index constraints. Final commit
still validates and publishes the complete write set.

```ts
const insert = await db.prepare('INSERT INTO tasks (id, title) VALUES ($1, $2)');
try {
  await db.transaction(async (tx) => {
    for (const title of titles) {
      await tx.execute(insert, [crypto.randomUUID(), title]);
    }
  });
} finally {
  await insert.close();
}
```

Updating, deleting, or revisiting a staged key switches that transaction to
complete write-set validation after each statement. Many individual writes
on that path can have quadratic staging cost. Repeated tx.exec() calls also
copy the current transaction state for script rollback. Keep transactions
bounded and prefer multi-row statements when the application can form them
within the [SQL limits](/guides/sql-compatibility/#hard-limits).

These limits apply cumulatively across the transaction, even when each
individual statement is small. Transaction reads use the staged row view;
secondary-index query acceleration is currently disabled inside callbacks.

## Re-query after a commit

Subscriptions report changed table names rather than maintaining a live result
object:

```ts
const unsubscribe = db.subscribe({tables: ['tasks']}, async () => {
  const {rows} = await db.query('SELECT * FROM tasks ORDER BY id');
  render(rows);
});
```

The listener runs after a commit. Several statements in one transaction produce
one committed revision and one table-level invalidation. Call the returned
function to unsubscribe before closing the database.

This explicit re-query model keeps TinyJoin independent of UI frameworks and
lets an application choose its own caching or rendering policy.
