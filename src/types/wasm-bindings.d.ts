declare module '*tinygres_wasm.js' {
  export default function init(
    moduleOrPath?: WebAssembly.Module | RequestInfo | URL | Response,
  ): Promise<unknown>;

  export class WasmEngine {
    constructor();
    free(): void;
    define_table(schema: unknown): void;
    replace_table(
      table: string,
      rows: unknown,
    ): import('../protocol.js').ApplyOutcome;
    apply_batch(batch: unknown): import('../protocol.js').ApplyOutcome;
    query(plan: unknown): import('../protocol.js').QueryResult;
    query_sql(
      sql: string,
      params: unknown,
    ): import('../protocol.js').QueryResult;
    revision(): bigint;
  }
}
