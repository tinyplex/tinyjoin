# Releases

This is a reverse chronological summary of TinyJoin releases and their public
compatibility boundaries. Every entry states what upgrading to it requires, so
check the entries between the version in use and the target before upgrading.
A release that needs no action says so explicitly.

## v0.3.0

This release closes some of the most commonly encountered gaps in the
[SQL dialect](/guides/sql-compatibility/). Each form is new syntax that was
previously rejected, so an existing statement keeps its meaning.

`BETWEEN` and `NOT BETWEEN` are accepted wherever a `WHERE` clause is, including
in aggregates, joins, `UPDATE`, `DELETE`, and prepared statements.
`price BETWEEN $1 AND $2` means exactly `price >= $1 AND price <= $2`, with the
same type checking and `NULL` behavior as those two comparisons, and it is
inclusive at both ends. There is no `SYMMETRIC` form, so a range whose bounds
are reversed matches nothing rather than being swapped.

A single-table `SELECT` can now rename its columns with `AS`, as aggregate and
join queries already could: `SELECT id AS task_id, title FROM tasks`. The alias
becomes the result field name, `ORDER BY` can refer to it, and one column may be
returned under several names. As in PostgreSQL, an `ORDER BY` name matches an
output alias before a source column of the same name. Output names must still be
distinct.

## v0.2.0

Subscriptions and statement results now report **which rows changed**, not only
which tables. A [`TablesChangedEvent`](/api/tinyjoin/interfaces/subscriptions/tableschangedevent/)
and every [`Results`](/api/tinyjoin/interfaces/query-results/results/) carry a
`keys` map of table name to the primary keys that changed, so a listener can
refresh individual rows instead of re-reading a table. Keys are reported for
writes from any connected Client, which makes a change made in another tab
resolvable to individual rows. See
[refreshing individual rows](/guides/transactions-and-changes/#refreshing-individual-rows).

Reporting is a bounded best effort, and the changed-table list stays
authoritative. A table appears in `keys` only when every key it changed fits
the bound of 1,000 keys per table; a table that changed more rows is absent
rather than partially listed, so an absent table means its rows cannot be named
and must be re-queried. `keys` is always empty alongside `reset: true`. Code
that assumes keys are present will silently miss large writes, so handle the
absent case.

**The Worker protocol version moves from 7 to 8.** Client and Worker ship
together and are upgraded together by a normal install, so this affects only a
deployment that pins or caches a Worker file independently of the client
bundle. A mismatched pair fails cleanly with `PROTOCOL_MISMATCH` rather than
misreading a result. Applications using
[`tinyjoinOffline()`](/api/vite/functions/offline/tinyjoinoffline/) are unaffected: the
plugin revisions both files by content, so a rebuild replaces them together.

**Upgrading from v0.1.0 needs no action.** Install, rebuild, and redeploy.
Persistent storage is unchanged: the page format remains format 2, so an
existing database opens with no migration and no OPFS namespace change. The
`keys` field is additive, so application code that ignores it behaves exactly
as it did before.

Grouped and aggregate queries now use a secondary index when their predicate
supplies every column of one, instead of always scanning the table. Results are
unchanged, because the predicate is still evaluated for each candidate row.
Because the scan limit counts candidate rows, a grouped query that previously
exceeded `QUERY_WORK_LIMIT_EXCEEDED` on a large table may now succeed.

`tinyjoin/node` no longer fails to start when the parent process runs with a
flag that Node rejects for a worker thread, such as the `--stack-trace-limit`
that several test runners set. Inherited flags are dropped rather than the
database failing to open.

## v0.1.0

The new [Node entry point](/guides/node/), `tinyjoin/node`, opens in-memory
databases in Node.js 22 or later. It constructs a Worker thread and loads WASM
automatically, returning the same Client API without additional dependencies.
Each Client owns an independent database; filesystem persistence is not included.

**Persistent storage breaks compatibility with v0.0.5.** The npm v0.0.5
release uses page format 1; v0.1.0 uses page format 2. Opening a v0.0.5
database with v0.1.0 fails with `UNSUPPORTED_PAGE`. There is no automatic
migration, and changing the OPFS name does not copy existing data.

- For data that can be reconstructed, use a new OPFS namespace, for example
  change `opfs://my-app-v1` to `opfs://my-app-v2`, and rebuild the database.
  The old namespace remains intact.
- To retain existing data, export the application's known tables using a
  client pinned to `tinyjoin@0.0.5` before upgrading. Create the schema in a
  new namespace with v0.1.0, import rows with parameterized statements, and
  verify the data before retiring the old database. TinyJoin does not yet
  provide a general database export or migration API.
- New apps generated by create-tinyjoin v0.1.0 target `tinyjoin@^0.1.0` and
  use a `-db-v2` storage name. Updating the generator does not migrate
  applications it generated previously.

This release also makes predicate and assignment type validation consistent
across reads and writes, compares heterogeneous JSON values consistently,
and bounds shared parameter-graph traversal and expanded SQL bindings before
copying values. Append-only transactions now validate new row statements
incrementally, including unique-index checks; mixed writes retain complete
staged write-set validation.

Selective multi-table joins now use their actual comparison count rather than
a worst-case Cartesian estimate. Scan, build-row, result, memory, and shared
script-work limits remain in force.

Page checksum calculation is faster while preserving the stored checksum
values, storage format, and corruption checks. This improves mixed writes and
populated startup; the repository's [workload measurements](https://github.com/tinyplex/tinyjoin/tree/main/benchmarks)
retain the before/after samples and runtime hashes.

`UPDATE` and `DELETE` now use direct primary-key lookup for exact complete key
predicates, including staged transaction rows. Other predicates retain their
scan path, and statement validation and constraint checks remain in force.

Persistent startup avoids a redundant table scan for unique indexes whose
columns are all `NOT NULL`. Table validation and complete index-entry checks
remain in force; nullable indexes and indexes without uniqueness retain the
existing count scan.

Duplicate output names in simple projections and `RETURNING` now fail
consistently with `INVALID_QUERY` before execution. `LIMIT` and `OFFSET`
reject nonnumeric parameter values consistently in ordinary, aggregate, and
join queries, including JSON objects shaped like internal prepared placeholders.

Persistent Clients now coordinate automatically across tabs and within a page.
One elected Worker owns the database; operations, prepared statements, and
notifications follow it through handover. Callback transactions exclude other
Clients for their duration. In-flight operations interrupted by owner loss
reject without automatic replay, and incompatible releases reject explicitly.

The optional `tinyjoin/vite` build plugin precaches a complete production
application, including lazy Worker, OPFS, and WASM assets. New starter apps
enable it by default. Updates wait for old controlled tabs to close; existing
service workers can use its generated manifest and helper instead. See the
[offline guide](/guides/offline/).

## v0.0.5

This release establishes TinyJoin as a standalone, SQL-first browser database
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
  `rowCount` shape, with TinyJoin revision and changed-table metadata.
- The published package includes its runtime, Worker, WASM, declarations,
  compatibility guide, README, release notes, and agent guidance.

TinyJoin remains experimental and deliberately smaller than PostgreSQL. This
release does not include a PostgreSQL server or wire protocol, hosted service,
remote replication, or offline-write synchronization.
