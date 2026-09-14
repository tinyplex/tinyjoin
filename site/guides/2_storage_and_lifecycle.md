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

OPFS persistence requires a secure browser context, a dedicated Worker, Web
Locks, and BroadcastChannel. Call create() with the same name in every tab:
TinyJoin elects one database-owning Worker, routes operations to it, and
broadcasts committed changes to all connected Clients. Multiple Clients in one
page work the same way. No additional configuration is needed.

Different names can open independently, but their rows and subscriptions are
separate. Coordination stays within the same origin and browser storage
partition; it does not connect other browser profiles or devices.

The owner retains an exclusive synchronous OPFS access handle. That remains
the final protection against concurrent mutation, including older TinyJoin
versions or custom code that does not participate in coordination. Such an
owner can still cause `STORAGE_LOCKED`; close the older application before
retrying. Mixed TinyJoin releases fail with `DATABASE_VERSION_MISMATCH`
instead of sharing an incompatible engine.

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

Call close() during application teardown. It seals that Client's prepared
statements, detaches it, and terminates its Worker. Other Clients retain their
data and connection. If the departing Worker owned the database, another
connected Worker automatically opens it. Outstanding cleanup is shared by
repeated close() calls.

Closing is irreversible. A Client, its prepared statements, and its
subscriptions cannot resume after close(). If a `pagehide` handler closes the
database, a page restored from that cache must initialize a new Client and
recreate its statements and subscriptions before accepting work. The
[getting-started example](/guides/getting-started/#close-cleanly) uses a reload
on `pageshow` when `event.persisted` is true as a simple application policy.

A browser does not await asynchronous `pagehide` cleanup, and teardown events
are not guaranteed to run. Await writes while the application is active;
close() during navigation is not a final-save or cancellation guarantee.

## Tab handover and subscriptions

The browser's locks elect the next owner when the previous Worker closes or
dies. New operations wait while that owner opens the database. Prepared
statements are restored automatically when needed. Notifications fan out to
every Client, including the one that wrote the data.

Operations already sent to a departing owner reject with `LEADER_CHANGED`.
Their effects may already have committed; TinyJoin never silently repeats
them. The Client reconnects for subsequent operations, so inspect the stored
outcome before retrying a write. A callback transaction interrupted by owner
loss cannot continue: subsequent transaction operations reject with
`TRANSACTION_LOST`. Start a new transaction after reconciliation.

After handover, or when a page becomes visible or resumes, subscriptions may
receive `{revision, tables: [], reset: true}`. Re-query on this notification
even though the precise changed tables are unknown. Filtered subscriptions
also receive it. A subscription is an invalidation signal, not a durable log
of every commit.

Transactions exclude other Clients for the entire callback. Keep callbacks
short and do not wait on work that needs another Client for the same name.
If a client disappears with a transaction open, the owner rolls it back. A
frozen but still live owner or transaction holder can delay other tabs until
it resumes or closes. TinyJoin does not steal a live storage lock based on a
timer, since doing so could let two engines write concurrently.

For offline reopening of the application itself, use the
[offline build integration](/guides/offline/). OPFS stores the data; a service
worker caches the application and database runtime files.

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
