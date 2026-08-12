declare module '*tinygres_wasm.js' {
  export default function init(
    moduleOrPath?: WebAssembly.Module | RequestInfo | URL | Response,
  ): Promise<unknown>;

  export class WasmEngine {
    constructor();
    free(): void;
    define_table(schema: unknown): void;
    replace_table_snapshot(
      schema: unknown,
      rows: unknown,
    ): import('../protocol.js').ApplyOutcome;
    apply_batch(batch: unknown): import('../protocol.js').ApplyOutcome;
    query(plan: unknown): import('../protocol.js').QueryResult;
    query_sql(
      sql: string,
      params: unknown,
    ): import('../protocol.js').QueryResult;
    execute_sql(
      sql: string,
      params: unknown,
    ): import('../protocol.js').SqlResult;
    begin_transaction(): void;
    commit_transaction(): import('../protocol.js').ApplyOutcome;
    rollback_transaction(): void;
    in_transaction(): boolean;
    revision(): bigint;
    export_snapshot(): Uint8Array;
    import_snapshot(snapshot: Uint8Array): void;
  }
}
