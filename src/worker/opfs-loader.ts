import {
  createPageWasmEngine,
  type WorkerEngine,
} from './engine.js';

interface OpfsRuntimeModule {
  createOpfsWasmEngine(
    name: string,
    provider: undefined,
    dependencies: {
      createPageEngine: typeof createPageWasmEngine;
    },
  ): Promise<WorkerEngine>;
}

type OpfsRuntimeLoader = (url: string) => Promise<OpfsRuntimeModule>;

const runtimeUrl = new URL(
  '../worker-opfs/tinyjoin_opfs_runtime.js',
  import.meta.url,
);

/** Loads the private OPFS page-storage graph only for an explicit OPFS session. */
export async function createOpfsWasmEngine(
  name: string,
  loadRuntime: OpfsRuntimeLoader = importOpfsRuntime,
): Promise<WorkerEngine> {
  const runtime = await loadRuntime(runtimeUrl.href);
  return runtime.createOpfsWasmEngine(name, undefined, {
    createPageEngine: createPageWasmEngine,
  });
}

function importOpfsRuntime(url: string): Promise<OpfsRuntimeModule> {
  return import(
    /* @vite-ignore */ /* webpackIgnore: true */ url
  ) as Promise<OpfsRuntimeModule>;
}
