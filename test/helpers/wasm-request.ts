import type {JsonValue} from '../../src/protocol.ts';
import {WASM_OPERATION} from '../../src/worker/wasm-bridge.ts';

/**
 * Reads a request the bridge wrote for WASM back into the arguments it was
 * written from, as WASM reads it: a statement's `{sql, params, arrayRows}`,
 * `{statementId, params, arrayRows}`, or `{sql, arrayRows}`, where `arrayRows`
 * appears only when set, the text to prepare, the statement to close, or
 * `undefined` for an operation that takes no arguments.
 */
export const decodeRequest = (
  operation: number,
  bytes: Uint8Array,
): unknown => {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const decoder = new TextDecoder('utf-8', {fatal: true});
  let at = 0;
  const u8 = (): number => view.getUint8(at++);
  const u32 = (): number => {
    const value = view.getUint32(at, true);
    at += 4;
    return value;
  };
  const f64 = (): number => {
    const value = view.getFloat64(at, true);
    at += 8;
    return value;
  };
  const string = (): string => {
    const length = u32();
    const text = decoder.decode(bytes.subarray(at, at + length));
    at += length;
    return text;
  };
  const value = (): JsonValue => {
    const tag = u8();
    switch (tag) {
      case 0:
        return null;
      case 1:
        return false;
      case 2:
        return true;
      case 3:
      case 5:
        return f64();
      case 6:
        return string();
      case 7:
        return values();
      case 8: {
        const object: Record<string, JsonValue> = {};
        for (let count = u32(); count > 0; count -= 1) {
          // Defined rather than assigned, so that a `__proto__` key stays data.
          Object.defineProperty(object, string(), {
            value: value(),
            enumerable: true,
            writable: true,
            configurable: true,
          });
        }
        return object;
      }
      default:
        throw new Error(`Unknown request value tag ${tag}`);
    }
  };
  const values = (): JsonValue[] => {
    const list: JsonValue[] = [];
    for (let count = u32(); count > 0; count -= 1) list.push(value());
    return list;
  };
  const arrayRows = (): {arrayRows?: true} => (u8() === 1 ? {arrayRows: true} : {});

  let decoded: unknown;
  switch (operation) {
    case WASM_OPERATION.executeSql: {
      const rows = arrayRows();
      decoded = {sql: string(), params: values(), ...rows};
      break;
    }
    case WASM_OPERATION.executePrepared: {
      const rows = arrayRows();
      decoded = {statementId: u32(), params: values(), ...rows};
      break;
    }
    case WASM_OPERATION.execSql: {
      const rows = arrayRows();
      decoded = {sql: string(), ...rows};
      break;
    }
    case WASM_OPERATION.prepareSql:
      decoded = string();
      break;
    case WASM_OPERATION.closePrepared:
      decoded = u32();
      break;
    case WASM_OPERATION.setSchema: {
      const drop = u8() === 1;
      decoded = {schema: values()[0], drop};
      break;
    }
    default:
      decoded = undefined;
  }
  if (at !== bytes.byteLength) {
    throw new Error(`A request for operation ${operation} has trailing bytes`);
  }
  return decoded;
};
