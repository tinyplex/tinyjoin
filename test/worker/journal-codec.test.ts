import {describe, expect, it} from 'vitest';

import {
  encodeJournalHeader,
  encodeJournalRecord,
  scanJournal,
} from '../../src/worker/journal-codec.ts';

function journal(baseSequence: bigint, ...records: Uint8Array[]): Uint8Array {
  const header = encodeJournalHeader(baseSequence);
  const length = records.reduce(
    (total, record) => total + record.byteLength,
    header.byteLength,
  );
  const bytes = new Uint8Array(length);
  bytes.set(header);
  let offset = header.byteLength;
  for (const record of records) {
    bytes.set(record, offset);
    offset += record.byteLength;
  }
  return bytes;
}

describe('journal envelope codec', () => {
  it('round-trips contiguous transactions after a checkpoint sequence', () => {
    const bytes = journal(
      41n,
      encodeJournalRecord(42n, new Uint8Array([1, 2, 3])),
      encodeJournalRecord(43n, new Uint8Array([4, 5])),
    );

    const scan = scanJournal(bytes);
    expect(scan.baseSequence).toBe(41n);
    expect(scan.tail).toBe('clean');
    expect(scan.validBytes).toBe(bytes.byteLength);
    expect(
      scan.records.map(({sequence, payload}) => ({
        sequence,
        payload: [...payload],
      })),
    ).toEqual([
      {sequence: 42n, payload: [1, 2, 3]},
      {sequence: 43n, payload: [4, 5]},
    ]);
  });

  it('treats every incomplete final-record prefix as a disposable tail', () => {
    const completeFirst = encodeJournalRecord(1n, new Uint8Array([1]));
    const completeSecond = encodeJournalRecord(
      2n,
      new Uint8Array([2, 3, 4, 5]),
    );
    const prefix = journal(0n, completeFirst);

    for (let length = 0; length < completeSecond.byteLength; length += 1) {
      const bytes = new Uint8Array(prefix.byteLength + length);
      bytes.set(prefix);
      bytes.set(completeSecond.subarray(0, length), prefix.byteLength);

      const scan = scanJournal(bytes);
      expect(scan.tail, `record prefix length ${length}`).toBe(
        length === 0 ? 'clean' : 'torn',
      );
      expect(scan.validBytes, `record prefix length ${length}`).toBe(
        prefix.byteLength,
      );
      expect(scan.records.map(({sequence}) => sequence)).toEqual([1n]);
    }
  });

  it('does not discard a fully framed transaction with corrupted data', () => {
    const bytes = journal(
      0n,
      encodeJournalRecord(1n, new Uint8Array([1, 2, 3])),
    );
    // The payload begins after the 28-byte file header and 24-byte record header.
    bytes[52] = (bytes[52] ?? 0) ^ 0xff;

    expect(() => scanJournal(bytes)).toThrowError(
      expect.objectContaining({code: 'STORAGE_JOURNAL_CORRUPT'}),
    );
  });

  it('rejects a sequence gap instead of replaying ambiguous history', () => {
    const bytes = journal(
      7n,
      encodeJournalRecord(9n, new Uint8Array([1])),
    );

    expect(() => scanJournal(bytes)).toThrowError(
      expect.objectContaining({code: 'STORAGE_JOURNAL_CORRUPT'}),
    );
  });

  it('distinguishes an incomplete file header from an empty journal', () => {
    const header = encodeJournalHeader(0n);
    for (let length = 0; length < header.byteLength; length += 1) {
      expect(
        () => scanJournal(header.subarray(0, length)),
        `header prefix length ${length}`,
      ).toThrowError(
        expect.objectContaining({code: 'STORAGE_JOURNAL_HEADER_TORN'}),
      );
    }
  });
});
