import {describe, expect, it} from 'vitest';

import {
  PAGE_AUTHORITY_RECORD_BYTES,
  classifyPageAuthorityRecord,
  encodePageAuthorityMarker,
  pageAuthorityCommitMagic,
  stagePageAuthorityMarker,
} from '../../src/worker/page-authority.ts';

const marker = {
  appliedJournalSequence: 0x0102_0304_0506_0708n,
  databaseRevision: 9_007_199_254_740_991n,
};

describe('page authority marker', () => {
  it('round-trips the exact fixed snapshot-envelope and page payload fields', () => {
    const bytes = encodePageAuthorityMarker(marker);
    expect(bytes).toHaveLength(PAGE_AUTHORITY_RECORD_BYTES);
    expect([...bytes.subarray(0, 8)]).toEqual([
      0x54, 0x47, 0x52, 0x53, 0x4f, 0x50, 0x46, 0x31,
    ]);
    const view = new DataView(bytes.buffer);
    expect(view.getUint32(8, true)).toBe(0x8000_0001);
    expect(view.getBigUint64(12, true)).toBe(1n);
    expect(view.getUint32(20, true)).toBe(32);
    expect(view.getBigUint64(24, true)).toBe(
      marker.appliedJournalSequence,
    );
    expect([...bytes.subarray(36, 44)]).toEqual([
      0x54, 0x47, 0x52, 0x50, 0x41, 0x47, 0x45, 0x00,
    ]);
    expect(view.getUint32(44, true)).toBe(1);
    expect(view.getUint32(48, true)).toBe(1);
    expect(view.getUint32(52, true)).toBe(4096);
    expect(view.getUint32(56, true)).toBe(65_536);
    expect(view.getBigUint64(60, true)).toBe(marker.databaseRevision);
    expect(classifyPageAuthorityRecord(bytes)).toEqual({kind: 'page', marker});
  });

  it('distinguishes staged publication from legacy authority', () => {
    const staged = stagePageAuthorityMarker(marker);
    expect(staged.subarray(0, 8)).toEqual(new Uint8Array(8));
    expect(classifyPageAuthorityRecord(staged)).toEqual({kind: 'staged'});

    const partialMagic = staged.slice();
    partialMagic.set(pageAuthorityCommitMagic().subarray(0, 3));
    expect(classifyPageAuthorityRecord(partialMagic)).toEqual({kind: 'staged'});

    const damagedMagic = encodePageAuthorityMarker(marker);
    damagedMagic[3] = damagedMagic[3]! ^ 0x01;
    expect(() => classifyPageAuthorityRecord(damagedMagic)).toThrowError(
      expect.objectContaining({code: 'STORAGE_CORRUPT'}),
    );

    expect(classifyPageAuthorityRecord(new Uint8Array())).toEqual({
      kind: 'legacy',
    });
    expect(
      classifyPageAuthorityRecord(
        new Uint8Array([0x54, 0x47, 0x52, 0x53, 0x4f, 0x50, 0x46, 0x31]),
      ),
    ).toEqual({kind: 'legacy'});
  });

  it('fails closed for every malformed committed marker class', () => {
    for (const offset of [8, 9, 10, 11, 12, 20, 32, 36, 44, 48, 52, 56, 60]) {
      const bytes = encodePageAuthorityMarker(marker);
      bytes[offset] = bytes[offset]! ^ 0x01;
      expect(() => classifyPageAuthorityRecord(bytes), `offset ${offset}`)
        .toThrowError(expect.objectContaining({code: 'STORAGE_CORRUPT'}));
    }

    const short = encodePageAuthorityMarker(marker).subarray(0, 67);
    expect(() => classifyPageAuthorityRecord(short)).toThrowError(
      expect.objectContaining({code: 'STORAGE_CORRUPT'}),
    );
    const long = new Uint8Array(69);
    long.set(encodePageAuthorityMarker(marker));
    expect(() => classifyPageAuthorityRecord(long)).toThrowError(
      expect.objectContaining({code: 'STORAGE_CORRUPT'}),
    );
  });

  it('checks the full journal and JavaScript-safe revision boundaries', () => {
    expect(() =>
      encodePageAuthorityMarker({
        appliedJournalSequence: 0xffff_ffff_ffff_ffffn,
        databaseRevision: 0n,
      }),
    ).not.toThrow();
    expect(() =>
      encodePageAuthorityMarker({
        appliedJournalSequence: 0x1_0000_0000_0000_0000n,
        databaseRevision: 0n,
      }),
    ).toThrow(RangeError);
    expect(() =>
      encodePageAuthorityMarker({
        appliedJournalSequence: 0n,
        databaseRevision: 9_007_199_254_740_992n,
      }),
    ).toThrow(RangeError);
  });
});
