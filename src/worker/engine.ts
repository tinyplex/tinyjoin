import type {
  ApplyOutcome,
  ChangeBatch,
  JsonValue,
  QueryPlan,
  QueryResult,
  Row,
  TableSchema,
} from '../protocol.js';

export interface WorkerEngine {
  defineTable(schema: TableSchema): void;
  replaceTable(table: string, rows: Row[]): ApplyOutcome;
  applyBatch(batch: ChangeBatch): ApplyOutcome;
  query(plan: QueryPlan): QueryResult;
  querySql(sql: string, params: JsonValue[]): QueryResult;
  revision(): number;
  close?(): void;
}

export async function createWasmEngine(): Promise<WorkerEngine> {
  const wasm = await import('../generated/wasm/tinygres_wasm.js');
  await wasm.default();
  const engine = new wasm.WasmEngine();
  return {
    defineTable: (schema) => engine.define_table(schema),
    replaceTable: (table, rows) => engine.replace_table(table, rows),
    applyBatch: (batch) => engine.apply_batch(batch),
    query: (plan) => engine.query(plan),
    querySql: (sql, params) => engine.query_sql(sql, params),
    revision: () => Number(engine.revision()),
    close: () => engine.free(),
  };
}
