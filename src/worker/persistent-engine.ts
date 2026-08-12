import type {
  ApplyOutcome,
  ChangeBatch,
  JsonValue,
  QueryPlan,
  QueryResult,
  Row,
  SqlResult,
  TableSchema,
} from '../protocol.js';
import type {WorkerEngine} from './engine.js';
import {
  decodeJournalTransaction,
  encodeJournalTransaction,
  JOURNAL_TRANSACTION_VERSION,
  MAX_JOURNAL_SQL_STATEMENTS,
  type JournalMutation,
} from './journal-payload.js';
import {
  StorageError,
  type SnapshotCandidate,
  type SnapshotStore,
} from './snapshot-store.js';

const CHECKPOINT_RECORDS = 128;
const CHECKPOINT_BYTES = 1024 * 1024;

type JournalSnapshotStore = SnapshotStore & {
  append(payload: Uint8Array): bigint;
  checkpoint(snapshot: Uint8Array): void;
  journalRecordCount(): number;
  journalByteLength(): number;
};

export function createPersistentEngine(
  engine: WorkerEngine,
  store: SnapshotStore,
): WorkerEngine {
  const journalStore = isJournalStore(store) ? store : undefined;
  const restored = restoreNewestValidSnapshot(engine, store);
  if (journalStore && !restored) {
    journalStore.checkpoint(engine.exportSnapshot());
  }
  let closed = false;
  let poisoned = false;
  let transactionSnapshot: Uint8Array | undefined;
  let transactionStatements: {sql: string; params: JsonValue[]}[] | undefined;

  const closeResources = (): void => {
    if (closed) {
      return;
    }
    closed = true;
    let firstError: unknown;
    try {
      store.close();
    } catch (error) {
      firstError = error;
    }
    try {
      engine.close?.();
    } catch (error) {
      firstError ??= error;
    }
    if (firstError !== undefined) {
      throw firstError;
    }
  };

  const poison = (): void => {
    poisoned = true;
    try {
      closeResources();
    } catch {
      // A poisoned engine must stay unusable even if resource cleanup fails.
    }
  };

  const assertUsable = (): void => {
    if (poisoned) {
      throw new StorageError(
        'STORAGE_ENGINE_POISONED',
        'The TinyGres persistent engine is unavailable after an uncertain storage failure',
      );
    }
    if (closed) {
      throw new StorageError(
        'STORAGE_CLOSED',
        'The TinyGres persistent engine is closed',
      );
    }
  };

  const checkpointIfNeeded = (): void => {
    if (
      journalStore &&
      (journalStore.journalRecordCount() >= CHECKPOINT_RECORDS ||
        journalStore.journalByteLength() >= CHECKPOINT_BYTES)
    ) {
      journalStore.checkpoint(engine.exportSnapshot());
    }
  };

  const mutate = <Result>(
    operation: () => Result,
    mutations: JournalMutation[],
  ): Result => {
    assertUsable();
    checkpointIfNeeded();
    const previous = engine.exportSnapshot();
    const revisionBefore = engine.revision();
    try {
      const result = operation();
      const next = engine.exportSnapshot();
      if (!bytesEqual(previous, next)) {
        if (journalStore) {
          journalStore.append(
            encodeJournalTransaction({
              version: JOURNAL_TRANSACTION_VERSION,
              revisionBefore,
              revisionAfter: engine.revision(),
              mutations,
            }),
          );
        } else {
          store.commit(next);
        }
      }
      return result;
    } catch (error) {
      try {
        engine.importSnapshot(previous);
      } catch (rollbackError) {
        poison();
        throw new StorageError(
          'STORAGE_ROLLBACK_FAILED',
          `TinyGres could not restore memory after a failed durable mutation: ${errorMessage(rollbackError)}`,
        );
      }
      if (errorCode(error) === 'STORAGE_COMMIT_OUTCOME_UNKNOWN') {
        poison();
      }
      throw error;
    }
  };

  return {
    defineTable(schema: TableSchema): void {
      mutate(() => engine.defineTable(schema), [
        {type: 'defineTables', schemas: [schema]},
      ]);
    },
    defineTables(schemas: TableSchema[]): void {
      if (schemas.length === 0) {
        assertUsable();
        engine.defineTables(schemas);
        return;
      }
      mutate(() => engine.defineTables(schemas), [
        {type: 'defineTables', schemas},
      ]);
    },
    replaceTableSnapshot(
      schema: TableSchema,
      rows: Row[],
    ): ApplyOutcome {
      return mutate(() => engine.replaceTableSnapshot(schema, rows), [
        {type: 'replaceTableSnapshot', schema, rows},
      ]);
    },
    applyBatch(batch: ChangeBatch): ApplyOutcome {
      return mutate(() => engine.applyBatch(batch), [
        {type: 'applyBatch', batch},
      ]);
    },
    query(plan: QueryPlan): QueryResult {
      assertUsable();
      return engine.query(plan);
    },
    querySql(sql: string, params: JsonValue[]): QueryResult {
      assertUsable();
      return engine.querySql(sql, params);
    },
    executeSql(sql: string, params: JsonValue[]): SqlResult {
      assertUsable();
      if (engine.inTransaction()) {
        if (
          transactionStatements &&
          transactionStatements.length >= MAX_JOURNAL_SQL_STATEMENTS
        ) {
          throw new StorageError(
            'STORAGE_JOURNAL_PAYLOAD_INVALID',
            `A transaction cannot contain more than ${MAX_JOURNAL_SQL_STATEMENTS} SQL statements`,
          );
        }
        const result = engine.executeSql(sql, params);
        transactionStatements?.push({sql, params});
        return result;
      }
      return mutate(() => engine.executeSql(sql, params), [
        {type: 'executeSql', statements: [{sql, params}]},
      ]);
    },
    beginTransaction(): void {
      assertUsable();
      checkpointIfNeeded();
      const previous = engine.exportSnapshot();
      engine.beginTransaction();
      transactionSnapshot = previous;
      transactionStatements = [];
    },
    commitTransaction(): ApplyOutcome {
      assertUsable();
      if (!transactionSnapshot) {
        return engine.commitTransaction();
      }
      const previous = transactionSnapshot;
      const statements = transactionStatements ?? [];
      const revisionBefore = engine.revision();
      try {
        const result = engine.commitTransaction();
        const next = engine.exportSnapshot();
        if (!bytesEqual(previous, next)) {
          if (journalStore) {
            journalStore.append(
              encodeJournalTransaction({
                version: JOURNAL_TRANSACTION_VERSION,
                revisionBefore,
                revisionAfter: engine.revision(),
                mutations: [{type: 'executeSql', statements}],
              }),
            );
          } else {
            store.commit(next);
          }
        }
        transactionSnapshot = undefined;
        transactionStatements = undefined;
        return result;
      } catch (error) {
        transactionSnapshot = undefined;
        transactionStatements = undefined;
        try {
          if (engine.inTransaction()) {
            engine.rollbackTransaction();
          }
          engine.importSnapshot(previous);
        } catch (rollbackError) {
          poison();
          throw new StorageError(
            'STORAGE_ROLLBACK_FAILED',
            `TinyGres could not restore memory after a failed durable transaction: ${errorMessage(rollbackError)}`,
          );
        }
        if (errorCode(error) === 'STORAGE_COMMIT_OUTCOME_UNKNOWN') {
          poison();
        }
        throw error;
      }
    },
    rollbackTransaction(): void {
      assertUsable();
      try {
        engine.rollbackTransaction();
      } finally {
        transactionSnapshot = undefined;
        transactionStatements = undefined;
      }
    },
    inTransaction(): boolean {
      assertUsable();
      return engine.inTransaction();
    },
    revision(): number {
      assertUsable();
      return engine.revision();
    },
    exportSnapshot(): Uint8Array {
      assertUsable();
      return engine.exportSnapshot();
    },
    importSnapshot(snapshot: Uint8Array): void {
      assertUsable();
      checkpointIfNeeded();
      const previous = engine.exportSnapshot();
      try {
        engine.importSnapshot(snapshot);
        const next = engine.exportSnapshot();
        if (!bytesEqual(previous, next)) {
          (journalStore ?? store).commit(next);
        }
      } catch (error) {
        try {
          engine.importSnapshot(previous);
        } catch (rollbackError) {
          poison();
          throw new StorageError(
            'STORAGE_ROLLBACK_FAILED',
            `TinyGres could not restore memory after a failed snapshot import: ${errorMessage(rollbackError)}`,
          );
        }
        if (errorCode(error) === 'STORAGE_COMMIT_OUTCOME_UNKNOWN') {
          poison();
        }
        throw error;
      }
    },
    close(): void {
      let rollbackError: unknown;
      try {
        if (!closed && !poisoned && engine.inTransaction()) {
          engine.rollbackTransaction();
          transactionSnapshot = undefined;
          transactionStatements = undefined;
        }
      } catch (error) {
        rollbackError = error;
      }
      try {
        closeResources();
      } catch (error) {
        rollbackError ??= error;
      }
      if (rollbackError !== undefined) {
        throw rollbackError;
      }
    },
  };
}

