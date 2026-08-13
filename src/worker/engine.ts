import type {
  ApplyOutcome,
  ChangeBatch,
  JsonValue,
  QueryPlan,
  QueryResult,
  Row,
  SqlResult,
  StorageOptions,
  TableSchema,
} from '../protocol.js';

export interface PreparedMutation<Result> {
  result: Result;
  /** Opaque Rust-owned bytes. `null` means the operation changed no state. */
  commit: Uint8Array | null;
}

export interface WorkerEngine {
  defineTable(schema: TableSchema): void;
  defineTables(schemas: TableSchema[]): void;
  replaceTableSnapshot(schema: TableSchema, rows: Row[]): ApplyOutcome;
  applyBatch(batch: ChangeBatch): ApplyOutcome;
  query(plan: QueryPlan): QueryResult;
  querySql(sql: string, params: JsonValue[]): QueryResult;
  executeSql(sql: string, params: JsonValue[]): SqlResult;
  beginTransaction(): void;
  commitTransaction(): ApplyOutcome;
  rollbackTransaction(): void;
  inTransaction(): boolean;
  revision(): number;
  close(): void;
}

/** Recovery-only capabilities retained by the temporary legacy persistence path. */
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

export type WorkerEngineFactory = (
  resolvedStorage: StorageOptions,
) => Promise<WorkerEngine>;

export async function createWasmEngine(
  _resolvedStorage: StorageOptions,
): Promise<LegacyRecoveryEngine> {
  const wasm = await import('../wasm/tinygres_wasm.js');
  await wasm.default();
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
