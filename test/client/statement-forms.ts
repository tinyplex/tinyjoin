import {
  PROTOCOL_VERSION,
  STATEMENT_COMMANDS,
  STATEMENT_PARAMS,
  STATEMENT_PREPARED,
  STATEMENT_SELECT,
  type JsonPrimitive,
  type JsonValue,
  type SqlResult,
  type StatementRequest,
  type StatementResponse,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.ts';
import {FakeWorker} from '../helpers/fake-worker.ts';

/**
 * The two forms a statement takes between the page and its Worker, written
 * here from protocol.ts's description of them rather than with the client's
 * own code, so that the tests can say what each side should have sent.
 */

/** The fields and rows of a statement that returns none. */
export const NO_ROWS = '{"fields":[],"rows":[]}';

/**
 * A message the page posted, as the request object it is or stands for: a
 * statement posted as a flat array becomes its `executeSql` or
 * `executePrepared` request, with exactly the keys the page once sent.
 */
export const asRequest = (message: unknown): WorkerRequest => {
  if (!Array.isArray(message)) {
    return message as WorkerRequest;
  }
  const [v, id, operation, target, transaction, arrayRows] =
    message as StatementRequest;
  return {
    v,
    id,
    method: operation === STATEMENT_PREPARED ? 'executePrepared' : 'executeSql',
    params: {
      ...(operation === STATEMENT_PREPARED
        ? {statementId: target}
        : {sql: target}),
      params: (message as JsonValue[]).slice(STATEMENT_PARAMS),
      ...(transaction === 0 ? {} : {transactionId: transaction}),
      ...(arrayRows === 1 ? {rowMode: 'array'} : {}),
    },
  } as WorkerRequest;
};

/**
 * The flat response that stands for a statement's result, or `undefined` for
 * a result that has no flat form and must cross as the object it is. A result
 * that reports a changed table's keys, but has none to report, names the key's
 * columns with `columns`, which no key is there to show.
 */
export const flatResponse = (
  id: number,
  result: SqlResult,
  columns?: string[],
): StatementResponse | undefined => {
  const command = (STATEMENT_COMMANDS as readonly string[]).indexOf(
    result.command,
  );
  const keyTables = Object.keys(result.keys);
  const head: StatementResponse = [
    PROTOCOL_VERSION,
    id,
    command,
    result.revision,
    result.rowCount,
  ];
  if (command === STATEMENT_SELECT) {
    return result.tables.length === 0 && keyTables.length === 0
      ? [...head, result.data]
      : undefined;
  }
  const table = result.tables[0];
  if (
    command < 0 ||
    result.data !== NO_ROWS ||
    result.tables.length > 1 ||
    keyTables.length > result.tables.length ||
    (keyTables.length === 1 && keyTables[0] !== table)
  ) {
    return undefined;
  }
  if (table === undefined) {
    return head;
  }
  if (keyTables.length === 0) {
    return [...head, table];
  }
  const rows = result.keys[table]!;
  const names = columns ?? Object.keys(rows[0] ?? {});
  const values: JsonPrimitive[] = [];
  for (const row of rows) {
    if (Object.keys(row).length !== names.length) {
      return undefined;
    }
    for (const name of names) {
      const value = row[name];
      if (value === undefined || (value !== null && typeof value === 'object')) {
        return undefined;
      }
      values.push(value);
    }
  }
  return names.length === 0
    ? undefined
    : [...head, table, names.length, ...names, ...values];
};

/**
 * A fake Worker for a page that sends its statements flat. Each message the
 * page posts is kept as it was posted, and handed to `onPost` as the request
 * object it is or stands for, so that a fake can answer a statement without
 * minding which form it came in.
 */
export class StatementWorker extends FakeWorker {
  override postMessage(message: unknown): void {
    this.posted.push(message);
    this.onPost?.(asRequest(message));
  }

  /** Posts a message to the page: a response object, an event, or a flat response. */
  override respond(
    message: WorkerResponse | WorkerEvent | StatementResponse,
  ): void {
    // The fake's own respond() takes no array. Its untyped method delivers
    // any message the same way.
    this.emitInvalidMessage(message);
  }

  /** Every message the page posted, each as the request it is or stands for. */
  requests(): WorkerRequest[] {
    return this.posted.map(asRequest);
  }

  /**
   * Answers a statement as a Worker does: with the flat response, when the
   * result published nothing and has a flat form, and otherwise with the
   * result object in a response object.
   */
  respondStatement(id: number, result: SqlResult, published = false): void {
    const flat = published ? undefined : flatResponse(id, result);
    this.respond(flat ?? {v: PROTOCOL_VERSION, id, ok: true, result});
  }
}