function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  return (
    left.byteLength === right.byteLength &&
    left.every((byte, index) => right[index] === byte)
  );
}

function restoreNewestValidSnapshot(
  engine: WorkerEngine,
  store: SnapshotStore,
): boolean {
  for (const candidate of store.candidates()) {
    try {
      engine.importSnapshot(candidate.snapshot);
      replayCandidate(engine, candidate);
      store.select(candidate);
      return true;
    } catch (error) {
      const code = errorCode(error);
      if (code === 'UNSUPPORTED_SNAPSHOT') {
        throw new StorageError(
          'STORAGE_VERSION_UNSUPPORTED',
          'The TinyGres OPFS database uses an unsupported engine snapshot format',
        );
      }
      if (code !== 'INVALID_SNAPSHOT') {
        // Never fall back across a complete but corrupt journal. The older
        // pair may predate acknowledged records appended after compaction;
        // choosing it would turn detectable corruption into silent data loss.
        throw mapRecoveryError(error);
      }
      // A torn or incompatible newer slot must not hide an older valid one.
    }
  }
  if (store.hadData) {
    throw new StorageError(
      'STORAGE_CORRUPT',
      'TinyGres found OPFS snapshot data but no valid database state',
    );
  }
  return false;
}

function replayCandidate(
  engine: WorkerEngine,
  candidate: SnapshotCandidate,
): void {
  if (candidate.journalError) {
    throw candidate.journalError;
  }
  for (const record of candidate.journal?.records ?? []) {
    let transaction;
    try {
      transaction = decodeJournalTransaction(record.payload);
    } catch (error) {
      throw mapRecoveryError(error);
    }
    if (engine.revision() !== transaction.revisionBefore) {
      throw new StorageError(
        'STORAGE_JOURNAL_CORRUPT',
        `TinyGres journal transaction ${record.sequence} does not follow the checkpoint revision`,
      );
    }
    try {
      for (const mutation of transaction.mutations) {
        replayMutation(engine, mutation);
      }
    } catch (error) {
      throw new StorageError(
        'STORAGE_JOURNAL_CORRUPT',
        `TinyGres could not replay journal transaction ${record.sequence}: ${errorMessage(error)}`,
      );
    }
    if (engine.revision() !== transaction.revisionAfter) {
      throw new StorageError(
        'STORAGE_JOURNAL_CORRUPT',
        `TinyGres journal transaction ${record.sequence} produced an unexpected revision`,
      );
    }
  }
}

