/**
 * The worker module lets advanced applications start TinyJoin inside a Worker
 * that they construct and bundle themselves.
 *
 * Most applications should use create from the main tinyjoin module, which
 * constructs the packaged Worker automatically.
 * @packageDocumentation
 * @module worker
 * @since v0.0.5
 */
/// worker

/**
 * The startWorker function starts the TinyJoin request host in the current
 * dedicated Worker.
 * Persistent databases participate in automatic same-name coordination across
 * tabs, just like the packaged Worker. Memory databases remain independent.
 * @example
 * ```ts
 * import {startWorker} from 'tinyjoin/worker';
 *
 * startWorker();
 * ```
 * @category Workers
 * @since v0.0.5
 */
/// startWorker
