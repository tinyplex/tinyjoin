# Transactions and changes

Each TinyGres query is atomic. Use a callback transaction when several related
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
escape rolls the complete transaction back.

TinyGres does not put a callback transaction into PostgreSQL's aborted state
after a statement failure. If application code catches that failure, earlier
staged writes may still commit. Call tx.rollback() or rethrow when the whole
unit should be discarded.

Run schema DDL such as `CREATE`, `ALTER`, and `DROP` outside the callback, using
a standalone query() or an atomic exec() script.

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

This explicit re-query model keeps TinyGres independent of UI frameworks and
lets an application choose its own caching or rendering policy.
