import { MemoryPageDevice } from './page-device.js';
import { createStructuredWasmEngine } from './wasm-bridge.js';
/** Opens the page-native default engine on an ephemeral in-memory page device. */
export async function createMemoryWasmEngine() {
    return createPageWasmEngine(new MemoryPageDevice());
}
/** Opens the page-native engine on a caller-owned device transferred to WASM. */
export async function createPageWasmEngine(device) {
    const wasm = await import('../wasm/tinyjoin_wasm.js');
    await wasm.default();
    return createStructuredWasmEngine(wasm.WasmEngine, device);
}
