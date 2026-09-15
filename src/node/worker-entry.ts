import {readFile} from 'node:fs/promises';
import {parentPort} from 'node:worker_threads';
import {startWorker} from '../worker/host.js';
import {createMemoryPageDevice} from '../worker/page-device.js';
import {createStructuredWasmEngine} from '../worker/wasm-bridge.js';
import init, {WasmEngine} from '../wasm/tinyjoin_wasm.js';

if (!parentPort) throw new Error('TinyJoin must start in a Node.js worker thread');
const port = parentPort;

startWorker({
  scope: {
    postMessage: (message) => port.postMessage(message),
    addEventListener: (type, listener) =>
      port.addEventListener(type, listener as EventListener),
    removeEventListener: (type, listener) =>
      port.removeEventListener(type, listener as EventListener),
    close: () => port.close(),
  },
  durableEngineFactory: async (storage) => {
    if (storage.kind !== 'memory') {
      throw new TypeError('TinyJoin in Node.js supports only memory:// databases');
    }
    await init({
      module_or_path: await readFile(
        new URL('../wasm/tinyjoin_wasm_bg.wasm', import.meta.url),
      ),
    });
    return createStructuredWasmEngine(WasmEngine, createMemoryPageDevice());
  },
});
