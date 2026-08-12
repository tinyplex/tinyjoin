import type {
  ChangeBatch,
  JsonValue,
  Row,
  TableSchema,
} from '../protocol.js';

export const JOURNAL_TRANSACTION_VERSION = 1 as const;
export const MAX_JOURNAL_TRANSACTION_BYTES = 16 * 1024 * 1024;
export const MAX_JOURNAL_MUTATIONS = 1_024;
export const MAX_JOURNAL_SQL_STATEMENTS = 1_024;

const MAX_SCHEMAS = 4_096;
const MAX_PRIMARY_KEY_COLUMNS = 64;
const MAX_ROWS = 100_000;
const MAX_CHANGES = 100_000;
const MAX_PARAMS = 65_535;
const MAX_JSON_DEPTH = 64;
const MAX_JSON_NODES = 1_000_000;
const MAX_NAME_CODE_UNITS = 1_024;
const MAX_METADATA_CODE_UNITS = 65_536;
const MAX_SQL_CODE_UNITS = 1024 * 1024;

export interface JournalStatement {
  sql: string;
  params: JsonValue[];
}

export type JournalMutation =
  | {type: 'defineTables'; schemas: TableSchema[]}
  | {type: 'replaceTableSnapshot'; schema: TableSchema; rows: Row[]}
  | {type: 'applyBatch'; batch: ChangeBatch}
  | {type: 'executeSql'; statements: JournalStatement[]};

export interface JournalTransaction {
  version: typeof JOURNAL_TRANSACTION_VERSION;
  revisionBefore: number;
  revisionAfter: number;
  mutations: JournalMutation[];
}

export class JournalPayloadError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = 'JournalPayloadError';
    this.code = code;
  }
}

const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', {fatal: true});

/**
 * Encodes a transaction as canonical JSON. Validation also creates a detached
 * JSON-only copy, so accessors, sparse arrays, prototypes, and caller mutation
 * cannot alter the bytes after this function returns.
 */
export function encodeJournalTransaction(
  transaction: JournalTransaction,
): Uint8Array {
  const normalized = normalizeTransaction(transaction);
  const encoded = encoder.encode(JSON.stringify(normalized));
  if (encoded.byteLength > MAX_JOURNAL_TRANSACTION_BYTES) {
    throw invalid(
      `A journal transaction cannot exceed ${MAX_JOURNAL_TRANSACTION_BYTES} bytes`,
      'STORAGE_JOURNAL_RECORD_TOO_LARGE',
    );
  }
  return encoded;
}

/** Decodes and strictly validates a complete transaction payload. */
export function decodeJournalTransaction(
  payload: Uint8Array,
): JournalTransaction {
  if (!(payload instanceof Uint8Array)) {
    throw invalid('A journal transaction payload must be bytes');
  }
  if (payload.byteLength === 0) {
    throw invalid('A journal transaction payload cannot be empty');
  }
  if (payload.byteLength > MAX_JOURNAL_TRANSACTION_BYTES) {
    throw invalid(
      `A journal transaction cannot exceed ${MAX_JOURNAL_TRANSACTION_BYTES} bytes`,
      'STORAGE_JOURNAL_RECORD_TOO_LARGE',
    );
  }

  let value: unknown;
  try {
    value = JSON.parse(decoder.decode(payload));
  } catch {
    throw invalid('A journal transaction payload is not valid UTF-8 JSON');
  }
  return normalizeTransaction(value);
}

interface ValidationBudget {
  nodes: number;
  codeUnits: number;
  seen: WeakSet<object>;
}

function normalizeTransaction(value: unknown): JournalTransaction {
  const budget: ValidationBudget = {
    nodes: 0,
    codeUnits: 0,
    seen: new WeakSet(),
  };
  const transaction = expectRecord(value, 'transaction', [
    'version',
    'revisionBefore',
    'revisionAfter',
    'mutations',
  ]);

  if (transaction.version !== JOURNAL_TRANSACTION_VERSION) {
    throw invalid(
      `Journal transaction version ${String(transaction.version)} is unsupported`,
      'STORAGE_VERSION_UNSUPPORTED',
    );
  }
  const revisionBefore = expectRevision(
    transaction.revisionBefore,
    'revisionBefore',
  );
  const revisionAfter = expectRevision(
    transaction.revisionAfter,
    'revisionAfter',
  );
  if (revisionAfter < revisionBefore) {
    throw invalid('revisionAfter cannot be less than revisionBefore');
  }
  const mutations = expectDenseArray(
    transaction.mutations,
    'mutations',
    1,
    MAX_JOURNAL_MUTATIONS,
  ).map((mutation, index) =>
    normalizeMutation(mutation, `mutations[${index}]`, budget),
  );

  return {
    version: JOURNAL_TRANSACTION_VERSION,
    revisionBefore,
    revisionAfter,
    mutations,
  };
}

