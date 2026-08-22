/**
 * The tinygres module provides a small PostgreSQL-shaped relational database
 * that runs in a dedicated browser Worker and stores data in memory or OPFS.
 *
 * Start with the create function. It constructs the Worker and loads the
 * WebAssembly engine, so applications do not need to manage either directly.
 * @packageDocumentation
 * @module tinygres
 * @since v0.0.5
 */
/// tinygres

/**
 * The JsonPrimitive type represents a scalar value accepted by TinyGres.
 * @category Data types
 * @since v0.0.5
 */
/// JsonPrimitive

/**
 * The JsonValue type represents a parameter or result value accepted by the
 * TinyGres JavaScript API.
 * @category Data types
 * @since v0.0.5
 */
/// JsonValue

/**
 * The Row type represents the default object form of a result row.
 * @category Data types
 * @since v0.0.5
 */
/// Row

/**
 * The DataDir type identifies the database storage mode.
 *
 * Use `memory://` for an ephemeral database, or `opfs://name` for a persistent
 * database. Calling create without a data directory also uses memory.
 * @category Configuration
 * @since v0.0.5
 */
/// DataDir

/**
 * The RowMode type selects object rows or positional array rows.
 * @category Query results
 * @since v0.0.5
 */
/// RowMode

/**
 * The QueryOptions interface configures the shape of query results.
 * @category Query results
 * @since v0.0.5
 */
/// QueryOptions
{
  /**
   * The rowMode property selects object rows by default, or arrays whose values
   * follow the order of the fields property.
   * @category Option
   * @since v0.0.5
   */
  /// QueryOptions.rowMode
}

/**
 * The ResultField interface describes one projected result column.
 * @category Query results
 * @since v0.0.5
 */
/// ResultField
{
  /**
   * The name property contains the projected column name or alias.
   * @category Result
   * @since v0.0.5
   */
  /// ResultField.name

  /**
   * The dataTypeID property contains the closest stable PostgreSQL OID for the
   * TinyGres runtime type.
   * @category Result
   * @since v0.0.5
   */
  /// ResultField.dataTypeID
}

/**
 * The Results interface is returned by queries, prepared statements, and
 * scripts.
 *
 * It follows the familiar PGlite result shape and adds the observed database
 * revision and the names of changed tables.
 * @category Query results
 * @since v0.0.5
 */
/// Results
{
  /**
   * The rows property contains the rows returned by the statement.
   * @category Result
   * @since v0.0.5
   */
  /// Results.rows

  /**
   * The fields property describes the projected columns in result order.
   * @category Result
   * @since v0.0.5
   */
  /// Results.fields

  /**
   * The affectedRows property reports the number of rows changed by a write.
   * @category Result
   * @since v0.0.5
   */
  /// Results.affectedRows

  /**
   * The command property contains the executed statement family.
   * @category Result
   * @since v0.0.5
   */
  /// Results.command

  /**
   * The rowCount property reports the number of rows returned or changed.
   * @category Result
   * @since v0.0.5
   */
  /// Results.rowCount

  /**
   * The revision property contains the database revision observed by the
   * statement.
   * @category Result
   * @since v0.0.5
   */
  /// Results.revision

  /**
   * The tables property contains the tables changed by the statement.
   * @category Result
   * @since v0.0.5
   */
  /// Results.tables
}

/**
 * The SerializedError interface is the stable error envelope sent by the
 * Worker.
 * @category Errors
 * @since v0.0.5
 */
/// SerializedError
{
  /**
   * The code property identifies the TinyGres error family.
   * @category Error
   * @since v0.0.5
   */
  /// SerializedError.code

  /**
   * The message property explains the failure.
   * @category Error
   * @since v0.0.5
   */
  /// SerializedError.message

  /**
   * The details property contains optional JSON-compatible context.
   * @category Error
   * @since v0.0.5
   */
  /// SerializedError.details

  /**
   * The retryable property indicates whether reopening or retrying may succeed.
   * @category Error
   * @since v0.0.5
   */
  /// SerializedError.retryable
}

/**
 * The WorkerLike interface is the minimal Worker surface accepted by a custom
 * Client configuration.
 *
 * Most applications should let create construct the packaged Worker instead.
 * @category Workers
 * @since v0.0.5
 */
