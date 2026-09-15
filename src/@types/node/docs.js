/**
 * The node module opens ephemeral TinyJoin databases in Node.js 22 or later.
 * It constructs a Node Worker thread and loads the packaged WebAssembly engine
 * automatically, with no extra dependencies, bundler, or polyfills.
 *
 * The returned Client has the same SQL, transaction, prepared-statement, and
 * subscription APIs as the browser Client. The module also exports ClientError
 * and the shared Client API types, so Node applications can import their query,
 * result, transaction, and subscription contracts from this entry point.
 * ClientError is the same class exported by the browser entry point.
 * See the [Node guide](/guides/node/).
 * @packageDocumentation
 * @module node
 * @since v0.0.6
 */
/// node

/**
 * The create function opens a fresh in-memory database in a dedicated Node
 * Worker thread. It resolves when the database is ready.
 *
 * Omit the argument or pass `memory://`. Each call owns an independent database
 * and Worker; close the Client to release the Worker. All data is lost when
 * the Client closes or the process exits. OPFS, filesystem persistence, and
 * remote synchronization are not supported by this entry point.
 * @param dataDir The optional `memory://` URL.
 * @returns A ready Client. Always await its close method when finished.
 * @example
 * ```ts
 * import {type Client, create} from 'tinyjoin/node';
 *
 * const db: Client = await create();
 * try {
 *   await db.exec('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)');
 *   await db.query('INSERT INTO notes VALUES ($1, $2)', [1, 'Hello from Node']);
 *   console.log((await db.query('SELECT * FROM notes ORDER BY id')).rows);
 * } finally {
 *   await db.close();
 * }
 * ```
 * @category Lifecycle
 * @since v0.0.6
 */
/// node.create
