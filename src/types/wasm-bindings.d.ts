declare module '*tinygres_wasm.js' {
  export default function init(
    moduleOrPath?: WebAssembly.Module | RequestInfo | URL | Response,
  ): Promise<unknown>;

  export class WasmEngine {
    constructor(device: import('../worker/page-device.js').PageDevice);
    call(operation: number, payload: Uint8Array): Uint8Array;
    free(): void;
  }
}

declare module '*tinygres_migration_wasm.js' {
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
    prepare_define_tables(schemas: unknown): PreparedMutation<null>;
    prepare_replace_table_snapshot(
      schema: unknown,
      rows: unknown,
    ): PreparedMutation<import('../protocol.js').ApplyOutcome>;
    prepare_apply_batch(
      batch: unknown,
    ): PreparedMutation<import('../protocol.js').ApplyOutcome>;
    prepare_execute_sql(
      sql: string,
      params: unknown,
    ): PreparedMutation<import('../protocol.js').SqlResult>;
    prepare_commit_transaction(): PreparedMutation<
      import('../protocol.js').ApplyOutcome
    >;
    install_prepared_commit(
      commit: Uint8Array,
    ): import('../protocol.js').ApplyOutcome;
    abort_prepared_commit(): void;
    replay_commit(commit: Uint8Array): import('../protocol.js').ApplyOutcome;
    begin_transaction(): void;
    commit_transaction(): import('../protocol.js').ApplyOutcome;
    rollback_transaction(): void;
    in_transaction(): boolean;
    revision(): bigint;
    export_snapshot(): Uint8Array;
    import_snapshot(snapshot: Uint8Array): void;
  }

  interface PreparedMutation<Result> {
    result: Result;
    commit: Uint8Array | number[] | null;
  }
}
