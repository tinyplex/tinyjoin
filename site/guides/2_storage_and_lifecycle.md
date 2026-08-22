# Storage and lifecycle

TinyGres supports an ephemeral memory database and an opt-in persistent browser
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
TinyGres opens the database with an exclusive synchronous access handle. A
second Worker opening the same name fails rather than risking concurrent
mutation; closing the first Client releases the lock.

There is no silent fallback to memory if persistent storage is unavailable,
locked, corrupt, or out of quota.

## Browser retention

OPFS is browser-managed storage. A user can clear it and a browser may evict
best-effort storage under pressure. An application that needs stronger local
retention can make a user-appropriate `navigator.storage.persist()` request.
TinyGres does not make that product decision during startup.

## Opening and closing

await create() is the simplest lifecycle: it returns only after
initialization succeeds. The returned Client also exposes `ready`, `waitReady`,
and `closed` for code that constructs a Client directly.

Call close() during application teardown. It seals prepared statements,
closes storage, and terminates TinyGres's packaged Worker. Outstanding cleanup
is shared by repeated calls.

The current persistent database is bounded to 65,536 4 KiB pages (256 MiB).
The [SQL compatibility guide](/guides/sql-compatibility/#hard-limits) lists the
independent query and mutation limits.
