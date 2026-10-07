import type {
  ApplyOutcome,
  JsonValue,
  RowMode,
  Schema,
  SqlResult,
  StatementResult,
  StorageOptions,
} from '../protocol.js';
import {createMemoryPageDevice, type PageDevice} from './page-device.js';
import {createStructuredWasmEngine} from './wasm-bridge.js';

export interface WorkerEngine {
  /**
   * Executes one statement. A result that published nothing comes back as a
   * {@link StatementResult}, the flat array its response is, where it has the
   * shape of one; any other result comes back as a {@link SqlResult}.
   *
   * `from` is where the statement's parameters begin in `params`, so that a
   * statement request's parameters are read where they arrived, after its
   * fixed slots, rather than copied out first.
   */
  executeSql(
    sql: string,
    params: readonly JsonValue[],
    rowMode?: RowMode,
    from?: number,
  ): SqlResult | StatementResult;
  prepareSql(sql: string): number;
  /** Executes a prepared statement, as {@link WorkerEngine.executeSql} executes one. */
  executePrepared(
    statementId: number,
    params: readonly JsonValue[],
    rowMode?: RowMode,
    from?: number,
  ): SqlResult | StatementResult;
  closePrepared(statementId: number): void;
  execSql(sql: string, rowMode?: RowMode): SqlResult[];
  beginTransaction(): void;
  commitTransaction(): ApplyOutcome;
  rollbackTransaction(): void;
  inTransaction(): boolean;
  revision(): number;
  check(): void;
  schema(): Schema;
  setSchema(schema: Schema, drop: boolean): ApplyOutcome;
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
