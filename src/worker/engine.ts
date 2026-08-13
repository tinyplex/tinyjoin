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
import {MemoryPageDevice} from './page-device.js';
import {createBinaryWasmEngine} from './wasm-wire.js';

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

export type WorkerEngineFactory = (
  resolvedStorage: StorageOptions,
) => Promise<WorkerEngine>;

/** Opens the page-native default engine on an ephemeral in-memory page device. */
export async function createMemoryWasmEngine(): Promise<WorkerEngine> {
  const wasm = await import('../wasm/tinygres_wasm.js');
  await wasm.default();
  return createBinaryWasmEngine(wasm.WasmEngine, new MemoryPageDevice());
}
