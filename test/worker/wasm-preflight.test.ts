import {describe, expect, it} from 'vitest';

import type {JsonValue} from '../../src/protocol.ts';
import {WASM_OPERATION} from '../../src/worker/wasm-bridge.ts';
import {
  encodeClosePrepared,
  encodeExecSql,
  encodePrepareSql,
  encodeSetSchema,
  encodeStatement,
} from '../../src/worker/wasm-preflight.ts';
import * as oracle from '../helpers/wasm-preflight-oracle.ts';
import {decodeRequest} from '../helpers/wasm-request.ts';

// One function writes both kinds of statement: a statement as text, and one
// prepared, as the bridge asks for each.
function encodeExecuteSql(
  sql: string,
  params: readonly JsonValue[],
  arrayRows: boolean,
  from?: number,
): Uint8Array {
  return encodeStatement(false, sql, params, arrayRows, from);
}

function encodeExecutePrepared(
  statementId: number,
  params: readonly JsonValue[],
  arrayRows: boolean,
  from?: number,
): Uint8Array {
  return encodeStatement(true, statementId, params, arrayRows, from);
}

// What writing a request comes to: a copy of its bytes, since the next
// request is written over them, or the code and message it failed with.
function outcome(write: () => Uint8Array): Uint8Array | string {
  try {
    return write().slice();
  } catch (error) {
    const {code, message} = error as {code?: string; message?: string};
    return `${code}: ${message}`;
  }
}

// Whether two outcomes are the same: byte for byte, or message for message.
// Long requests are compared as buffers, which takes no time to speak of.
function same(left: Uint8Array | string, right: Uint8Array | string): boolean {
  return typeof left === 'string' || typeof right === 'string'
    ? left === right
    : Buffer.from(left.buffer, left.byteOffset, left.length).equals(right);
}

// The fixed slots a statement request has before its parameters.
const LEAD: JsonValue[] = [12, 7, 4, 3, 'tx-1', 0];

