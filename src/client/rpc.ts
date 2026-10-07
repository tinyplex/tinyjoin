import {
  arrayIsArray,
  isRecord,
  isUndefined,
  objFreeze,
  objHasOwn,
} from '../common.js';
import {
  PROTOCOL_VERSION,
  STATEMENT_PREPARED,
  STATEMENT_SELECT,
  isRpcResult,
  isRpcResultHeader,
  isSqlDataText,
  isStatementResponse,
  isWorkerEvent,
  isWorkerResponse,
  type JsonValue,
  type RpcMethod,
  type RpcMethods,
  type SqlResult,
  type StatementRequest,
  type StatementResponse,
  type StatementResult,
  type WorkerEvent,
  type WorkerRequest,
} from '../protocol.js';
import type {WorkerLike} from './client.js';
import {ClientError, clientError} from './error.js';

export type ResultValidation = 'full' | 'header';

/**
 * What a request settles with: its method's result. A statement's result that
 * published nothing may instead arrive as the flat array of a
 * {@link StatementResponse}, however the statement was sent.
 */
export type RpcResult<Method extends RpcMethod> = Method extends
  | 'executeSql'
  | 'executePrepared'
  ? SqlResult | StatementResult
  : RpcMethods[Method]['response'];

export interface WorkerRpc {
  /**
   * Sends a request, and settles with its result, as `read` returns it when
   * given. Reading the result as the response arrives, rather than in a
   * promise chained onto this one, saves the caller a turn of the microtask
   * queue between one statement and the next.
   */
  request<Method extends RpcMethod, Result = RpcResult<Method>>(
    method: Method,
    params: RpcMethods[Method]['request'],
    read?: (response: RpcResult<Method>) => Result,
  ): Promise<Result>;
  onEvent(listener: (event: WorkerEvent) => void): void;
  dispose(error?: ClientError): void;
}

/**
 * Told that a statement has settled, as its response arrives and before its
 * promise settles: whether it failed, the error if it did, and the `context`
 * the statement was sent with.
 */
export type StatementSettled<Context> = (
  failed: boolean,
  error: unknown,
  context: Context,
) => void;

/**
 * The connection a page holds to its Worker, which sends a statement in the
 * flat form as well as any request in the general one.
 */
export interface PageRpc extends WorkerRpc {
  /**
   * Sends a statement as a {@link StatementRequest}, and settles with its
   * result: as `readFlat` returns it when the response is a
   * {@link StatementResponse}, or as `read` returns it when the result came
   * as a {@link SqlResult}. `operation` is the request's, and `target` the SQL
   * text or the prepared statement's id that goes with it.
   *
   * Parameters that no statement can take go as they were given, in the
   * request object the statement stands for, for the Worker or its engine to
   * refuse: those that are not an array, which no flat request can hold apart,
   * and an array of more than a statement may refer to.
   *
   * A caller that must know when its statements have settled passes `settled`
   * rather than attaching a reaction to every promise. It is called exactly
   * once for the statement, whatever becomes of it, a connection already
   * disposed included. Such a statement is its caller's to answer for, so if
   * it fails, its promise is marked as handled, and raises no unhandled
   * rejection when nothing else reads it.
   */
  execute<Result, Context = undefined>(
    operation: StatementRequest[2],
    target: string | number,
    transaction: StatementRequest[4],
    arrayRows: boolean,
    params: readonly JsonValue[],
    readFlat: (response: StatementResponse) => Result,
    read: (result: SqlResult) => Result,
    settled?: StatementSettled<Context>,
    context?: Context,
  ): Promise<Result>;
}

type PendingRequest = {
  method: RpcMethod;
  // What reads a result that arrived in a response object, and what reads one
  // that arrived as a statement's flat response.
  read: ((response: never) => unknown) | undefined;
  readFlat: ((response: never) => unknown) | undefined;
  resolve(value: unknown): void;
  reject(error: unknown): void;
  // A tracked statement's hook and context, and the promise to mark as handled
  // should the statement fail.
  settled: StatementSettled<never> | undefined;
  context: unknown;
  promise: Promise<unknown> | undefined;
};

const MISMATCH = 'PROTOCOL_MISMATCH';
const INVALID_MESSAGE = 'The TinyJoin worker sent an invalid protocol message';

// Whether an array holds each of its slots and no other property. Counting
// its keys is not enough, since a hole and a named property together make the
// count right, so the last key is tested too.
const isWholeArray = (value: readonly unknown[]): boolean => {
  const keys = Object.keys(value);
  return (
    keys.length === value.length &&
    keys[value.length - 1] === String(value.length - 1)
  );
};

/** The error for a result that is not what its request asks for. */
export const invalidResult = (): ClientError =>
  clientError(
    MISMATCH,
    'The TinyJoin worker returned an invalid result for the requested operation',
  );