function normalizeMutation(
  value: unknown,
  path: string,
  budget: ValidationBudget,
): JournalMutation {
  const candidate = expectPlainRecord(value, path);
  switch (candidate.type) {
    case 'defineTables': {
      expectOnlyKeys(candidate, path, ['type', 'schemas']);
      const schemas = expectDenseArray(
        candidate.schemas,
        `${path}.schemas`,
        1,
        MAX_SCHEMAS,
      ).map((schema, index) =>
        normalizeSchema(schema, `${path}.schemas[${index}]`, budget),
      );
      return {type: 'defineTables', schemas};
    }
    case 'replaceTableSnapshot': {
      expectOnlyKeys(candidate, path, ['type', 'schema', 'rows']);
      const schema = normalizeSchema(candidate.schema, `${path}.schema`, budget);
      const rows = expectDenseArray(
        candidate.rows,
        `${path}.rows`,
        0,
        MAX_ROWS,
      ).map((row, index) => normalizeRow(row, `${path}.rows[${index}]`, budget));
      return {type: 'replaceTableSnapshot', schema, rows};
    }
    case 'applyBatch': {
      expectOnlyKeys(candidate, path, ['type', 'batch']);
      return {
        type: 'applyBatch',
        batch: normalizeBatch(candidate.batch, `${path}.batch`, budget),
      };
    }
    case 'executeSql': {
      expectOnlyKeys(candidate, path, ['type', 'statements']);
      const statements = expectDenseArray(
        candidate.statements,
        `${path}.statements`,
        1,
        MAX_JOURNAL_SQL_STATEMENTS,
      ).map((statement, index) =>
        normalizeStatement(statement, `${path}.statements[${index}]`, budget),
      );
      return {type: 'executeSql', statements};
    }
    default:
      throw invalid(`${path}.type is not a supported journal mutation`);
  }
}

function normalizeSchema(
  value: unknown,
  path: string,
  budget: ValidationBudget,
): TableSchema {
  const schema = expectRecord(value, path, ['name', 'primaryKey']);
  const name = expectString(schema.name, `${path}.name`, MAX_NAME_CODE_UNITS, budget);
  const primaryKey = expectDenseArray(
    schema.primaryKey,
    `${path}.primaryKey`,
    1,
    MAX_PRIMARY_KEY_COLUMNS,
  ).map((column, index) =>
    expectString(
      column,
      `${path}.primaryKey[${index}]`,
      MAX_NAME_CODE_UNITS,
      budget,
    ),
  );
  if (new Set(primaryKey).size !== primaryKey.length) {
    throw invalid(`${path}.primaryKey cannot contain duplicate columns`);
  }
  return {name, primaryKey};
}

function normalizeBatch(
  value: unknown,
  path: string,
  budget: ValidationBudget,
): ChangeBatch {
  const batch = expectPlainRecord(value, path);
  expectOnlyKeys(batch, path, [
    'sourceId',
    'cursor',
    'transactionId',
    'committedAt',
    'changes',
  ]);
  if (!Object.hasOwn(batch, 'changes')) {
    throw invalid(`${path}.changes is required`);
  }
  const changes = expectDenseArray(
    batch.changes,
    `${path}.changes`,
    0,
    MAX_CHANGES,
  ).map((change, index) =>
    normalizeChange(change, `${path}.changes[${index}]`, budget),
  );

  const normalized: ChangeBatch = {changes};
  if (Object.hasOwn(batch, 'sourceId')) {
    normalized.sourceId = expectString(
      batch.sourceId,
      `${path}.sourceId`,
      MAX_METADATA_CODE_UNITS,
      budget,
      true,
    );
  }
  if (Object.hasOwn(batch, 'cursor')) {
    const cursor = expectRecord(batch.cursor, `${path}.cursor`, ['kind', 'value']);
    normalized.cursor = {
      kind: expectString(
        cursor.kind,
        `${path}.cursor.kind`,
        MAX_METADATA_CODE_UNITS,
        budget,
      ),
      value: expectString(
        cursor.value,
        `${path}.cursor.value`,
        MAX_METADATA_CODE_UNITS,
        budget,
        true,
      ),
    };
  }
  if (Object.hasOwn(batch, 'transactionId')) {
    normalized.transactionId = expectString(
      batch.transactionId,
      `${path}.transactionId`,
      MAX_METADATA_CODE_UNITS,
      budget,
      true,
    );
  }
  if (Object.hasOwn(batch, 'committedAt')) {
    normalized.committedAt = expectString(
      batch.committedAt,
      `${path}.committedAt`,
      MAX_METADATA_CODE_UNITS,
      budget,
      true,
    );
  }
  return normalized;
}

