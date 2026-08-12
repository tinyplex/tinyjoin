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
  StorageError,
  type SnapshotStore,
} from './snapshot-store.js';

export function createPersistentEngine(
  engine: WorkerEngine,
  store: SnapshotStore,
): WorkerEngine {
  restoreNewestValidSnapshot(engine, store);
  let closed = false;
  let poisoned = false;
  let transactionSnapshot: Uint8Array | undefined;

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

  const mutate = <Result>(operation: () => Result): Result => {
    assertUsable();
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

  return {
    defineTable(schema: TableSchema): void {
      mutate(() => engine.defineTable(schema));
    },
    defineTables(schemas: TableSchema[]): void {
      mutate(() => engine.defineTables(schemas));
    },
    replaceTableSnapshot(
      schema: TableSchema,
      rows: Row[],
    ): ApplyOutcome {
      return mutate(() => engine.replaceTableSnapshot(schema, rows));
    },
    applyBatch(batch: ChangeBatch): ApplyOutcome {
      return mutate(() => engine.applyBatch(batch));
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
      return engine.inTransaction()
        ? engine.executeSql(sql, params)
        : mutate(() => engine.executeSql(sql, params));
    },
    beginTransaction(): void {
      assertUsable();
      const previous = engine.exportSnapshot();
      engine.beginTransaction();
      transactionSnapshot = previous;
    },
    commitTransaction(): ApplyOutcome {
      assertUsable();
      if (!transactionSnapshot) {
        return engine.commitTransaction();
      }
      const previous = transactionSnapshot;
      try {
        const result = engine.commitTransaction();
        const next = engine.exportSnapshot();
        if (!bytesEqual(previous, next)) {
          store.commit(next);
        }
        transactionSnapshot = undefined;
        return result;
      } catch (error) {
        transactionSnapshot = undefined;
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
      mutate(() => engine.importSnapshot(snapshot));
    },
    close(): void {
      let rollbackError: unknown;
      try {
        if (!closed && !poisoned && engine.inTransaction()) {
          engine.rollbackTransaction();
          transactionSnapshot = undefined;
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
): void {
  for (const candidate of store.candidates()) {
    try {
      engine.importSnapshot(candidate.snapshot);
      store.select(candidate);
      return;
    } catch (error) {
      const code = errorCode(error);
      if (code === 'UNSUPPORTED_SNAPSHOT') {
        throw new StorageError(
          'STORAGE_VERSION_UNSUPPORTED',
          'The TinyGres OPFS database uses an unsupported engine snapshot format',
        );
      }
      if (code !== 'INVALID_SNAPSHOT') {
        throw error;
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
