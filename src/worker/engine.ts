import type {
  ApplyOutcome,
  JsonValue,
  SqlResult,
  StorageOptions,
} from '../protocol.js';
import {createMemoryPageDevice, type PageDevice} from './page-device.js';
import {createStructuredWasmEngine} from './wasm-bridge.js';

export interface WorkerEngine {
  executeSql(sql: string, params: JsonValue[]): SqlResult;
  prepareSql(sql: string): number;
  executePrepared(statementId: number, params: JsonValue[]): SqlResult;
  closePrepared(statementId: number): void;
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
export const createMemoryWasmEngine = (): Promise<WorkerEngine> =>
  createPageWasmEngine(createMemoryPageDevice());

/** Opens the page-native engine on a caller-owned device transferred to WASM. */
export const createPageWasmEngine = async (
  device: PageDevice,
): Promise<WorkerEngine> => {
  const wasm = await import('../wasm/tinyjoin_wasm.js');
  await wasm.default();
  return createStructuredWasmEngine(wasm.WasmEngine, device);
};
