import type {
  ApplyOutcome,
  ChangeBatch,
  JsonValue,
  Row,
  SqlResult,
  TableSchema,
} from '../protocol.js';
import type {WorkerEngine} from './engine.js';

export interface PreparedMutation<Result> {
  result: Result;
  /** Opaque Rust-owned bytes. `null` means the operation changed no state. */
  commit: Uint8Array | null;
}

/** Recovery-only capabilities retained by the temporary OPFS migration path. */
export interface LegacyRecoveryEngine extends WorkerEngine {
  prepareDefineTables(schemas: TableSchema[]): PreparedMutation<null>;
  prepareReplaceTableSnapshot(
    schema: TableSchema,
    rows: Row[],
  ): PreparedMutation<ApplyOutcome>;
  prepareApplyBatch(batch: ChangeBatch): PreparedMutation<ApplyOutcome>;
  prepareExecuteSql(
    sql: string,
    params: JsonValue[],
  ): PreparedMutation<SqlResult>;
  prepareCommitTransaction(): PreparedMutation<ApplyOutcome>;
  installPreparedCommit(commit: Uint8Array): ApplyOutcome;
  abortPreparedCommit(): void;
  replayCommit(commit: Uint8Array): ApplyOutcome;
  exportSnapshot(): Uint8Array;
  importSnapshot(snapshot: Uint8Array): void;
}

interface MigrationWasmModule {
  default(options: {
    module_or_path: string | URL;
  }): Promise<unknown>;
  WasmEngine: new () => MigrationRawEngine;
}

type MigrationRawEngine = InstanceType<
  typeof import('../wasm-migration/tinygres_migration_wasm.js').WasmEngine
>;

/** Opens the temporary legacy runtime which currently owns OPFS stores. */
export async function createMigrationWasmEngine(
  glueUrl: string,
  wasmUrl: string,
): Promise<LegacyRecoveryEngine> {
  const wasm = (await import(
    /* @vite-ignore */ /* webpackIgnore: true */ glueUrl
  )) as MigrationWasmModule;
  await wasm.default({module_or_path: wasmUrl});
  const engine = new wasm.WasmEngine();
  return {
    defineTable: (schema) => engine.define_table(schema),
    defineTables: (schemas) => {
      for (const schema of schemas) {
        engine.define_table(schema);
      }
    },
    replaceTableSnapshot: (schema, rows) =>
      engine.replace_table_snapshot(schema, rows),
    applyBatch: (batch) => engine.apply_batch(batch),
    query: (plan) => engine.query(plan),
    querySql: (sql, params) => engine.query_sql(sql, params),
    executeSql: (sql, params) => engine.execute_sql(sql, params),
    prepareDefineTables: (schemas) =>
      normalizePreparedMutation(engine.prepare_define_tables(schemas)),
    prepareReplaceTableSnapshot: (schema, rows) =>
      normalizePreparedMutation(
        engine.prepare_replace_table_snapshot(schema, rows),
      ),
    prepareApplyBatch: (batch) =>
      normalizePreparedMutation(engine.prepare_apply_batch(batch)),
    prepareExecuteSql: (sql, params) =>
      normalizePreparedMutation(engine.prepare_execute_sql(sql, params)),
    prepareCommitTransaction: () =>
      normalizePreparedMutation(engine.prepare_commit_transaction()),
    installPreparedCommit: (commit) => engine.install_prepared_commit(commit),
    abortPreparedCommit: () => engine.abort_prepared_commit(),
    replayCommit: (commit) => engine.replay_commit(commit),
    beginTransaction: () => engine.begin_transaction(),
    commitTransaction: () => engine.commit_transaction(),
    rollbackTransaction: () => engine.rollback_transaction(),
    inTransaction: () => engine.in_transaction(),
    revision: () => Number(engine.revision()),
    exportSnapshot: () => engine.export_snapshot(),
    importSnapshot: (snapshot) => engine.import_snapshot(snapshot),
    close: () => engine.free(),
  };
}

function normalizePreparedMutation<Result>(value: {
  result: Result;
  commit: Uint8Array | number[] | null;
}): PreparedMutation<Result> {
  return {
    result: value.result,
    commit:
      value.commit === null
        ? null
        : value.commit instanceof Uint8Array
          ? value.commit.slice()
          : new Uint8Array(value.commit),
  };
}
