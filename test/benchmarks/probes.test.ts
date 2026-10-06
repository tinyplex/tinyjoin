import {mkdtemp, readdir, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {describe, expect, it, vi} from 'vitest';

import {
  createFlushProbe,
  createSettler,
  type Probe,
  type SettlerOptions,
} from '../../benchmarks/compare/probes.mjs';

// A probe that answers from a script of readings, or throws the error the
// script holds, and a sleep that only records its calls, so that a settler's
// waiting can be checked without waiting.
const scripted = (readings: (number | Error)[]): Probe & {calls: number} => {
  const probe = {
    name: 'scripted probe',
    calls: 0,
    probe: (): number => {
      probe.calls += 1;
      const reading = readings.shift();
      if (reading == null) {
        throw new Error('the script ran out of readings');
      }
      if (reading instanceof Error) {
        throw reading;
      }
      return reading;
    },
  };
  return probe;
};

const sleeper = () => vi.fn<(seconds: number) => Promise<void>>(async () => {});
const logger = () => vi.fn<(message: string) => void>();
const warner = () => vi.fn<(message: string) => void>();

// The flush gate's settings: a reference that may fall, and a tolerance of half
// again or a tenth of a millisecond, whichever is the larger.
const flushOptions = (overrides: Partial<SettlerOptions> = {}): SettlerOptions => ({
  tolerance: (reference) => Math.max(reference * 1.5, reference + 0.1),
  stepSeconds: 1,
  maxSeconds: 3,
  reference: 'repeated',
  counter: 'unrecoveredSamples',
  sleep: sleeper(),
  log: logger(),
  warn: warner(),
  ...overrides,
});

describe('a settler', () => {
  it('takes its baseline as the median of the windows, a gap apart', async () => {
    const sleep = sleeper();
    const settler = await createSettler(
      scripted([0.5, 0.9, 0.4]),
      flushOptions({baselineWindows: 3, baselineGapSeconds: 1, sleep}),
    );
    expect(settler.report()).toMatchObject({
      baselineMs: 0.5,
      referenceMs: 0.5,
      quietestMs: 0.5,
      slowestMs: 0,
      waitedSeconds: 0,
      unrecoveredSamples: 0,
    });
    expect(sleep.mock.calls).toEqual([[1], [1]]);
  });

  it('returns at once, with the reading, when it is within tolerance', async () => {
    const options = flushOptions();
    const settler = await createSettler(scripted([0.4, 0.5]), options);
    expect(await settler.settle('tinyjoin reopen round 1')).toEqual({
      ms: 0.5,
      waitedSeconds: 0,
      recovered: true,
    });
    expect(options.sleep).not.toHaveBeenCalled();
    expect(settler.report()).toMatchObject({slowestMs: 0.5, quietestMs: 0.4});
  });

  it('waits in steps until the probe recovers, and keeps the first reading', async () => {
    const options = flushOptions();
    const settler = await createSettler(scripted([0.4, 1.2, 1.1, 0.45]), options);
    expect(await settler.settle('tinyjoin reopen round 1')).toEqual({
      ms: 1.2,
      waitedSeconds: 2,
      recovered: true,
    });
    expect(options.sleep).toHaveBeenCalledTimes(2);
    expect(options.sleep).toHaveBeenCalledWith(1);
    expect(settler.report()).toMatchObject({
      waitedSeconds: 2,
      slowestMs: 1.2,
      unrecoveredSamples: 0,
    });
    expect(options.warn).not.toHaveBeenCalled();
  });

  it('gives up at the cap, counts the sample, and warns with the label', async () => {
    const warn = warner();
    const options = flushOptions({warn});
    const probe = scripted([0.4, 1.2, 1.3, 1.2, 1.4, 1.2]);
    const settler = await createSettler(probe, options);
    expect(await settler.settle('sqlite reopen round 2')).toEqual({
      ms: 1.2,
      waitedSeconds: 3,
      recovered: false,
    });
    expect(probe.calls).toBe(5);
    expect(options.sleep).toHaveBeenCalledTimes(3);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn.mock.calls[0]?.[0]).toContain('sqlite reopen round 2');
    expect(warn.mock.calls[0]?.[0]).toContain('scripted probe');
    expect(settler.report()).toMatchObject({
      waitedSeconds: 3,
      slowestMs: 1.4,
      unrecoveredSamples: 1,
    });
  });

  it('lowers the reference only to a quieter reading that the next reading repeats', async () => {
    const tolerance = (reference: number) => reference * 2;
    const settler = await createSettler(
      scripted([0.4, 0.3, 0.5, 0.3, 0.35, 0.7, 0.2, 0.32]),
      flushOptions({tolerance}),
    );
    await settler.settle('one quick window');
    expect(settler.report().referenceMs).toBe(0.4);
    await settler.settle('a usual window clears it');
    await settler.settle('quick again');
    expect(settler.report().referenceMs).toBe(0.4);
    await settler.settle('and repeated: the higher of the two');
    expect(settler.report().referenceMs).toBe(0.35);
    await settler.settle('a slow reading never raises it');
    expect(settler.report().referenceMs).toBe(0.35);
    await settler.settle('quick');
    await settler.settle('repeated');
    expect(settler.report()).toMatchObject({
      referenceMs: 0.32,
      quietestMs: 0.2,
      slowestMs: 0.7,
    });
  });

  it('keeps the start as the reference for a probe referenced to it', async () => {
    const warn = warner();
    const settler = await createSettler(scripted([18, 17, 17.2, 19.5, 17.5]), {
      tolerance: (reference) => reference * 1.05,
      stepSeconds: 5,
      maxSeconds: 60,
      sleep: sleeper(),
      log: logger(),
      warn,
    });
    await settler.settle('round 1');
    await settler.settle('round 2');
    expect(settler.report()).toMatchObject({
      baselineMs: 18,
      referenceMs: 18,
      quietestMs: 17,
      unrecoveredRounds: 0,
    });
    expect(await settler.settle('round 3')).toEqual({
      ms: 19.5,
      waitedSeconds: 5,
      recovered: true,
    });
    expect(settler.report().referenceMs).toBe(18);
    expect(warn).not.toHaveBeenCalled();
  });

  it('reports the live state object that settling mutates', async () => {
    const settler = await createSettler(scripted([0.4, 0.9, 0.4]), flushOptions());
    const state = settler.report();
    expect(settler.report()).toBe(state);
    await settler.settle('round 1');
    expect(state).toMatchObject({waitedSeconds: 1, slowestMs: 0.9});
  });

  it('carries the probe primitive and whether it is approximate', async () => {
    const probe = Object.assign(scripted([0.4]), {
      primitive: 'F_FULLFSYNC',
      approximate: true,
    });
    const settler = await createSettler(probe, flushOptions());
    expect(settler.report()).toMatchObject({
      primitive: 'F_FULLFSYNC',
      approximate: true,
    });
    const plain = await createSettler(scripted([18]), flushOptions());
    expect(plain.report()).not.toHaveProperty('primitive');
    expect(plain.report()).not.toHaveProperty('approximate');
  });

  it('stops waiting on a probe that fails, after warning once', async () => {
    const warn = warner();
    const options = flushOptions({warn});
    const probe = scripted([0.4, new Error('EIO: i/o error\nmore detail'), 0.4]);
    const settler = await createSettler(probe, options);
    expect(await settler.settle('pglite reopen round 1')).toEqual({
      ms: null,
      waitedSeconds: 0,
      recovered: true,
    });
    expect(settler.report().fault).toBe('EIO: i/o error');
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn.mock.calls[0]?.[0]).toContain('EIO: i/o error');
    expect(await settler.settle('tinyjoin reopen round 1')).toEqual({
      ms: null,
      waitedSeconds: 0,
      recovered: true,
    });
    expect(probe.calls).toBe(2);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(options.sleep).not.toHaveBeenCalled();
  });

  it('records a probe that fails while its baseline is taken, and never reads it again', async () => {
    const warn = warner();
    const options = flushOptions({warn});
    const probe = scripted([new Error('ENOSPC: no space left on device'), 0.4]);
    const settler = await createSettler(probe, options);
    expect(settler.report()).toMatchObject({
      baselineMs: null,
      referenceMs: null,
      quietestMs: null,
      fault: 'ENOSPC: no space left on device',
    });
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn.mock.calls[0]?.[0]).toContain('before the run');
    expect(await settler.settle('tinyjoin reopen round 1')).toEqual({
      ms: null,
      waitedSeconds: 0,
      recovered: true,
    });
    expect(probe.calls).toBe(1);
    expect(options.sleep).not.toHaveBeenCalled();
  });

  it('lowers a repeated reference no further than the floor set from the baseline', async () => {
    const settler = await createSettler(
      scripted([0.6, 0.3, 0.3, 0.45, 0.45]),
      flushOptions({
        tolerance: (reference) => reference * 3,
        floor: (baselineMs) => baselineMs / 1.5,
      }),
    );
    await settler.settle('quick');
    expect(settler.report().referenceMs).toBe(0.6);
    await settler.settle('repeated, but below the floor');
    expect(settler.report()).toMatchObject({referenceMs: 0.4, quietestMs: 0.3});
    await settler.settle('above the reference');
    await settler.settle('and again');
    expect(settler.report().referenceMs).toBe(0.4);
  });

  it('prints each wait as it happens, with the first and last readings', async () => {
    const log = logger();
    const settler = await createSettler(
      scripted([0.4, 1.2, 0.9, 0.45, 0.5]),
      flushOptions({log}),
    );
    await settler.settle('pglite insert-autocommit round 3');
    expect(log).toHaveBeenCalledTimes(1);
    expect(log.mock.calls[0]?.[0]).toBe(
      '  pglite insert-autocommit round 3: waited 2 s for the scripted probe, ' +
        'from 1.2 ms to 0.45 ms',
    );
    await settler.settle('tinyjoin insert-autocommit round 3');
    expect(log).toHaveBeenCalledTimes(1);
  });

  it('gives up after a run of unrecovered samples, and then reads once without waiting', async () => {
    const warn = warner();
    const options = flushOptions({warn, maxSeconds: 1, giveUpAfter: 2});
    const probe = scripted([0.4, 1.2, 1.2, 1.3, 1.3, 1.4, 0.45, 1.5]);
    const settler = await createSettler(probe, options);
    expect((await settler.settle('round 1')).recovered).toBe(false);
    expect(settler.report()).not.toHaveProperty('gaveUp');
    expect((await settler.settle('round 2')).recovered).toBe(false);
    expect(settler.report()).toMatchObject({
      gaveUp: true,
      unrecoveredSamples: 2,
      waitedSeconds: 2,
    });
    expect(warn).toHaveBeenCalledTimes(3);
    expect(warn.mock.calls[2]?.[0]).toContain('2 samples in a row');
    expect(options.sleep).toHaveBeenCalledTimes(2);
    expect(await settler.settle('round 3')).toEqual({
      ms: 1.4,
      waitedSeconds: 0,
      recovered: false,
    });
    expect(await settler.settle('round 4')).toEqual({
      ms: 0.45,
      waitedSeconds: 0,
      recovered: true,
    });
    expect(await settler.settle('round 5')).toEqual({
      ms: 1.5,
      waitedSeconds: 0,
      recovered: false,
    });
    expect(probe.calls).toBe(8);
    expect(options.sleep).toHaveBeenCalledTimes(2);
    expect(warn).toHaveBeenCalledTimes(3);
    expect(settler.report()).toMatchObject({
      gaveUp: true,
      unrecoveredSamples: 4,
      waitedSeconds: 2,
      slowestMs: 1.5,
    });
  });
});

describe('the flush probe', () => {
  it(
    'times a write and flush in a scratch directory that it removes',
    async () => {
      const dir = await mkdtemp(join(tmpdir(), 'tinyjoin-probes-test-'));
      try {
        const probe = await createFlushProbe(dir, {warn: warner()});
        try {
          expect(['F_BARRIERFSYNC', 'F_FULLFSYNC', 'fsync']).toContain(
            probe.primitive,
          );
          expect(await readdir(dir)).toHaveLength(1);
          const ms = await probe.probe();
          expect(Number.isFinite(ms)).toBe(true);
          expect(ms).toBeGreaterThan(0);
          // A flush of a few pages takes well under a millisecond on a laptop's
          // drive, and a few milliseconds on a hosted runner's disk.
          expect(ms).toBeLessThan(1000);
          const settler = await createSettler(probe, flushOptions({baselineWindows: 2}));
          expect(settler.report()).toMatchObject({primitive: probe.primitive});
          expect(settler.report().baselineMs).toBeGreaterThan(0);
        } finally {
          await probe.close();
        }
        expect(await readdir(dir)).toEqual([]);
      } finally {
        await rm(dir, {recursive: true, force: true});
      }
    },
    60_000,
  );
});
