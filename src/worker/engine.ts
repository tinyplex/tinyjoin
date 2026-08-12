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
  exportSnapshot(): Uint8Array;
  importSnapshot(snapshot: Uint8Array): void;
  close?(): void;
}

export async function createWasmEngine(): Promise<WorkerEngine> {
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