function replayMutation(engine: WorkerEngine, mutation: JournalMutation): void {
  switch (mutation.type) {
    case 'defineTables':
      engine.defineTables(mutation.schemas);
      return;
    case 'replaceTableSnapshot':
      engine.replaceTableSnapshot(mutation.schema, mutation.rows);
      return;
    case 'applyBatch':
      engine.applyBatch(mutation.batch);
      return;
    case 'executeSql':
      engine.beginTransaction();
      try {
        for (const statement of mutation.statements) {
          engine.executeSql(statement.sql, statement.params);
        }
        engine.commitTransaction();
      } catch (error) {
        if (engine.inTransaction()) {
          engine.rollbackTransaction();
        }
        throw error;
      }
  }
}

function mapRecoveryError(error: unknown): unknown {
  const code = errorCode(error);
  if (code === 'STORAGE_VERSION_UNSUPPORTED') {
    return error;
  }
  if (code === 'UNSUPPORTED_SNAPSHOT') {
    return new StorageError(
      'STORAGE_VERSION_UNSUPPORTED',
      'The TinyGres OPFS database uses an unsupported engine snapshot format',
    );
  }
  if (code?.startsWith('STORAGE_')) {
    return error;
  }
  return new StorageError(
    'STORAGE_JOURNAL_CORRUPT',
    `TinyGres could not decode its OPFS journal: ${errorMessage(error)}`,
  );
}

function isJournalStore(store: SnapshotStore): store is JournalSnapshotStore {
  return (
    typeof store.append === 'function' &&
    typeof store.checkpoint === 'function' &&
    typeof store.journalRecordCount === 'function' &&
    typeof store.journalByteLength === 'function'
  );
}

function errorCode(error: unknown): string | undefined {
  return typeof error === 'object' &&
    error !== null &&
    'code' in error &&
    typeof error.code === 'string'
    ? error.code
    : undefined;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
