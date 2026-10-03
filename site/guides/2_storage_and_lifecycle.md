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
[release notes](/guides/releases/) before upgrading TinyJoin: each release
states whether it can open the page format written by earlier ones. The only
such break so far is that v0.1.0 and later cannot open a database written by
v0.0.5. Preserve any needed data with the old version before moving to a new
namespace.

The current persistent database is bounded to 65,536 4 KiB pages (256 MiB).
The [SQL compatibility guide](/guides/sql-compatibility/#hard-limits) lists the
independent query and mutation limits.

## Browser retention

OPFS is browser-managed storage. A user can clear it and a browser may evict
best-effort storage under pressure. An application that needs stronger local
retention can make a user-appropriate `navigator.storage.persist()` request.
TinyJoin does not make that product decision during startup.

## Reading the schema

getSchema() returns the database's tables as typed objects, with their columns,
primary keys, and indexes, rather than through SQL catalog tables, which
TinyJoin does not have:

```ts
const {version, tables} = await db.getSchema();
const tasks = tables.find(({name}) => name === 'tasks');
// -> {
//   name: 'tasks',
//   columns: [
//     {name: 'id', type: 'text', nullable: false},
//     {name: 'done', type: 'boolean', nullable: false, default: false},
//   ],
//   primaryKey: ['id'],
//   indexes: [{name: 'tasks_done', columns: ['done'], unique: false}],
// }
```

Tables, and each table's indexes, are in name order. Columns are in the order
the table declares them, followed by any that `ALTER TABLE` added. A column's
`type` is one of the five [runtime types](/guides/sql-compatibility/#runtime-types),
not the spelling it was declared with, so `VARCHAR` reads back as `text` and
`BIGINT` as `integer`; a `VARCHAR(n)` column's `maxLength` is its `n`. `default` is present only for a column that declares
one, and is `null` for `DEFAULT NULL`. The primary key is not listed among the
indexes, and its columns are never nullable. `version` is the version the
application last gave the schema with setSchema(), or zero.

Use it to confirm that a database matches the schema the application expects,
to decide which migration steps an older database still needs, or to generate
code from the tables. It reads only the catalog, not rows, so it is cheap even
for a large database. Call it outside a transaction.

## Setting the schema

setSchema() is the other way round: it takes a schema in the shape getSchema()
returns, and makes the database's tables match it. An application can declare
its schema in code and call setSchema() as it starts, rather than keeping a
series of migrations:

```ts
await db.setSchema({
  version: 2,
  tables: [
    {
      name: 'tasks',
      columns: [
        {name: 'id', type: 'text', nullable: false},
        {name: 'name', renamedFrom: 'title', type: 'text', nullable: false, maxLength: 200},
        {name: 'done', type: 'boolean', nullable: false, default: false},
        {name: 'priority', type: 'integer', nullable: false, default: 0},
      ],
      primaryKey: ['id'],
      indexes: [{name: 'tasks_done', columns: ['done'], unique: false}],
    },
  ],
});
```

It compares the schema with the database's and makes the difference in one
change, which commits whole or not at all, with the statements TinyJoin's
[DDL](/guides/sql-compatibility/#statements-and-clauses) would run:

- a table or column the database lacks is created;
- a table or column whose `renamedFrom` the database still has is renamed,
  keeping its rows and values;
- a column's default, nullability, and `VARCHAR` length become the schema's;
- each table's indexes become exactly the schema's; and
- a table or column the schema leaves out stays, unless setSchema() is given
  `{drop: true}`.

Rows keep their values, so a change that a row prevents fails, changing
nothing: `NOT NULL` where a row holds `NULL`, a shorter `VARCHAR` than a value,
or a `NOT NULL` column without a default added to a table with rows. A change
no statement could make, to a primary key or a column's runtime type, is
refused. Make one by creating a new table under a new name, copying the rows
with the application's own code, and dropping the old table.

A schema the database already has changes nothing and resolves to `false`, so
every tab can call setSchema() as it starts. `version` guards against a tab
that still runs an older copy of the application: setSchema() refuses a schema
whose version is lower than the database's with `SCHEMA_OUTDATED`, and records
a higher one. Give each schema change a higher version, and leave `renamedFrom`
in place for as long as an older database may still have the old name.

The [Drizzle guide](/guides/drizzle/#push) shows how to set a Drizzle schema
this way.

## Application backups and restoration

TinyJoin has no general public export, import, list-databases, or
delete-database API, and no portable binary backup contract. Keep schemas and
migration rules in your application. For
reconstructible data, fetching it again from its original source may be simpler
than a backup.

For data that must be retained, export explicit columns from each known table
to an application-owned format. Include a schema version, preserve primary
keys, and save the result outside this origin's browser storage, for example
as a user-downloaded file. A second OPFS name on the same origin is not an
independent backup. Verify the export by restoring and reading it before
removing the original.

Here is a bounded example for the `notes` schema in the
[getting-started guide](/guides/getting-started/#create-a-table). It exports at
most 1,000 small notes and restores into an empty table. Imported values are
validated before they become SQL parameters; a TypeScript row generic alone
does not validate a file.

```ts
import type {Client} from 'tinyjoin';

type SavedNote = {id: string; body: string; pinned: boolean};

async function backupNotes(db: Client): Promise<string> {
  const {rows} = await db.query<SavedNote>(
    'SELECT id, body, pinned FROM notes ORDER BY id LIMIT 1001',
  );
  if (rows.length > 1000) throw new Error('Export needs a larger-data policy');
  return JSON.stringify({schemaVersion: 1, notes: rows});
}

async function restoreNotes(db: Client, text: string): Promise<void> {
  const backup = JSON.parse(text);
  if (
    backup?.schemaVersion !== 1 || !Array.isArray(backup.notes) ||
    backup.notes.length > 1000 ||
    !backup.notes.every((note: SavedNote) =>
      note !== null && typeof note === 'object' &&
      typeof note.id === 'string' && typeof note.body === 'string' &&
      typeof note.pinned === 'boolean'
    )
  ) throw new Error('Invalid notes backup');

  await db.exec(`
    CREATE TABLE IF NOT EXISTS notes (
      id TEXT PRIMARY KEY,
      body TEXT NOT NULL,
      pinned BOOLEAN NOT NULL DEFAULT false
    )
  `);
  await db.transaction(async (tx) => {
    if ((await tx.query('SELECT id FROM notes LIMIT 1')).rows.length) {
      throw new Error('Restore requires an empty notes table');
    }
    for (const note of backup.notes) {
      await tx.query(
        'INSERT INTO notes (id, body, pinned) VALUES ($1, $2, $3)',
        [note.id, note.body, note.pinned],
      );
    }
  });
}
```

This is an application example, not an arbitrary database importer. Stop
application writes while exporting; for several tables, read them in one short
transaction when the full snapshot fits the limits. Large rows, result bytes,
and staged writes can reach the [engine limits](/guides/sql-compatibility/#hard-limits)
before the example's row cap. There is no streaming backup or cross-batch
snapshot API. Plan bounded batches and a stable maintenance window for larger
datasets. Restore schema DDL before the data transaction, including your known
indexes. If restoring fails, preserve the export and follow the uncertain-write
recovery rules below before retrying.

## Checking a database

Every page carries a checksum, which TinyJoin verifies when it reads the page,
and a statement rejects a row it cannot read. A crash or an interrupted write
therefore surfaces as an error rather than as wrong results, and reopening
returns to the last complete commit. Opening a database checks its catalog but
not every row, so a large database opens as quickly as a small one.

Damage that no statement happens to read, such as a row that is never queried
or an index that no longer matches its table, shows only in a check of the
whole database. check() reads every row and index entry, and every page that
holds them, and rejects with the first problem it finds:

```ts
try {
  await db.check();
} catch (error) {
  // error.code names the first problem, such as STORAGE_CORRUPT.
}
```

Such damage would come from a defect in TinyJoin or from other code rewriting
the database file, not from an interrupted write. A check takes time in
proportion to the database, and statements from every Client of the same OPFS
name wait while it runs, so run it while the application is idle, or in tests.

TinyJoin cannot yet repair a database that fails its check. Restore the
application's own backup, as described above, into a new OPFS name.

## Resetting and removing obsolete databases

A logical reset can use `DELETE FROM notes` to empty a known table, or
`DROP TABLE IF EXISTS notes` followed by the application's schema setup. First
preserve and verify any data that must survive, stop writes, and await close()
on **every Client in every tab or Worker** for this name. Closing only the owner
causes handover to another connected Client. Keep other pages closed, open one
maintenance Client, perform the deliberate reset, and close it before normal
startup resumes. A reset is destructive and is never an uncertain-write
recovery step.

Changing `opfs://my-app-v1` to `opfs://my-app-v2` leaves the first namespace on
disk. Record the names your application creates; TinyJoin does not list or
selectively delete them through its public API. A logical reset also leaves
the namespace allocated. Do not depend on private directory names or remove
individual engine files while a database is open.

To remove obsolete names and their physical storage with current public
facilities, use the browser's **clear site data** control for this origin:

1. Export or otherwise preserve all needed data for every database and other
   application sharing the origin, and verify those backups outside the origin.
2. Stop application work, await all Client closes, and close every page or
   Worker that could reopen these databases.
3. Clear storage for the origin in the browser's site settings. This also
   removes other origin data and offline application caches; it is not a
   TinyJoin-only operation.
4. Reopen the application online, create only its current namespaces, and
   restore the known schemas and retained data before accepting new writes.

If clearing the entire origin is inappropriate, retain the obsolete namespace
until the application has an independently reviewed storage-management policy.
Selective deletion and automatic migration remain separate API decisions.

## Space reuse and quota

`DELETE` and `DROP` free engine pages for internal reuse. The page file normally
keeps its high-water size: fewer rows do not mean a smaller physical file or
immediate browser-quota reclamation. TinyJoin has no public `VACUUM` or
compaction API. Opening a new namespace can temporarily require space for both
databases and does not reclaim the old one.

`navigator.storage.estimate()` can help monitor approximate origin-wide usage
and quota; it is not a per-database size or a reservation. Allow space for
writes and recovery, handle quota failures, and use the deliberate cleanup
procedure above when physical storage must be removed.

## Opening and closing

await create() is the simplest lifecycle: it returns only after
initialization succeeds. The returned Client also exposes `ready`, `waitReady`,
and `closed` for code that constructs a Client directly.

These properties describe initialization and explicit cleanup, not connection
health. `waitReady` resolves once; `ready` can remain `true`, `closed` can remain
`false`, and prepared handles can remain unsealed after a terminal Worker
failure. Operations still reject. Do not poll these properties to decide
whether a failed operation is safe to repeat.

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

### Terminal Worker failures

A Worker crash (`WORKER_ERROR`), unreadable message (`WORKER_MESSAGE_ERROR`), or
invalid protocol response (`PROTOCOL_MISMATCH`) disposes that connection.
Pending operations reject with the failure; subsequent database operations and
prepared executions reject with `WORKER_TERMINATED`. The Client does not
automatically replace a terminally failed Worker. Its statements' `closed`
properties report whether they were sealed, not whether the Worker is healthy.

Stop accepting work, retain the original error, and call close(). Cleanup seals
the prepared handles and completes Client cleanup even if its Promise rejects
because the Worker is already gone. Open a new Client, recreate statements and
subscriptions, and inspect stored operation identifiers before replaying writes:

```ts
// In the application's recovery path, after preserving the original error:
await db.close().catch((cleanupError) => console.error(cleanupError));
const reopened = await create('opfs://my-app'); // Use the original name.
// Reconcile the original operation using reopened before accepting more work.
```

A crash during a write can lose its acknowledgement after committing. Follow
[uncertain-write reconciliation](#recovering-after-an-uncertain-write), even
when the error is a Worker failure instead of a storage code. Memory data is
lost with its Worker.

Temporary OPFS owner handover is different: `LEADER_CHANGED` rejects in-flight
operations, but the same Client reconnects and restores prepared statements for
subsequent work. Reconcile interrupted writes; an interrupted transaction ends
with `TRANSACTION_LOST`. The next section describes this recoverable lifecycle.

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
apply an operation twice. TinyJoin has no automatic replay, so this policy
belongs to the application. Recording the identifier with
`ON CONFLICT DO NOTHING` skips an operation that is already recorded, but it
does not compare the recorded values with the intended ones.

The `retryable` property only says that a later attempt or reopen may succeed.
It is not a guarantee that replaying a write is safe, nor that the current
Client remains usable. A rollback attempted after an uncertain commit cannot
establish that the commit was absent. See also
[transaction error handling](/guides/transactions-and-changes/#errors-and-cancellation).