/// WorkerLike
{
  /**
   * The postMessage method sends a request to the Worker.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.postMessage

  /**
   * This addEventListener overload listens for Worker messages.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.addEventListener.message

  /**
   * This addEventListener overload listens for message decoding failures.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.addEventListener.messageerror

  /**
   * This addEventListener overload listens for Worker runtime failures.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.addEventListener.error

  /**
   * This removeEventListener overload removes a message listener.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.removeEventListener.message

  /**
   * This removeEventListener overload removes a message-error listener.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.removeEventListener.messageerror

  /**
   * This removeEventListener overload removes a Worker error listener.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.removeEventListener.error

  /**
   * The terminate method stops an application-owned Worker when available.
   * @category Worker
   * @since v0.0.5
   */
  /// WorkerLike.terminate
}

/**
 * The ClientOptions interface configures storage or an application-owned
 * Worker.
 *
 * The zero-boilerplate default constructs TinyGres's packaged module Worker.
 * Provide at most one of worker, workerFactory, or workerUrl.
 * @category Configuration
 * @since v0.0.5
 */
/// ClientOptions
{
  /**
   * The worker property provides an already-created Worker-compatible object.
   * @category Option
   * @since v0.0.5
   */
  /// ClientOptions.worker

  /**
   * The workerFactory property creates an application-owned Worker lazily.
   * @category Option
   * @since v0.0.5
   */
  /// ClientOptions.workerFactory

  /**
   * The workerUrl property identifies an application-owned module Worker.
   * @category Option
   * @since v0.0.5
   */
  /// ClientOptions.workerUrl

  /**
   * The dataDir property selects memory or a named OPFS database.
   * @category Option
   * @since v0.0.5
   */
  /// ClientOptions.dataDir
}

/**
 * The TablesChangedEvent interface describes one committed table-level
 * invalidation.
 * @category Subscriptions
 * @since v0.0.5
 */
/// TablesChangedEvent
{
  /**
   * The revision property contains the committed database revision.
   * @category Event
   * @since v0.0.5
   */
  /// TablesChangedEvent.revision

  /**
   * The tables property contains the names of changed tables.
   * @category Event
   * @since v0.0.5
   */
  /// TablesChangedEvent.tables
}

/**
 * The SubscriptionOptions interface filters invalidations by table name.
 * @category Subscriptions
 * @since v0.0.5
 */
/// SubscriptionOptions
{
  /**
   * The tables property limits delivery to invalidations that include at least
   * one of these tables. Omit it to observe every table-change invalidation;
   * adjacent changes may be coalesced.
   * @category Option
   * @since v0.0.5
   */
  /// SubscriptionOptions.tables
}

/**
 * The PreparedStatement interface represents one parsed SELECT, INSERT,
 * UPDATE, or DELETE statement retained by an open Client.
 * @category SQL
 * @since v0.0.5
 */
