declare module '*tinyjoin_wasm.js' {
  export default function init(
    moduleOrPath?: WebAssembly.Module | RequestInfo | URL | Response,
  ): Promise<unknown>;

  export class WasmEngine {
    constructor(device: import('../worker/page-device.js').PageDevice);
    callStructured(
      bridgeVersion: number,
      operation: number,
      payload: unknown,
    ): unknown;
    free(): void;
  }
}
