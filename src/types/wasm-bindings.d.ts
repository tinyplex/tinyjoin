declare module '*tinyjoin_wasm.js' {
  export default function init(
    options?: {module_or_path: Uint8Array | WebAssembly.Module},
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