function normalizeChange(
  value: unknown,
  path: string,
  budget: ValidationBudget,
): ChangeBatch['changes'][number] {
  const change = expectPlainRecord(value, path);
  if (change.type === 'upsert') {
    expectOnlyKeys(change, path, ['type', 'table', 'row']);
    return {
      type: 'upsert',
      table: expectString(
        change.table,
        `${path}.table`,
        MAX_NAME_CODE_UNITS,
        budget,
      ),
      row: normalizeRow(change.row, `${path}.row`, budget),
    };
  }
  if (change.type === 'delete') {
    expectOnlyKeys(change, path, ['type', 'table', 'key']);
    return {
      type: 'delete',
      table: expectString(
        change.table,
        `${path}.table`,
        MAX_NAME_CODE_UNITS,
        budget,
      ),
      key: normalizeRow(change.key, `${path}.key`, budget),
    };
  }
  throw invalid(`${path}.type is not a supported change`);
}

function normalizeStatement(
  value: unknown,
  path: string,
  budget: ValidationBudget,
): JournalStatement {
  const statement = expectRecord(value, path, ['sql', 'params']);
  const sql = expectString(
    statement.sql,
    `${path}.sql`,
    MAX_SQL_CODE_UNITS,
    budget,
  );
  const params = expectDenseArray(
    statement.params,
    `${path}.params`,
    0,
    MAX_PARAMS,
  ).map((param, index) =>
    normalizeJson(param, `${path}.params[${index}]`, budget, 0),
  );
  return {sql, params};
}

function normalizeRow(
  value: unknown,
  path: string,
  budget: ValidationBudget,
): Row {
  const row = expectPlainRecord(value, path);
  return normalizeJsonRecord(row, path, budget, 0);
}

function normalizeJson(
  value: unknown,
  path: string,
  budget: ValidationBudget,
  depth: number,
): JsonValue {
  consumeNode(budget, path);
  if (value === null || typeof value === 'boolean') {
    return value;
  }
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) {
      throw invalid(`${path} must contain only finite JSON numbers`);
    }
    return value;
  }
  if (typeof value === 'string') {
    consumeString(budget, value, path, MAX_JOURNAL_TRANSACTION_BYTES);
    return value;
  }
  if (depth >= MAX_JSON_DEPTH) {
    throw invalid(`${path} exceeds the maximum JSON nesting depth`);
  }
  if (Array.isArray(value)) {
    assertDenseArray(value, path);
    enterObject(value, path, budget);
    try {
      return value.map((item, index) =>
        normalizeJson(item, `${path}[${index}]`, budget, depth + 1),
      );
    } finally {
      budget.seen.delete(value);
    }
  }
  const record = expectPlainRecord(value, path);
  enterObject(record, path, budget);
  try {
    return normalizeJsonRecord(record, path, budget, depth);
  } finally {
    budget.seen.delete(record);
  }
}

function normalizeJsonRecord(
  record: Record<string, unknown>,
  path: string,
  budget: ValidationBudget,
  depth: number,
): Row {
  const normalized = Object.create(null) as Row;
  for (const key of Object.keys(record).sort()) {
    consumeString(budget, key, `${path} key`, MAX_JOURNAL_TRANSACTION_BYTES);
    normalized[key] = normalizeJson(
      record[key],
      `${path}.${key}`,
      budget,
      depth + 1,
    );
  }
  return normalized;
}

