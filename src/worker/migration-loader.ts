import type {WorkerEngine} from './engine.js';

interface MigrationRuntimeModule {
  createMigrationConfiguredEngine(
    name: string,
    glueUrl: string,
    wasmUrl: string,
  ): Promise<WorkerEngine>;
}

type MigrationRuntimeLoader = (
  url: string,
) => Promise<MigrationRuntimeModule>;

const runtimeUrl = new URL(
  '../worker-migration/tinygres_migration_runtime.js',
  import.meta.url,
);
const glueUrl = new URL(
  '../wasm-migration/tinygres_migration_wasm.js',
  import.meta.url,
);
const wasmUrl = new URL(
  '../wasm-migration/tinygres_migration_wasm_bg.wasm',
  import.meta.url,
);

/** Loads no legacy code until OPFS is explicitly selected. */
export async function createOpfsMigrationEngine(
  name: string,
  loadRuntime: MigrationRuntimeLoader = importMigrationRuntime,
): Promise<WorkerEngine> {
  const runtime = await loadRuntime(runtimeUrl.href);
  return runtime.createMigrationConfiguredEngine(
    name,
    glueUrl.href,
    wasmUrl.href,
  );
}

function importMigrationRuntime(url: string): Promise<MigrationRuntimeModule> {
  return import(
    /* @vite-ignore */ /* webpackIgnore: true */ url
  ) as Promise<MigrationRuntimeModule>;
}
