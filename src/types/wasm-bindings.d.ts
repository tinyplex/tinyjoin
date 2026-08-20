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