function expectRecord(
  value: unknown,
  path: string,
  keys: readonly string[],
): Record<string, unknown> {
  const record = expectPlainRecord(value, path);
  expectOnlyKeys(record, path, keys);
  for (const key of keys) {
    if (!Object.hasOwn(record, key)) {
      throw invalid(`${path}.${key} is required`);
    }
  }
  return record;
}

function expectPlainRecord(
  value: unknown,
  path: string,
): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw invalid(`${path} must be an object`);
  }
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) {
    throw invalid(`${path} must be a plain JSON object`);
  }
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== 'string') {
      throw invalid(`${path} cannot contain symbol properties`);
    }
    const descriptor = Object.getOwnPropertyDescriptor(value, key);
    if (!descriptor?.enumerable || !('value' in descriptor)) {
      throw invalid(`${path}.${key} must be an enumerable data property`);
    }
  }
  return value as Record<string, unknown>;
}

function expectOnlyKeys(
  value: Record<string, unknown>,
  path: string,
  keys: readonly string[],
): void {
  const allowed = new Set(keys);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) {
      throw invalid(`${path}.${key} is not supported`);
    }
  }
}

function expectDenseArray(
  value: unknown,
  path: string,
  minimum: number,
  maximum: number,
): unknown[] {
  if (!Array.isArray(value)) {
    throw invalid(`${path} must be an array`);
  }
  assertDenseArray(value, path);
  if (value.length < minimum || value.length > maximum) {
    throw invalid(`${path} must contain between ${minimum} and ${maximum} items`);
  }
  return value;
}

function assertDenseArray(value: unknown[], path: string): void {
  if (Object.getPrototypeOf(value) !== Array.prototype) {
    throw invalid(`${path} must be a plain array`);
  }
  for (let index = 0; index < value.length; index += 1) {
    if (!Object.hasOwn(value, index)) {
      throw invalid(`${path} cannot be sparse`);
    }
  }
  for (const key of Reflect.ownKeys(value)) {
    if (key === 'length') {
      continue;
    }
    if (
      typeof key !== 'string' ||
      !/^(0|[1-9]\d*)$/.test(key) ||
      Number(key) >= value.length
    ) {
      throw invalid(`${path} cannot contain non-index properties`);
    }
    const descriptor = Object.getOwnPropertyDescriptor(value, key);
    if (!descriptor?.enumerable || !('value' in descriptor)) {
      throw invalid(`${path}[${key}] must be an enumerable data property`);
    }
  }
}

function expectRevision(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) {
    throw invalid(`${path} must be a non-negative safe integer`);
  }
  return Number(value);
}

function expectString(
  value: unknown,
  path: string,
  maximum: number,
  budget: ValidationBudget,
  allowEmpty = false,
): string {
  if (
    typeof value !== 'string' ||
    (!allowEmpty && value.length === 0) ||
    value.length > maximum
  ) {
    throw invalid(
      `${path} must be ${allowEmpty ? '' : 'a non-empty '}string of at most ${maximum} code units`,
    );
  }
  consumeString(budget, value, path, maximum);
  return value;
}

function consumeNode(budget: ValidationBudget, path: string): void {
  budget.nodes += 1;
  if (budget.nodes > MAX_JSON_NODES) {
    throw invalid(`${path} exceeds the maximum JSON value count`);
  }
}

function consumeString(
  budget: ValidationBudget,
  value: string,
  path: string,
  maximum: number,
): void {
  if (value.length > maximum) {
    throw invalid(`${path} exceeds the maximum string length`);
  }
  budget.codeUnits += value.length;
  if (budget.codeUnits > MAX_JOURNAL_TRANSACTION_BYTES) {
    throw invalid(
      'A journal transaction exceeds its aggregate string allocation limit',
      'STORAGE_JOURNAL_RECORD_TOO_LARGE',
    );
  }
}

function enterObject(
  value: object,
  path: string,
  budget: ValidationBudget,
): void {
  if (budget.seen.has(value)) {
    throw invalid(`${path} cannot contain cyclic object references`);
  }
  budget.seen.add(value);
}

function invalid(
  message: string,
  code = 'STORAGE_JOURNAL_PAYLOAD_INVALID',
): JournalPayloadError {
  return new JournalPayloadError(code, message);
}
