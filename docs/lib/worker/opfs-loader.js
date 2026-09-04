import { createPageWasmEngine, } from './engine.js';
const runtimeUrl = new URL('../worker-opfs/tinyjoin_opfs_runtime.js', import.meta.url);
/** Loads the private OPFS page-storage graph only for an explicit OPFS session. */
export async function createOpfsWasmEngine(name, loadRuntime = importOpfsRuntime) {
    const runtime = await loadRuntime(runtimeUrl.href);
    return runtime.createOpfsWasmEngine(name, undefined, {
        createPageEngine: createPageWasmEngine,
    });
}
function importOpfsRuntime(url) {
    return import(
    /* @vite-ignore */ /* webpackIgnore: true */ url);
}
