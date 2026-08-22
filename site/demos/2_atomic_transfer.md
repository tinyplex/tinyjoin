# Atomic transfer

Callback transactions group related parameterized mutations.

```ts
await db.transaction(async (tx) => {
  const sender = await tx.query<{balance: number}>(
    'SELECT balance FROM accounts WHERE id = $1',
    ['sender'],
  );
  const recipient = await tx.query<{balance: number}>(
    'SELECT balance FROM accounts WHERE id = $1',
    ['recipient'],
  );
  const senderBalance = sender.rows[0]?.balance;
  const recipientBalance = recipient.rows[0]?.balance;
  if (senderBalance === undefined || senderBalance < 25) {
    throw new Error('Insufficient balance');
  }
  if (recipientBalance === undefined) {
    throw new Error('Missing recipient');
  }

  await tx.query('UPDATE accounts SET balance = $1 WHERE id = $2', [
    senderBalance - 25,
    'sender',
  ]);
  await tx.query('UPDATE accounts SET balance = $1 WHERE id = $2', [
    recipientBalance + 25,
    'recipient',
  ]);
});
```

If any uncaught operation fails, neither update becomes visible. Reads through
the transaction object see its staged rows before the final commit.
