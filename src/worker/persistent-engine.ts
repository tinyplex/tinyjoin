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
import type {LegacyRecoveryEngine, PreparedMutation} from './engine.js';
import {
  decodeJournalTransaction,
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
  engine: LegacyRecoveryEngine,
  store: SnapshotStore,
): LegacyRecoveryEngine {
  const journalStore = isJournalStore(store) ? store : undefined;
  const recovery = restoreNewestValidSnapshot(engine, store);
  if (journalStore) {
    if (!recovery.restored || recovery.replayedLegacyJournal) {
      // A checkpoint after legacy replay makes all later recovery independent
      // of historical SQL parser behavior. Alternating slots keep migration
      // crash-safe: the legacy pair remains valid until this publishes.
      journalStore.checkpoint(engine.exportSnapshot());
    }
  }
  let closed = false;
  let poisoned = false;

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
      engine.close();
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

  const mutateSnapshot = <Result>(operation: () => Result): Result => {
    assertUsable();
    checkpointIfNeeded();
    const previous = engine.exportSnapshot();
    try {
      const result = operation();
      const next = engine.exportSnapshot();
      if (!bytesEqual(previous, next)) {
        store.commit(next);
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

  const mutatePrepared = <Result>(
    prepare: () => PreparedMutation<Result>,
  ): Result => {
    assertUsable();
    checkpointIfNeeded();
    const prepared = prepare();
    if (prepared.commit === null) {
      return prepared.result;
    }
    let abortFailed = false;

    try {
      journalStore!.append(prepared.commit);
    } catch (error) {
      try {
        engine.abortPreparedCommit();
      } catch {
        abortFailed = true;
        poison();
      }
      if (errorCode(error) === 'STORAGE_COMMIT_OUTCOME_UNKNOWN') {
        poison();
      }
      if (abortFailed) {
        throw new StorageError(
          'STORAGE_ROLLBACK_FAILED',
          `TinyGres could not discard a prepared mutation after storage rejected it: ${errorMessage(error)}`,
        );
      }
      throw error;
    }

    try {
      engine.installPreparedCommit(prepared.commit);
    } catch (error) {
      // The append and its independent marker are already durable. Rolling
      // memory back, retrying, or acknowledging would all be dishonest. A
      // reopen will replay the exact canonical bytes that reached storage.
      poison();
      throw new StorageError(
        'STORAGE_COMMIT_DURABLE_REOPEN_REQUIRED',
        `TinyGres durably stored a commit but could not publish it in memory; reopen the database to recover it, and do not retry until its effects have been inspected: ${errorMessage(error)}`,
      );
    }
    return prepared.result;
  };

  return {
    defineTable(schema: TableSchema): void {
      if (journalStore) {
        mutatePrepared(() => engine.prepareDefineTables([schema]));
      } else {
        mutateSnapshot(() => engine.defineTable(schema));
      }
    },
    defineTables(schemas: TableSchema[]): void {
      if (schemas.length === 0) {
        assertUsable();
        engine.defineTables(schemas);
        return;
      }
      if (journalStore) {
        mutatePrepared(() => engine.prepareDefineTables(schemas));
      } else {
        mutateSnapshot(() => engine.defineTables(schemas));
      }
    },
    replaceTableSnapshot(schema: TableSchema, rows: Row[]): ApplyOutcome {
      return journalStore
        ? mutatePrepared(() => engine.prepareReplaceTableSnapshot(schema, rows))
        : mutateSnapshot(() => engine.replaceTableSnapshot(schema, rows));
    },
    applyBatch(batch: ChangeBatch): ApplyOutcome {
      return journalStore
        ? mutatePrepared(() => engine.prepareApplyBatch(batch))
        : mutateSnapshot(() => engine.applyBatch(batch));
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
        return engine.executeSql(sql, params);
      }
      return journalStore
        ? mutatePrepared(() => engine.prepareExecuteSql(sql, params))
        : mutateSnapshot(() => engine.executeSql(sql, params));
    },
    prepareDefineTables(schemas) {
      assertUsable();
      return engine.prepareDefineTables(schemas);
    },
    prepareReplaceTableSnapshot(schema, rows) {
      assertUsable();
      return engine.prepareReplaceTableSnapshot(schema, rows);
    },
    prepareApplyBatch(batch) {
      assertUsable();
      return engine.prepareApplyBatch(batch);
    },
    prepareExecuteSql(sql, params) {
      assertUsable();
      return engine.prepareExecuteSql(sql, params);
    },
    prepareCommitTransaction() {
      assertUsable();
      return engine.prepareCommitTransaction();
    },
    installPreparedCommit(commit) {
      assertUsable();
      return engine.installPreparedCommit(commit);
    },
    abortPreparedCommit() {
      assertUsable();
      engine.abortPreparedCommit();
    },
    replayCommit(commit) {
      assertUsable();
      return engine.replayCommit(commit);
    },
    beginTransaction(): void {
      assertUsable();
      checkpointIfNeeded();
      engine.beginTransaction();
    },
    commitTransaction(): ApplyOutcome {
      assertUsable();
      return journalStore
        ? mutatePrepared(() => engine.prepareCommitTransaction())
        : mutateSnapshot(() => engine.commitTransaction());
    },
    rollbackTransaction(): void {
      assertUsable();
      engine.rollbackTransaction();
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
  engine: LegacyRecoveryEngine,
  store: SnapshotStore,
): {restored: boolean; replayedLegacyJournal: boolean} {
  for (const candidate of store.candidates()) {
    try {
      engine.importSnapshot(candidate.snapshot);
      const replayedLegacyJournal = replayCandidate(engine, candidate);
      store.select(candidate);
      return {restored: true, replayedLegacyJournal};
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
  return {restored: false, replayedLegacyJournal: false};
}

function replayCandidate(
  engine: LegacyRecoveryEngine,
  candidate: SnapshotCandidate,
): boolean {
  if (candidate.journalError) {
    throw candidate.journalError;
  }
  let replayedLegacyJournal = false;
  for (const record of candidate.journal?.records ?? []) {
    if (record.kind === 'prepared-commit') {
      try {
        engine.replayCommit(record.payload);
      } catch (error) {
        if (errorCode(error) === 'UNSUPPORTED_PREPARED_COMMIT') {
          throw new StorageError(
            'STORAGE_VERSION_UNSUPPORTED',
            `TinyGres journal transaction ${record.sequence} uses an unsupported prepared commit format`,
          );
        }
        throw new StorageError(
          'STORAGE_JOURNAL_CORRUPT',
          `TinyGres could not replay journal transaction ${record.sequence}: ${errorMessage(error)}`,
        );
      }
      continue;
    }
    replayedLegacyJournal = true;
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
  return replayedLegacyJournal;
}

function replayMutation(
  engine: LegacyRecoveryEngine,
  mutation: JournalMutation,
): void {
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