const noop = (): void => undefined;

const apply = Reflect.apply;
const push = Array.prototype.push;

// As many parameters as a statement can refer to, `$1` to `$1024`.
const MAX_STATEMENT_PARAMS = 1024;

/** The error for a request made after its connection was disposed. */
export const workerTerminated = (): ClientError =>
  clientError('WORKER_TERMINATED', 'The TinyJoin worker has been closed');

// The error for a request the Worker could not be sent. What a Worker's
// stand-in threw may be past describing, as an object that cannot become a
// string is. The request then fails with what describing it threw, which is
// what a promise makes of an executor that throws.
const postFailed = (error: unknown): unknown => {
  try {
    return error instanceof ClientError
      ? error
      : clientError(
          'WORKER_POST_FAILED',
          error instanceof Error ? error.message : String(error),
        );
  } catch (indescribable) {
    return indescribable;
  }
};

/**
 * Carries the RPC protocol over one Worker.
 *
 * Anything that makes the Worker untrustworthy - an unreadable message, an
 * unexpected result, a crash - disposes the connection and rejects every
 * request still in flight, rather than leaving a caller waiting forever.
 */
export const createWorkerRpc = (
  worker: WorkerLike,
  resultValidation: ResultValidation = 'full',
): PageRpc => {
  const pending = new Map<number, PendingRequest>();
  const eventListeners = new Set<(event: WorkerEvent) => void>();
  const deep = resultValidation === 'full';
  let nextId = 1;
  let disposed = false;

  // Fails a request. The caller that tracks a statement is told first, and the
  // rejection is then marked as handled, so that a statement nothing else
  // reads raises no unhandled rejection. Only a statement that fails pays for
  // that reaction.
  const fail = (request: PendingRequest, error: unknown): void => {
    if (request.settled !== undefined) {
      request.settled(true, error, request.context as never);
      void request.promise!.then(undefined, noop);
    }
    request.reject(error);
  };

  const dispose = (error?: ClientError): void => {
    if (disposed) {
      return;
    }
    disposed = true;
    worker.removeEventListener('message', onMessage);
    worker.removeEventListener('messageerror', onMessageError);
    worker.removeEventListener('error', onError);
    worker.terminate?.();
    const reason = error ?? workerTerminated();
    for (const request of pending.values()) {
      fail(request, reason);
    }
    pending.clear();
    eventListeners.clear();
  };

  const onMessage = (event: MessageEvent<unknown>): void => {
    const data = event.data;
    let request: PendingRequest | undefined;
    let result: unknown;
    let read: ((response: never) => unknown) | undefined;
    if (arrayIsArray(data)) {
      // An array is a statement's response, and the whole of its message. It
      // is judged in the order a response object is. First its envelope: the
      // version, and an id that a request could have, without which it is an
      // invalid message. Then its request: one that names no request in
      // flight is passed over, whatever else it holds. Only then its result:
      // what follows the id must be a statement's, and the request a
      // statement, or it is an invalid result for that request. Either
      // failure disposes the connection. A Worker an application supplied is
      // also held to an array of its slots and nothing else, as its response
      // object is held to its four keys.
      const id: unknown = data[1];
      if (
        data[0] !== PROTOCOL_VERSION ||
        !(
          typeof id === 'number' &&
          id >= 1 &&
          id <= 9007199254740991 &&
          id % 1 === 0
        ) ||
        (deep && !isWholeArray(data))
      ) {
        dispose(clientError(MISMATCH, INVALID_MESSAGE));
        return;
      }
      request = pending.get(id);
      if (request === undefined) {
        return;
      }
      if (
        (request.method !== 'executePrepared' &&
          request.method !== 'executeSql') ||
        !isStatementResponse(data, deep)
      ) {
        dispose(invalidResult());
        return;
      }
      pending.delete(id);
      // A read's rows are text the engine wrote and the Worker passed on
      // unread. Text that cannot be read fails its own statement, as the
      // Worker would have failed it: the Worker and its database are unharmed.
      if (
        deep &&
        data[2] === STATEMENT_SELECT &&
        !isSqlDataText(data[5] as string)
      ) {
        fail(
          request,
          clientError(
            'BRIDGE_SERIALIZATION_ERROR',
            'WASM returned an invalid structured SQL result',
          ),
        );
        return;
      }
      result = data;
      read = request.readFlat;
    } else {
      // A response carries an id and an event does not, so a response skips
      // the event check.
      if (!(isRecord(data) && objHasOwn(data, 'id')) && isWorkerEvent(data)) {
        for (const listener of eventListeners) {
          listener(data);
        }
        return;
      }
      if (!isWorkerResponse(data)) {
        dispose(clientError(MISMATCH, INVALID_MESSAGE));
        return;
      }
      request = pending.get(data.id);
      if (isUndefined(request)) {
        return;
      }
      if (!data.ok) {
        pending.delete(data.id);
        fail(request, new ClientError(data.error));
        return;
      }
      result = data.result;
      if (
        !(deep ? isRpcResult : isRpcResultHeader)(request.method, result)
      ) {
        dispose(invalidResult());
        return;
      }
      pending.delete(data.id);
      read = request.read;
    }
    if (read !== undefined) {
      try {
        result = read(result as never);
      } catch (error) {
        fail(request, error);
        return;
      }
    }
    if (request.settled !== undefined) {
      request.settled(false, undefined, request.context as never);
    }
    request.resolve(result);
  };

  const onMessageError = (): void =>
    dispose(
      clientError(
        'WORKER_MESSAGE_ERROR',
        'The browser could not deserialize a TinyJoin worker message',
      ),
    );

  const onError = (event: ErrorEvent): void =>
    dispose(
      clientError(
        'WORKER_ERROR',
        event.message || 'The TinyJoin worker crashed',
      ),
    );

  worker.addEventListener('message', onMessage);
  worker.addEventListener('messageerror', onMessageError);
  worker.addEventListener('error', onError);

  return objFreeze({
    request: <Method extends RpcMethod, Result = RpcResult<Method>>(
      method: Method,
      params: RpcMethods[Method]['request'],
      read?: (response: RpcResult<Method>) => Result,
    ): Promise<Result> => {
      if (disposed) {
        return Promise.reject(workerTerminated());
      }
      const id = nextId++;
      const message = {v: PROTOCOL_VERSION, id, method, params} as WorkerRequest;
      return new Promise((resolve, reject) => {
        // A statement sent this way may still be answered by a flat response,
        // which the same reader is then given.
        pending.set(id, {
          method,
          read,
          readFlat: read,
          resolve,
          reject,
          settled: undefined,
          context: undefined,
          promise: undefined,
        });
        try {
          worker.postMessage(message);
        } catch (error) {
          pending.delete(id);
          reject(postFailed(error));
        }
      }) as Promise<Result>;
    },

    execute: <Result, Context = undefined>(
      operation: StatementRequest[2],
      target: string | number,
      transaction: StatementRequest[4],
      arrayRows: boolean,
      params: readonly JsonValue[],
      readFlat: (response: StatementResponse) => Result,
      read: (result: SqlResult) => Result,
      settled?: StatementSettled<Context>,
      context?: Context,
    ): Promise<Result> => {
      let request!: PendingRequest;
      const promise = new Promise<Result>((resolve, reject) => {
        request = {
          method:
            operation === STATEMENT_PREPARED ? 'executePrepared' : 'executeSql',
          read,
          readFlat,
          resolve: resolve as (value: unknown) => void,
          reject,
          settled,
          context,
          promise: undefined,
        };
      });
      request.promise = promise;
      if (disposed) {
        fail(request, workerTerminated());
        return promise;
      }
      const id = nextId++;
      pending.set(id, request);
      try {
        if (arrayIsArray(params) && params.length <= MAX_STATEMENT_PARAMS) {
          const message: unknown[] = [
            PROTOCOL_VERSION,
            id,
            operation,
            target,
            transaction,
            arrayRows ? 1 : 0,
          ];
          // The parameters are appended in one call, which costs what
          // appending one does. Each is an argument on the stack, and no
          // statement takes more of them than this.
          apply(push, message, params);
          worker.postMessage(message);
        } else {
          // Anything else is a mistake, which the Worker or its engine will
          // refuse: parameters that are not an array, or more of them than a
          // statement can refer to. It goes as it was given, in the request
          // object the statement stands for. Laid out flat, a string would
          // pass for a parameter to each character, and an array that is
          // nearly all holes would be copied slot by slot on this thread,
          // where a structured clone writes only what the array holds.
          worker.postMessage({
            v: PROTOCOL_VERSION,
            id,
            method: request.method,
            params: {
              ...(operation === STATEMENT_PREPARED
                ? {statementId: target}
                : {sql: target}),
              params,
              ...(transaction === 0 ? {} : {transactionId: transaction}),
              ...(arrayRows ? {rowMode: 'array'} : {}),
            },
          });
        }
      } catch (error) {
        // A Worker's stand-in on the page may have answered the statement, or
        // lost the connection, before its postMessage threw. The statement
        // has then settled already, and its caller has been told.
        if (pending.delete(id)) {
          fail(request, postFailed(error));
        }
      }
      return promise;
    },

    onEvent: (listener: (event: WorkerEvent) => void): void => {
      eventListeners.add(listener);
    },

    dispose,
  });
};
