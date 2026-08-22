# Releases

This is a reverse chronological summary of TinyGres releases and their public
compatibility boundaries.

## v0.0.5

This release establishes TinyGres as a standalone, SQL-first browser database
package.

- create() opens the packaged dedicated Worker and WebAssembly engine with no
  application Worker boilerplate.
- Memory and persistent OPFS databases use one bounded page-native engine.
- The JavaScript client provides parameterized queries, atomic scripts,
  callback transactions, reusable prepared statements, and table-level change
  subscriptions.
- Typed tables support primary keys, maintained secondary and unique indexes,
  bounded schema additions, aggregates, and left-deep inner and left joins.
- Results use a familiar `rows`, `fields`, `affectedRows`, `command`, and
  `rowCount` shape, with TinyGres revision and changed-table metadata.
- The published package includes its runtime, Worker, WASM, declarations,
  compatibility guide, README, release notes, and agent guidance.

TinyGres remains experimental and deliberately smaller than PostgreSQL. This
release does not include a PostgreSQL server or wire protocol, hosted service,
remote replication, or offline-write synchronization.
