import type {JsonPrimitive} from '../../src/protocol.ts';

/**
 * The result a statement result's header stands for: what the header of the
 * result's JSON response would say. `columns` and `keys` are those of the
 * table that changed, and are absent where its keys are not reported.
 */
export type StatementHeader = {
  command: 'INSERT' | 'UPDATE' | 'DELETE' | 'SELECT';
  revision: number;
  rowCount: number;
  table?: string;
  columns?: string[];
  keys?: JsonPrimitive[][];
};

const COMMANDS = ['INSERT', 'UPDATE', 'DELETE', 'SELECT'] as const;
const FIXED_BYTES = 16;
const DECODED_TEXT = 0x8000;
const PLAIN_TEXT_BYTES = 64;

/**
 * Writes a header as the engine writes one, for a stand-in engine to answer
 * with: the inverse of the bridge's reader, as `decodeRequest` is the inverse
 * of its request writer.
 *
 * Its numbers are little-endian. It begins with the command's number plus
 * one, the number of columns in the changed table's key, the number of keys
 * as a u16, its own length as a u16, two bytes of zero, and the revision and
 * row count as u32s. The table's name follows, then its key's column names,
 * and then every key's values, each a tag and then a number's eight bytes as
 * a float or a string's text. A text is its length in bytes as a u16, with
 * the top bit set unless it is at most 64 bytes of ASCII, and then its UTF-8
 * bytes.
 */
export const encodeStatementHeader = (header: StatementHeader): Uint8Array => {
  const bytes: number[] = [];
  const u16 = (value: number): void => {
    bytes.push(value & 0xff, value >> 8);
  };
  const u32 = (value: number): void => {
    u16(value & 0xffff);
    u16(value >>> 16);
  };
  const f64 = (value: number): void => {
    const buffer = new Uint8Array(8);
    new DataView(buffer.buffer).setFloat64(0, value, true);
    bytes.push(...buffer);
  };
  const text = (value: string): void => {
    const encoded = new TextEncoder().encode(value);
    const plain =
      encoded.length <= PLAIN_TEXT_BYTES && encoded.every((byte) => byte < 0x80);
    u16(encoded.length | (plain ? 0 : DECODED_TEXT));
    bytes.push(...encoded);
  };
  const width = header.columns?.length ?? 0;
  bytes.push(COMMANDS.indexOf(header.command) + 1, width);
  u16(header.keys?.length ?? 0);
  u16(0);
  u16(0);
  u32(header.revision);
  u32(header.rowCount);
  if (header.table !== undefined) {
    text(header.table);
    header.columns?.forEach(text);
    for (const value of header.keys?.flat() ?? []) {
      if (value === null) {
        bytes.push(0);
      } else if (typeof value === 'boolean') {
        bytes.push(value ? 2 : 1);
      } else if (typeof value === 'number') {
        bytes.push(Number.isSafeInteger(value) && !Object.is(value, -0) ? 3 : 5);
        f64(value);
      } else {
        bytes.push(6);
        text(value);
      }
    }
  }
  bytes[4] = bytes.length & 0xff;
  bytes[5] = bytes.length >> 8;
  return Uint8Array.from(bytes);
};

/**
 * Reads the header at the start of `bytes`, strictly: anything the engine
 * does not write throws. It is written apart from the bridge's reader, so
 * that the two can be held against each other.
 */
export const decodeStatementHeader = (bytes: Uint8Array): StatementHeader => {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const decoder = new TextDecoder('utf-8', {fatal: true, ignoreBOM: true});
  const fail = (what: string): never => {
    throw new Error(`A statement header has an invalid ${what}`);
  };
  const command = COMMANDS[view.getUint8(0) - 1] ?? fail('command');
  const width = view.getUint8(1);
  const count = view.getUint16(2, true);
  const length = view.getUint16(4, true);
  if (view.getUint16(6, true) !== 0) fail('unused word');
  const revision = view.getUint32(8, true);
  const rowCount = view.getUint32(12, true);
  let at = FIXED_BYTES;
  const text = (): string => {
    const head = view.getUint16(at, true);
    const start = at + 2;
    at = start + (head & ~DECODED_TEXT);
    if (at > length) fail('text length');
    const encoded = bytes.subarray(start, at);
    const plain =
      encoded.length <= PLAIN_TEXT_BYTES && encoded.every((byte) => byte < 0x80);
    if (plain === head >= DECODED_TEXT) fail('text mark');
    return decoder.decode(encoded);
  };
  const header: StatementHeader = {command, revision, rowCount};
  if (length > FIXED_BYTES) {
    if (command === 'SELECT') fail('read');
    header.table = text();
    if (width > 0) {
      header.columns = Array.from({length: width}, text);
      header.keys = Array.from({length: count}, () =>
        Array.from({length: width}, (): JsonPrimitive => {
          const tag = view.getUint8(at++);
          switch (tag) {
            case 0:
              return null;
            case 1:
              return false;
            case 2:
              return true;
            case 3:
            case 5: {
              const value = view.getFloat64(at, true);
              at += 8;
              if (!Number.isFinite(value)) fail('number');
              // An integer is tagged as one only when a response can carry
              // it exactly; a float may hold any finite number.
              if (
                tag === 3 &&
                !(Number.isSafeInteger(value) && !Object.is(value, -0))
              ) {
                fail('number tag');
              }
              return value;
            }
            case 6:
              return text();
            default:
              return fail('value tag');
          }
        }),
      );
    } else if (count !== 0) {
      fail('key count');
    }
  } else if (length !== FIXED_BYTES || width !== 0 || count !== 0) {
    fail('length');
  }
  if (at !== length) fail('end');
  return header;
};

/**
 * The result a header stands for as the array the bridge returns for it, with
 * its first two slots left for whoever posts it. `data` is a read's fields
 * and rows.
 */
export const statementResult = (
  header: StatementHeader,
  data?: string,
): JsonPrimitive[] => {
  const result: JsonPrimitive[] = [
    0,
    0,
    COMMANDS.indexOf(header.command),
    header.revision,
    header.rowCount,
  ];
  if (header.command === 'SELECT') {
    result.push(data ?? '');
  } else if (header.table !== undefined) {
    result.push(header.table);
    if (header.columns) {
      result.push(header.columns.length, ...header.columns, ...(header.keys ?? []).flat());
    }
  }
  return result;
};
