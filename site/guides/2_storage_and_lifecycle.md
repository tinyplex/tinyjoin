# Storage and lifecycle

TinyJoin supports an ephemeral memory database and an opt-in persistent browser
database. Both use the same Worker and page-native engine.

## Memory

```ts
const db = await create();
// Equivalent to: await create('memory://')
```

Memory starts empty each time the Worker opens. It is useful for tests,
temporary views, and trying the API.

## Persistent browser storage

```ts
const db = await create('opfs://my-project-v1');
```

The name must contain 1-64 ASCII letters, numbers, dots, underscores, or
hyphens, and start with a letter or number. Include the application, dataset,
and schema generation in it. A name is a namespace, not an encryption or
access-control boundary.

OPFS persistence requires a secure browser context and a dedicated Worker.
TinyJoin opens the database with an exclusive synchronous access handle. A
second Worker opening the same name fails rather than risking concurrent
mutation; closing the first Client releases the lock.

This applies to two Clients in one page as well as Clients in separate tabs of
the same origin. Different names can be open independently, but their rows and
subscriptions are separate: changing the name does not share or synchronize
the original database. Handle `STORAGE_LOCKED` during startup with an
already-open message and an explicit retry after the other Client closes.

There is no silent fallback to memory if persistent storage is unavailable,
locked, corrupt, or out of quota.

An OPFS name identifies stored data; changing the name opens a different
database and leaves the old data in place. Check the
[release compatibility boundary](/guides/releases/#v0-0-6) before upgrading
TinyJoin: v0.0.6 cannot open the page format published in v0.0.5. Preserve any
needed data with the old version before moving to a new namespace.

The current persistent database is bounded to 65,536 4 KiB pages (256 MiB).
The [SQL compatibility guide](/guides/sql-compatibility/#hard-limits) lists the
independent query and mutation limits.

## Browser retention

OPFS is browser-managed storage. A user can clear it and a browser may evict
best-effort storage under pressure. An application that needs stronger local
retention can make a user-appropriate `navigator.storage.persist()` request.
TinyJoin does not make that product decision during startup.

## Opening and closing

await create() is the simplest lifecycle: it returns only after
initialization succeeds. The returned Client also exposes `ready`, `waitReady`,
and `closed` for code that constructs a Client directly.

Call close() during application teardown. It seals prepared statements,
closes storage, and terminates TinyJoin's packaged Worker. Outstanding cleanup
is shared by repeated calls.

Closing is irreversible. A Client, its prepared statements, and its
subscriptions cannot resume after close(). TinyJoin does not manage the
browser's back/forward cache lifecycle. If a `pagehide` handler closes the
database, a page restored from that cache must initialize a new Client and
recreate its statements and subscriptions before accepting work. The
[getting-started example](/guides/getting-started/#close-cleanly) uses a reload
on `pageshow` when `event.persisted` is true as a simple application policy.

A browser does not await asynchronous `pagehide` cleanup, and teardown events
are not guaranteed to run. Await writes while the application is active;
close() during navigation is not a final-save or cancellation guarantee.

## Recovering after an uncertain write

A rejected write does not always prove that nothing committed. The following
ClientError codes mean the current engine must no longer be used:

| Code | Meaning |
| --- | --- |
| `RECOVERY_REQUIRED` | The engine cannot safely continue after a storage publication failure; reopening must establish the stored state. |
| `STORAGE_COMMIT_OUTCOME_UNKNOWN` | A storage write or a result after a possible commit could not be confirmed. The requested change may have committed. |
| `STORAGE_ENGINE_POISONED` | A previous uncertain or fatal result already made this engine unusable. |

Stop accepting database work, keep the original error, and close the Client.
For OPFS, open the **same database name** with a new create() call and inspect
the recovered rows before deciding whether to repeat the operation. If open
or inspection fails, keep the application in a recovery state and preserve the
stored data; changing the name or deleting the database would hide the state
you need to reconcile. A new memory database starts empty and cannot recover
the old Client's data.

Give an application operation a stable identifier before its first attempt
and record that identifier in the same transaction as its effects. After
reopening, query that record and compare the intended values. A matching
record means the operation already happened; a missing record may allow a
deliberate retry with the **same identifier**. An unexpected record needs
application reconciliation. Generating a fresh identifier on every retry can
apply an operation twice. TinyJoin has no automatic replay or `ON CONFLICT`
clause, so this policy belongs to the application.

The `retryable` property only says that a later attempt or reopen may succeed.
It is not a guarantee that replaying a write is safe, nor that the current
Client remains usable. A rollback attempted after an uncertain commit cannot
establish that the commit was absent. See also
[transaction error handling](/guides/transactions-and-changes/#errors-and-cancellation).
