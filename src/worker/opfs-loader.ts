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
export const createOpfsWasmEngine = async (
  name: string,
  loadRuntime: OpfsRuntimeLoader = importOpfsRuntime,
): Promise<WorkerEngine> => {
  const runtime = await loadRuntime(runtimeUrl.href);
  return runtime.createOpfsWasmEngine(name, undefined, {
    createPageEngine: createPageWasmEngine,
  });
};

const importOpfsRuntime = (url: string): Promise<OpfsRuntimeModule> =>
  import(
    /* @vite-ignore */ /* webpackIgnore: true */ url
  ) as Promise<OpfsRuntimeModule>;
