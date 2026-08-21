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
import {MemoryPageDevice, type PageDevice} from './page-device.js';
import {createBinaryWasmEngine} from './wasm-wire.js';

export interface WorkerEngine {
  defineTable(schema: TableSchema): void;
  defineTables(schemas: TableSchema[]): void;
  replaceTableSnapshot(schema: TableSchema, rows: Row[]): ApplyOutcome;
  applyBatch(batch: ChangeBatch): ApplyOutcome;
  query(plan: QueryPlan): QueryResult;
  executeSql(sql: string, params: JsonValue[]): SqlResult;
  execSql(sql: string): SqlResult[];
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
  return createPageWasmEngine(new MemoryPageDevice());
}

/** Opens the page-native engine on a caller-owned device transferred to WASM. */
export async function createPageWasmEngine(
  device: PageDevice,
): Promise<WorkerEngine> {
  const wasm = await import('../wasm/tinygres_wasm.js');
  await wasm.default();
  return createBinaryWasmEngine(wasm.WasmEngine, device);
}