/// PreparedStatement
{
  /**
   * The execute method binds a complete parameter list and runs the statement.
   * @param params JSON-compatible values for `$1`, `$2`, and so on.
   * @param options Result-shape options.
   * @returns A Promise resolving to the statement results.
   * @category SQL
   * @since v0.0.5
   */
  /// PreparedStatement.execute

  /**
   * The close method seals the handle and releases its Worker resources after
   * already-started executions settle.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// PreparedStatement.close

  /**
   * The closed property indicates whether the handle has been sealed.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// PreparedStatement.closed
}

/**
 * The Transaction interface runs reads and row mutations against one isolated
 * staged database inside a Client.transaction callback.
 *
 * Do not retain this object after its callback completes. Run schema DDL in a
 * standalone Client.query call or Client.exec script.
 * @category Transactions
 * @since v0.0.5
 */
/// Transaction
{
  /**
   * The query method runs one parameterized statement against staged data.
   * @category Transactions
   * @since v0.0.5
   */
  /// Transaction.query

  /**
   * The sql method is a parameterizing tagged-template form of query.
   * @category Transactions
   * @since v0.0.5
   */
  /// Transaction.sql

  /**
   * The exec method runs a parameter-free DML and read script as one savepoint.
   * @category Transactions
   * @since v0.0.5
   */
  /// Transaction.exec

  /**
   * The execute method runs a prepared statement owned by the same Client.
   * @category Transactions
   * @since v0.0.5
   */
  /// Transaction.execute

  /**
   * The rollback method explicitly discards the staged transaction.
   * @category Transactions
   * @since v0.0.5
   */
  /// Transaction.rollback

  /**
   * The closed property indicates that the transaction is sealed, including
   * after an explicit rollback or after its callback finishes.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// Transaction.closed
}

/**
 * The Client class represents one open TinyGres database and its dedicated
 * Worker.
 *
 * Prefer the async create function so initialization failures are reported
 * before the Client is returned.
 * @category Lifecycle
 * @since v0.0.5
 */
/// Client
{
  /**
   * The constructor begins opening a Client immediately.
   *
   * Prefer create unless code specifically needs the waitReady lifecycle.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// Client.constructor

  /**
   * The waitReady property resolves when the Worker and database are ready.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// Client.waitReady

  /**
   * The ready property indicates that initialization finished and closing has
   * not started.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// Client.ready

  /**
   * The closed property indicates that Client cleanup has completed.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// Client.closed

  /**
   * The query method executes exactly one parameterized SQL statement.
   * @param sql A statement in the documented TinyGres SQL subset.
   * @param params JSON-compatible values for `$1`, `$2`, and so on.
   * @param options Result-shape options.
   * @returns A Promise resolving to the statement results.
   * @category SQL
   * @since v0.0.5
   */
  /// Client.query

  /**
   * The sql method is a parameterizing tagged-template form of query.
   *
   * Interpolated values become `$n` parameters. It does not interpolate raw
   * identifiers or SQL fragments.
   * @category SQL
   * @since v0.0.5
   */
  /// Client.sql

  /**
   * The prepare method parses and retains one reusable read or row-mutation
   * statement in the Worker.
   * @category SQL
   * @since v0.0.5
   */
  /// Client.prepare

  /**
   * The exec method runs one or more parameter-free statements as one implicit
   * transaction.
   * @category SQL
   * @since v0.0.5
   */
  /// Client.exec

  /**
   * The transaction method stages row mutations and publishes them together
   * when the callback succeeds, unless it explicitly rolls back.
   * @category Transactions
   * @since v0.0.5
   */
  /// Client.transaction

  /**
   * The subscribe method listens for committed table changes and returns an
   * unsubscribe function.
   * @category Subscriptions
   * @since v0.0.5
   */
  /// Client.subscribe

  /**
   * The getRevision method returns the newest database revision observed by
   * this Client.
   * @category Subscriptions
   * @since v0.0.5
   */
  /// Client.getRevision

  /**
   * The close method releases prepared statements, storage, and the Worker.
   * Repeated calls share the same asynchronous cleanup.
   * @category Lifecycle
   * @since v0.0.5
   */
  /// Client.close
}

/**
 * The create function opens a TinyGres database in a dedicated Worker.
 *
 * Calling it with no argument creates an ephemeral memory database. Pass a
 * stable `opfs://name` to persist the database in browser storage.
 * @example
 * ```ts
 * import {create} from 'tinygres';
 *
 * const db = await create('opfs://my-app');
 * await db.exec(`
 *   CREATE TABLE IF NOT EXISTS tasks (
 *     id INTEGER PRIMARY KEY,
 *     title TEXT NOT NULL
 *   )
 * `);
 * const {rows} = await db.query('SELECT * FROM tasks ORDER BY id');
 * await db.close();
 * ```
 * @category Lifecycle
 * @since v0.0.5
 */
/// create

/**
 * The ClientError class extends JavaScript Error with a validated error
 * returned by the TinyGres Worker.
 * @category Errors
 * @since v0.0.5
 */
/// ClientError
{
  /**
   * The constructor creates an Error from its serialized Worker envelope.
   * @category Error
   * @since v0.0.5
   */
  /// ClientError.constructor

  /**
   * The code property identifies the TinyGres error family.
   * @category Error
   * @since v0.0.5
   */
  /// ClientError.code

  /**
   * The details property contains optional JSON-compatible context.
   * @category Error
   * @since v0.0.5
   */
  /// ClientError.details

  /**
   * The retryable property indicates whether reopening or retrying may succeed.
   * @category Error
   * @since v0.0.5
   */
  /// ClientError.retryable
}