describe('WASM request writing', () => {
  it('writes every SQL-first request as WASM reads it', () => {
    const params = [
      -0,
      1.5,
      9_007_199_254_740_993,
      {nested: [true, null, 'é😀'], ['__proto__']: 'data'},
    ] satisfies JsonValue[];
    expect(
      decodeRequest(
        WASM_OPERATION.executeSql,
        encodeExecuteSql('SELECT $1, $2', params, false),
      ),
    ).toEqual({sql: 'SELECT $1, $2', params});
    expect(
      decodeRequest(WASM_OPERATION.prepareSql, encodePrepareSql('SELECT $1')),
    ).toBe('SELECT $1');
    expect(
      decodeRequest(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(7, [1, 'two'], true),
      ),
    ).toEqual({statementId: 7, params: [1, 'two'], arrayRows: true});
    expect(
      decodeRequest(WASM_OPERATION.closePrepared, encodeClosePrepared(7)),
    ).toBe(7);
    expect(
      decodeRequest(
        WASM_OPERATION.execSql,
        encodeExecSql('CREATE TABLE items; SELECT * FROM items', false),
      ),
    ).toEqual({sql: 'CREATE TABLE items; SELECT * FROM items'});
  });

  it('writes short ASCII strings as copies and every other string as UTF-8', () => {
    const params = [
      '',
      'plain',
      '\u007f',
      '\u0080',
      'ends in é',
      'é begins',
      'x'.repeat(64),
      `${'y'.repeat(63)}é`,
      'z'.repeat(65),
      '\ufeffmarked',
      '\ufeff',
    ];
    expect(
      decodeRequest(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(2, params, false),
      ),
    ).toEqual({statementId: 2, params});
  });

  it('writes a lone surrogate as TextEncoder does, as U+FFFD', () => {
    expect(
      decodeRequest(
        WASM_OPERATION.executeSql,
        encodeExecuteSql('SELECT $1', ['a\ud800b'], false),
      ),
    ).toEqual({sql: 'SELECT $1', params: ['a�b']});
  });

  it('writes each request afresh into the reused buffer, growing it as needed', () => {
    const large = 'x'.repeat(100_000);
    const first = decodeRequest(
      WASM_OPERATION.executeSql,
      encodeExecuteSql('SELECT $1', [large], false),
    );
    expect(first).toEqual({sql: 'SELECT $1', params: [large]});
    expect(
      decodeRequest(
        WASM_OPERATION.executeSql,
        encodeExecuteSql('SELECT 1', [], true),
      ),
    ).toEqual({sql: 'SELECT 1', params: [], arrayRows: true});
  });

  it('keeps every value when the buffer grows in the middle of one', () => {
    // Enough values to cross several capacities, at every alignment of tags,
    // numbers, and strings, and then to be let go as unusually large.
    const params: JsonValue[] = Array.from({length: 200_000}, (_, index) =>
      index % 3 === 0 ? index : index % 3 === 1 ? index + 0.5 : `v${index}`,
    );
    expect(
      decodeRequest(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(3, params, false),
      ),
    ).toEqual({statementId: 3, params});
    expect(
      decodeRequest(
        WASM_OPERATION.executePrepared,
        encodeExecutePrepared(4, [{a: [1, 'b']}], false),
      ),
    ).toEqual({statementId: 4, params: [{a: [1, 'b']}]});
  });

  it('makes room for a string that fills the buffer to the byte', () => {
    // A request of over a megabyte leaves the buffer as small as it began.
    const reset = (): void => {
      encodeExecuteSql('SELECT $1', ['x'.repeat(2 * 1024 * 1024)], false);
    };
    // A statement's number, the count of its parameters, and the tag and
    // length of the first take fourteen bytes, and each of these characters
    // takes the three it is given room for, so the string ends at, just
    // before, and just past the end of a buffer of 1,024 bytes and of 2,048.
    for (const units of [335, 336, 337, 338, 339, 677, 678, 679, 680]) {
      for (const text of ['€'.repeat(units), `${'€'.repeat(units - 1)}x`, 'x'.repeat(units * 3)]) {
        reset();
        const params = [text, 7, text];
        expect(
          decodeRequest(
            WASM_OPERATION.executePrepared,
            encodeExecutePrepared(7, params, false),
          ),
        ).toEqual({statementId: 7, params});
        reset();
        expect(same(
          outcome(() => encodeExecutePrepared(7, params, false)),
          outcome(() => oracle.encodeExecutePrepared(7, params, false)),
        )).toBe(true);
      }
    }
    // A statement's text that is longer than twice the buffer has a buffer
    // made to fit the room it is given, three bytes for each of its units. So
    // its bytes end at the buffer's end, or a few short of it by what its last
    // units are, and what follows it must make its own room: the count of
    // the parameters, and then the first of them, whatever its kind.
    for (const units of [683, 700, 1400]) {
      for (const tail of ['', 'é', 'x', 'éx', 'xx', 'éxx', 'xxx']) {
        const sql = `${'€'.repeat(units - tail.length)}${tail}`;
        for (const first of [7, 0.5, 'text', '', null, true, false, ['nested']] as JsonValue[]) {
          const params = [first, sql, 9];
          reset();
          expect(
            decodeRequest(WASM_OPERATION.executeSql, encodeExecuteSql(sql, params, true)),
            `${units} ${tail} ${JSON.stringify(first)}`,
          ).toEqual({sql, params, arrayRows: true});
          reset();
          expect(same(
            outcome(() => encodeExecuteSql(sql, [...LEAD, ...params], false, LEAD.length)),
            outcome(() => oracle.encodeExecuteSql(sql, params, false)),
          )).toBe(true);
        }
      }
    }
  });

  it('gives a string three bytes a unit of room up to the bound on a request, and measures it past that', () => {
    // After the four bytes of its length, a string of 5,592,404 units has
    // room that ends exactly where a request must: it is written as any
    // shorter one is. One unit more and its room would pass the bound, so it
    // is measured first, and written in the bytes it takes.
    for (const units of [5_592_403, 5_592_404, 5_592_405]) {
      const sql = 'x'.repeat(units);
      const written = outcome(() => encodePrepareSql(sql));
      expect(typeof written === 'string' ? written : written.length).toBe(4 + units);
      expect(same(written, outcome(() => oracle.encodePrepareSql(sql)))).toBe(true);
    }
    // Units of three bytes each fill that room to its last byte, which is
    // twelve bytes more than a request may be estimated to retain: the room
    // is made, and the string then refused.
    const wide = '€'.repeat(5_592_404);
    const refused = outcome(() => encodePrepareSql(wide));
    expect(refused).toBe('RESOURCE_LIMIT: A bridge call exceeded its resource limit');
    expect(outcome(() => oracle.encodePrepareSql(wide))).toBe(refused);
    // The next request is written as ever.
    expect(decodeRequest(WASM_OPERATION.prepareSql, encodePrepareSql('SELECT 1'))).toBe('SELECT 1');
  }, 30_000);

  it('rejects sparse arrays, values beyond JSON, revoked proxies, and oversized work', () => {
    const sparse = new Array<JsonValue>(1);
    expect(() => encodeExecuteSql('SELECT $1', sparse, false)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
    for (const value of [Number.NaN, () => 1, {nested: [undefined]}]) {
      expect(() =>
        encodeExecuteSql('SELECT $1', [value as JsonValue], false),
      ).toThrow(expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}));
    }

    const revoked = Proxy.revocable([], {});
    revoked.revoke();
    expect(() => encodeExecuteSql('SELECT 1', revoked.proxy, false)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );

    expect(() =>
      encodeExecuteSql('SELECT 1', new Array(1_000_001), false),
    ).toThrow(expect.objectContaining({code: 'RESOURCE_LIMIT'}));
    expect(() =>
      encodeExecuteSql('x'.repeat(16 * 1024 * 1024), [], false),
    ).toThrow(expect.objectContaining({code: 'RESOURCE_LIMIT'}));

    for (const invalidId of [0, -1, 1.5, 0x1_0000_0000]) {
      expect(() => encodeExecutePrepared(invalidId, [], false)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
      expect(() => encodeClosePrepared(invalidId)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
  });

  it('checks the retained Rust-model estimate', () => {
    // wasm32 Vec<Value> is 12 bytes and each serde_json::Value slot is 24.
    expect(() =>
      encodeExecuteSql('SELECT', new Array<JsonValue>(699_049).fill(null), false),
    ).not.toThrow();
    expect(() =>
      encodeExecuteSql('SELECT', new Array<JsonValue>(699_050).fill(null), false),
    ).toThrow(expect.objectContaining({code: 'RESOURCE_LIMIT'}));
  }, 15_000);

  it('writes the parameters from where they begin, as if they had been sliced out', () => {
    const params = [
      -0,
      1.5,
      'two',
      null,
      true,
      false,
      9_007_199_254_740_993,
      'é😀',
      {nested: [true, null, 'x'], ['__proto__']: 'data'},
      [1, [2, [3]]],
    ] satisfies JsonValue[];
    // Every count of parameters, after every length of what comes before them.
    for (let lead = 0; lead <= LEAD.length; lead++) {
      for (let count = 0; count <= params.length; count++) {
        const sliced = params.slice(0, count);
        const whole = [...LEAD.slice(0, lead), ...sliced];
        for (const arrayRows of [false, true]) {
          const prepared = outcome(() => encodeExecutePrepared(7, sliced, arrayRows));
          expect(outcome(() => encodeExecutePrepared(7, whole, arrayRows, lead))).toEqual(prepared);
          expect(prepared).toEqual(outcome(() => oracle.encodeExecutePrepared(7, sliced, arrayRows)));
          const text = outcome(() => encodeExecuteSql('SELECT $1', sliced, arrayRows));
          expect(outcome(() => encodeExecuteSql('SELECT $1', whole, arrayRows, lead))).toEqual(text);
          expect(text).toEqual(outcome(() => oracle.encodeExecuteSql('SELECT $1', sliced, arrayRows)));
        }
        expect(
          decodeRequest(
            WASM_OPERATION.executePrepared,
            encodeExecutePrepared(7, whole, false, lead),
          ),
        ).toEqual({statementId: 7, params: sliced});
      }
    }
    // Beginning at the end, or past it, leaves no parameters.
    for (const from of [LEAD.length, LEAD.length + 1, 1000]) {
      expect(
        decodeRequest(WASM_OPERATION.executeSql, encodeExecuteSql('SELECT 1', LEAD, true, from)),
      ).toEqual({sql: 'SELECT 1', params: [], arrayRows: true});
    }
    // And beginning at the start is writing them all.
    expect(outcome(() => encodeExecutePrepared(7, params, false, 0))).toEqual(
      outcome(() => encodeExecutePrepared(7, params, false)),
    );
  });

  it('applies every check and bound to the parameters alone', () => {
    // What comes before the parameters is not looked at, whatever it is.
    const before = [undefined, () => 1, Number.NaN, {nested: [undefined]}, Symbol('x'), 10n];
    const holes = new Array<JsonValue>(LEAD.length);
    for (const lead of [before as unknown as JsonValue[], holes]) {
      expect(
        decodeRequest(
          WASM_OPERATION.executePrepared,
          encodeExecutePrepared(7, [...lead, 1, 'two'], false, lead.length),
        ),
      ).toEqual({statementId: 7, params: [1, 'two']});
    }
    // What is refused among them is refused wherever they begin, and as the
    // same thing: a hole, a value beyond JSON, a number that is not finite,
    // a value too deep, and a statement that is not one.
    const deep = (depth: number): JsonValue => {
      let value: JsonValue = null;
      for (let index = 0; index < depth; index += 1) {
        value = [value];
      }
      return value;
    };
    const refused: [params: unknown[], message: string][] = [
      [new Array(1), 'INVALID_BRIDGE_VALUE: SQL parameters cannot be sparse'],
      [[1, , 3], 'INVALID_BRIDGE_VALUE: SQL parameters cannot be sparse'],
      [[1, undefined], 'INVALID_BRIDGE_VALUE: SQL parameters cannot be sparse'],
      [['a', () => 1], 'INVALID_BRIDGE_VALUE: A value is not JSON-compatible'],
      [[10n], 'INVALID_BRIDGE_VALUE: A value is not JSON-compatible'],
      [[1, Number.NaN], 'INVALID_BRIDGE_VALUE: A JSON number must be finite'],
      [[-Infinity, 1], 'INVALID_BRIDGE_VALUE: A JSON number must be finite'],
      [[{nested: [undefined]}], 'INVALID_BRIDGE_VALUE: JSON array cannot be sparse'],
      [[1, 'two', deep(65)], 'INVALID_BRIDGE_VALUE: A bridge value is too deeply nested'],
    ];
    for (const [params, message] of refused) {
      for (const lead of [[], LEAD]) {
        const whole = [...lead, ...params] as JsonValue[];
        // Spreading turns a hole into an undefined, which is refused alike.
        expect(outcome(() => encodeExecutePrepared(7, whole, false, lead.length))).toBe(message);
        expect(outcome(() => encodeExecuteSql('SELECT 1', whole, false, lead.length))).toBe(message);
      }
      expect(outcome(() => oracle.encodeExecutePrepared(7, params as JsonValue[], false))).toBe(message);
    }
    expect(() => encodeExecutePrepared(7, [...LEAD, deep(64)], false, LEAD.length)).not.toThrow();
    for (const invalidId of [0, -1, 1.5, 0x1_0000_0000, Number.NaN, '7', null, undefined]) {
      expect(
        outcome(() => encodeExecutePrepared(invalidId as number, [...LEAD, 1], false, LEAD.length)),
      ).toBe('INVALID_BRIDGE_VALUE: A prepared statement ID must be a nonzero unsigned 32-bit integer');
    }
    expect(outcome(() => encodeExecuteSql(7 as unknown as string, [...LEAD, 1], false, LEAD.length))).toBe(
      'INVALID_BRIDGE_VALUE: A bridge string must be a string',
    );
    expect(
      outcome(() => encodeExecutePrepared(7, 'params' as unknown as JsonValue[], false, 2)),
    ).toBe('INVALID_BRIDGE_VALUE: SQL parameters must be an array');

    // The bounds count the parameters, and not what is before them: as many
    // values as may be retained, and a request as long as one may be.
    const most = [...LEAD, ...new Array<JsonValue>(699_050).fill(null)];
    expect(outcome(() => encodeExecutePrepared(7, most, false, LEAD.length))).toBeInstanceOf(Uint8Array);
    most.push(null);
    expect(outcome(() => encodeExecutePrepared(7, most, false, LEAD.length))).toBe(
      'RESOURCE_LIMIT: A bridge call exceeded its resource limit',
    );
    expect(outcome(() => encodeExecutePrepared(7, most, false, LEAD.length + 1))).toBeInstanceOf(Uint8Array);
    expect(outcome(() => oracle.encodeExecutePrepared(7, most.slice(LEAD.length), false))).toBe(
      'RESOURCE_LIMIT: A bridge call exceeded its resource limit',
    );
    expect(
      outcome(() => oracle.encodeExecutePrepared(7, most.slice(LEAD.length + 1), false)),
    ).toBeInstanceOf(Uint8Array);
    // The values of the list are counted before any is written, on top of
    // what the statement's text retains: itself and twelve bytes more. With
    // one value to follow it, a text is as long as it may be when the two
    // come to the bound exactly, and a unit more is refused.
    const MAX = 16 * 1024 * 1024;
    for (const [units, expected] of [
      [MAX - 48, 1 + 4 + (MAX - 48) + 4 + 1],
      [MAX - 47, 'RESOURCE_LIMIT: A bridge call exceeded its resource limit'],
    ] as const) {
      const sql = 'x'.repeat(units);
      for (const written of [
        outcome(() => encodeExecuteSql(sql, [...LEAD, null], false, LEAD.length)),
        outcome(() => oracle.encodeExecuteSql(sql, [null], false)),
      ]) {
        expect(typeof written === 'string' ? written : written.length).toBe(expected);
      }
    }
    // A string is bounded by what it would take to hold: itself and twelve
    // bytes more, beside the twenty-four of its value and the twelve of the
    // list it is in. One that long is written, and one a byte longer is not.
    for (const [units, expected] of [
      [MAX - 48, MAX - 48 + 14],
      [MAX - 47, 'RESOURCE_LIMIT: A bridge call exceeded its resource limit'],
      [MAX, 'RESOURCE_LIMIT: A bridge call exceeded its resource limit'],
    ] as const) {
      const text = 'x'.repeat(units);
      for (const written of [
        outcome(() => encodeExecutePrepared(7, [...LEAD, text], false, LEAD.length)),
        outcome(() => oracle.encodeExecutePrepared(7, [text], false)),
      ]) {
        expect(typeof written === 'string' ? written : written.length).toBe(expected);
      }
    }
  }, 60_000);

  it('writes what the writer it replaced wrote, for generated requests of every kind', () => {
    // Fixed integer arithmetic makes the generated requests the same on every run.
    let state = 0x5eed;
    const below = (bound: number): number => {
      state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
      return Math.floor((state / 0x1_0000_0000) * bound);
    };
    const texts = [
      '',
      'a',
      'updated',
      'seven thousand nine hundred nineteen',
      'x'.repeat(63),
      'x'.repeat(64),
      'x'.repeat(65),
      `${'y'.repeat(63)}é`,
      'é',
      'é😀',
      'a\ud800b',
      '\udc00',
      '\ufeffbom',
      '\u0000\u007f\u0080',
      'z'.repeat(5_000),
    ];
    const numbers = [
      0,
      -0,
      1,
      -1,
      7919,
      0.5,
      -2.25,
      1e300,
      5e-324,
      2 ** 31,
      2 ** 32,
      Number.MAX_SAFE_INTEGER,
      Number.MIN_SAFE_INTEGER,
      Number.MAX_SAFE_INTEGER + 1,
      -(2 ** 53),
      1e21,
    ];
    const scalar = (): JsonValue => {
      switch (below(8)) {
        case 0:
          return null;
        case 1:
          return below(2) === 0;
        case 2:
        case 3:
        case 4:
          return numbers[below(numbers.length)]!;
        default:
          return texts[below(texts.length)]!;
      }
    };
    // Now and then, what no request may hold.
    const unusual = (): unknown =>
      [undefined, Number.NaN, Infinity, () => 1, 10n, Symbol('s'), new Date(0), new Map()][below(8)];
    const value = (depth: number): unknown => {
      const kind = below(depth > 3 ? 20 : 30);
      if (kind < 20) {
        return scalar();
      }
      if (kind === 20) {
        return unusual();
      }
      if (kind < 25) {
        return Array.from({length: below(5)}, () => value(depth + 1));
      }
      if (kind === 25) {
        let nested: unknown = scalar();
        for (let levels = 60 + below(8); levels > 0; levels--) {
          nested = below(2) === 0 ? [nested] : {k: nested};
        }
        return nested;
      }
      const object: Record<string, unknown> = {};
      for (let keys = below(5); keys > 0; keys--) {
        Object.defineProperty(object, ['a', 'b', 'é', '__proto__', '', 'key'][below(6)]!, {
          value: value(depth + 1),
          enumerable: true,
          writable: true,
          configurable: true,
        });
      }
      return object;
    };
    const ids = [1, 7, 0xffff_ffff, 0, -1, 1.5, 0x1_0000_0000, Number.NaN];
    let written = 0;
    let refused = 0;
    for (let run = 0; run < 3_000; run++) {
      // Most requests are a statement's few scalars, as most statements' are.
      const count = [0, 1, 2, 2, 5, 5, 7, 40, 150][below(9)]!;
      const plain = below(4) > 0;
      const params = Array.from({length: count}, () => (plain ? scalar() : value(0))) as JsonValue[];
      if (below(40) === 0 && count > 0) {
        delete params[below(count)];
      }
      const lead = LEAD.slice(0, below(LEAD.length + 1));
      const whole = [...lead, ...params];
      // Spreading turned any hole into an undefined: put it back.
      for (let index = 0; index < count; index++) {
        if (!(index in params)) {
          delete whole[lead.length + index];
        }
      }
      const arrayRows = below(2) === 0;
      const id = ids[below(below(6) === 0 ? ids.length : 3)]!;
      const sql = texts[below(texts.length)]!;
      const cases: [written: () => Uint8Array, from: () => Uint8Array, replaced: () => Uint8Array][] = [
        [
          () => encodeExecutePrepared(id, params, arrayRows),
          () => encodeExecutePrepared(id, whole, arrayRows, lead.length),
          () => oracle.encodeExecutePrepared(id, params, arrayRows),
        ],
        [
          () => encodeExecuteSql(sql, params, arrayRows),
          () => encodeExecuteSql(sql, whole, arrayRows, lead.length),
          () => oracle.encodeExecuteSql(sql, params, arrayRows),
        ],
      ];
      for (const [alone, from, replaced] of cases) {
        const expected = outcome(replaced);
        if (!same(outcome(alone), expected) || !same(outcome(from), expected)) {
          // Said in full only for a difference.
          expect(outcome(alone), `run ${run}`).toEqual(expected);
          expect(outcome(from), `run ${run} from ${lead.length}`).toEqual(expected);
        }
        if (typeof expected === 'string') {
          refused += 1;
        } else {
          written += 1;
        }
      }
      // The other requests are written alike too.
      const schema = (run % 3 === 0 ? value(0) : scalar()) as JsonValue;
      for (const [now, before] of [
        [() => encodeSetSchema(schema, arrayRows), () => oracle.encodeSetSchema(schema, arrayRows)],
        [() => encodePrepareSql(sql), () => oracle.encodePrepareSql(sql)],
        [() => encodeExecSql(sql, arrayRows), () => oracle.encodeExecSql(sql, arrayRows)],
        [() => encodeClosePrepared(id), () => oracle.encodeClosePrepared(id)],
      ] as const) {
        if (!same(outcome(now), outcome(before))) {
          expect(outcome(now), `run ${run}`).toEqual(outcome(before));
        }
      }
    }
    // Both outcomes came up often enough to mean something.
    expect(written).toBeGreaterThan(3_000);
    expect(refused).toBeGreaterThan(500);
  }, 60_000);

  it('bounds a request as the writer it replaced did, value for value', () => {
    // Each bound is reached by values of its own kind: more values than a
    // request may count, more than it may retain, deeper than it may nest,
    // and more bytes than it may be.
    const wide = Array.from({length: 1000}, () => new Array<JsonValue>(1000).fill(null));
    const big = 'x'.repeat(4 * 1024 * 1024);
    for (const params of [
      [wide],
      [wide, 1],
      [1, 'two', wide],
      [wide.slice(0, 600)],
      [big, big, big],
      [big, big, big, big],
      [big, 1, [big, {a: big}], big],
      [Array.from({length: 120_000}, () => ({}))],
      [Array.from({length: 100_000}, () => ({a: 1}))],
      [Array.from({length: 120_000}, () => 'x')],
      new Array<JsonValue>(1_000_001).fill(null),
      new Array<JsonValue>(1_000_000).fill(1),
    ] as JsonValue[][]) {
      const expected = outcome(() => oracle.encodeExecutePrepared(7, params, false));
      for (const written of [
        outcome(() => encodeExecutePrepared(7, params, false)),
        outcome(() => encodeExecutePrepared(7, [...LEAD, ...params], false, LEAD.length)),
      ]) {
        expect(typeof written === 'string' ? written : written.length).toBe(
          typeof expected === 'string' ? expected : expected.length,
        );
        expect(same(written, expected)).toBe(true);
      }
    }
  }, 120_000);

  it('accepts JSON depth 64 and rejects the first deeper node', () => {
    const nested = (depth: number): JsonValue => {
      let value: JsonValue = null;
      for (let index = 0; index < depth; index += 1) {
        value = [value];
      }
      return value;
    };

    expect(() =>
      encodeExecuteSql('SELECT $1', [nested(64)], false),
    ).not.toThrow();
    expect(() => encodeExecuteSql('SELECT $1', [nested(65)], false)).toThrow(
      expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
    );
  });

  it('rejects non-finite JSON numbers', () => {
    for (const value of [Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(() => encodeExecuteSql('SELECT $1', [value], false)).toThrow(
        expect.objectContaining({code: 'INVALID_BRIDGE_VALUE'}),
      );
    }
  });
});
