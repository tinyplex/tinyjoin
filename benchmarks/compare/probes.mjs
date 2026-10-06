import {spawn} from 'node:child_process';
import {closeSync, fsyncSync, openSync, writeSync} from 'node:fs';
import {mkdtemp, rm} from 'node:fs/promises';
import {platform} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';

// The probes the runner waits on before it times anything, and the settler that does the
// waiting. A probe returns one reading in milliseconds; a settler takes a probe's baseline
// before the suite starts, and then, before a round or a sample, probes until the reading is
// back within tolerance of its reference, or a cap is reached, and keeps the figures the report
// publishes: the baseline, the slowest reading, the seconds spent waiting, and how many rounds
// or samples began before the probe had recovered.

const median = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length / 2;
  return sorted.length % 2 ? sorted[Math.floor(middle)] : (sorted[middle - 1] + sorted[middle]) / 2;
};

// A fixed computation, timed to tell whether the CPU still runs at the speed it started at. A
// fanless laptop slows as it heats over a long run, and background work takes its cores; either
// would weigh on whichever samples ran then. Each result is stored, so that the computation
// cannot be optimized away.
const probeSink = new Int32Array(1);
export const createCpuProbe = () => ({
  name: 'CPU probe',
  probe: () => {
    let best = Infinity;
    for (let attempt = 0; attempt < 3; attempt++) {
      const start = performance.now();
      let x = 0x9e3779b9 | 0;
      let sum = 0;
      for (let i = 0; i < 10_000_000; i++) {
        x ^= x << 13;
        x ^= x >>> 17;
        x ^= x << 5;
        sum = (sum + (x & 0xff)) | 0;
      }
      best = Math.min(best, performance.now() - start);
      probeSink[0] ^= sum;
    }
    return Math.round(best * 10) / 10;
  },
});

// A write of a few pages and a flush of them to a scratch file on the volume that holds the
// browser profiles, timed to tell whether the disk still commits at the latency it started at.
// Committing one insert flushes once, so a workload of 1,000 such commits is mostly flushes, and
// a flush takes its turn in the drive's queue behind whatever else is being written to the
// volume: the deletion of the previous sample's profile, which for PGlite holds a pool of 1,000
// files, slowed the following sample's commits two- to threefold for seconds, which the CPU
// probe cannot see. One reading is the median of a window of flushes, so that one quick or slow
// flush does not move it.
//
// On macOS the probe issues the system call Chromium's OPFS flush issues. A
// FileSystemSyncAccessHandle's flush() reaches base::File::Flush(), which on Apple platforms is
// fcntl(F_BARRIERFSYNC), with fsync as the fallback: a barrier has the drive write the file's
// data before anything queued after it, without asking it to empty its cache. Node's fs.fsync
// cannot be asked for that: libuv issues F_FULLFSYNC, which does empty the drive's cache and
// takes several times as long, with a cost and a noise of its own, and falls back to the barrier
// only when the file system refuses the full flush, which APFS does not. So the probe runs
// through a long-lived python3 child, flush-probe.py, whose fcntl module can pass the barrier's
// raw command number. Without python3 the probe falls back to Node's fsync, and the report says
// so, since a full flush's readings only approximate the barrier's. On other platforms Node's
// fsync is the flush the probe times.
export const FLUSH_BYTES = 4 * 4096;
const WINDOW = 20;

// The first reading warms the file up with an untimed window, so that a flush that fails, for
// want of space or from a failing drive, surfaces where a settler can record it rather than
// before the run starts.
const createNodeFlush = (dir, window) => {
  const fd = openSync(join(dir, 'node-flush-probe'), 'w');
  const buffer = Buffer.alloc(FLUSH_BYTES, 0x5a);
  let count = 0;
  const flush = () => {
    // The counter changes the file's content with every write, as a commit's would.
    buffer.writeUInt32LE(++count >>> 0, 0);
    const start = performance.now();
    writeSync(fd, buffer, 0, FLUSH_BYTES, 0);
    fsyncSync(fd);
    return performance.now() - start;
  };
  let warmed = false;
  return {
    probe: () => {
      if (!warmed) {
        for (let i = 0; i < window; i++) flush();
        warmed = true;
      }
      return median(Array.from({length: window}, flush));
    },
    close: () => closeSync(fd),
  };
};

// The python3 helper, kept running for the whole suite. It answers each `probe` line on its
// standard input with a window's median, and names the flush it issues once its warm-up is done,
// since a file system that refuses the barrier makes it fall back to fsync, as Chromium does.
const startHelper = (dir, window) =>
  new Promise((resolve, reject) => {
    const script = fileURLToPath(new URL('./flush-probe.py', import.meta.url));
    const child = spawn('python3', ['-u', script, dir, String(window), String(FLUSH_BYTES)], {stdio: ['pipe', 'pipe', 'pipe']});
    const waiting = [];
    let failure = null;
    let stderr = '';
    let buffered = '';
    const fail = (error) => {
      failure ??= error;
      while (waiting.length) waiting.shift().reject(failure);
    };
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      buffered += chunk;
      for (let end = buffered.indexOf('\n'); end >= 0; end = buffered.indexOf('\n')) {
        const line = buffered.slice(0, end);
        buffered = buffered.slice(end + 1);
        waiting.shift()?.resolve(line);
      }
    });
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    child.stdin.on('error', () => {});
    child.on('error', (error) => fail(error));
    child.on('exit', (code, signal) => {
      const detail = stderr.trim().split('\n').at(-1);
      fail(new Error(`flush-probe.py exited with ${signal ?? code}${detail ? `: ${detail}` : ''}`));
    });
    const readLine = () => (failure ? Promise.reject(failure) : new Promise((resolveLine, rejectLine) => waiting.push({resolve: resolveLine, reject: rejectLine})));
    const request = (line) => {
      if (failure) return Promise.reject(failure);
      child.stdin.write(`${line}\n`);
      return readLine();
    };
    const exited = new Promise((resolveExit) => child.once('exit', resolveExit));
    const close = async () => {
      child.stdin.end();
      await Promise.race([exited, new Promise((resolveWait) => setTimeout(resolveWait, 5000).unref())]);
      if (child.exitCode == null && child.signalCode == null) child.kill();
    };
    // The helper's first line comes after its warm-up; a helper that never answers is given up on.
    const started = setTimeout(() => fail(new Error('flush-probe.py did not start within 20 s')), 20_000);
    readLine().then(
      (line) => {
        clearTimeout(started);
        const [ready, primitive] = line.split(' ');
        if (ready !== 'ready' || !primitive) {
          fail(new Error(`flush-probe.py answered ${JSON.stringify(line)}`));
          close().then(() => reject(failure));
          return;
        }
        const probe = async () => {
          const line = await request('probe');
          const ms = Number(line);
          if (!Number.isFinite(ms)) throw new Error(`flush-probe.py answered ${JSON.stringify(line)}`);
          return ms;
        };
        resolve({primitive, probe, close});
      },
      (error) => {
        clearTimeout(started);
        if (child.exitCode == null && child.signalCode == null) child.kill();
        reject(error);
      },
    );
  });

export const createFlushProbe = async (baseDir, {window = WINDOW, warn = console.warn} = {}) => {
  const dir = await mkdtemp(join(baseDir, 'tinyjoin-compare-flush-'));
  const darwin = platform() === 'darwin';
  let helper = null;
  if (darwin) {
    helper = await startHelper(dir, window).catch((error) => {
      warn(`Flush probe: python3 could not issue the barrier flush (${error.message}); timing Node's fsync, a full flush of the drive, which is approximate.`);
      return null;
    });
  }
  const local = helper ? null : createNodeFlush(dir, window);
  const probe = {
    name: 'flush probe',
    primitive: helper ? helper.primitive : darwin ? 'F_FULLFSYNC' : 'fsync',
    probe: async () => Math.round((helper ? await helper.probe() : local.probe()) * 1000) / 1000,
    close: async () => {
      if (helper) await helper.close();
      else local.close();
      await rm(dir, {recursive: true, force: true});
    },
  };
  if (darwin && !helper) probe.approximate = true;
  return probe;
};

// Node's fsync alone, which on macOS is F_FULLFSYNC: the full flush of the drive's cache that the
// engines do not pay for, timed beside the barrier for reference in --probe mode.
export const createNodeFlushProbe = async (baseDir, {window = WINDOW} = {}) => {
  const dir = await mkdtemp(join(baseDir, 'tinyjoin-compare-fsync-'));
  const local = createNodeFlush(dir, window);
  return {
    name: 'Node fsync',
    primitive: platform() === 'darwin' ? 'F_FULLFSYNC' : 'fsync',
    probe: async () => Math.round(local.probe() * 1000) / 1000,
    close: async () => {
      local.close();
      await rm(dir, {recursive: true, force: true});
    },
  };
};

const pause = (seconds) => new Promise((resolve) => setTimeout(resolve, seconds * 1000));

// Waits, before a round or a sample, until a probe's reading is within tolerance of its
// reference. The reference is the baseline, the median of the windows taken before the suite
// starts, which for the CPU probe never moves. A disk is not necessarily quiet when a run starts,
// so the flush probe's reference may fall, but only to a reading that a later reading repeats,
// and then to the higher of the two, and never below the floor the caller sets from the
// baseline: one quick window, which a drive that happens to be idle produces now and then, must
// not become the reference that every later sample is held to, and the hundreds of readings of
// a long run must not work it down until ordinary readings wait. The tolerance is a function of
// the reference, so that a probe whose readings are small can carry an absolute margin as well
// as a ratio. Every wait is printed as it happens. A probe that has not recovered before several
// rounds or samples in a row can be given up on: the report says so, and each later settle takes
// one reading without waiting, so that the readings are still recorded. A probe that fails,
// while the baseline is taken or later, is warned about once and no longer read, so that a fault
// in a probe cannot end a half-hour run.
export const createSettler = async (
  probe,
  {
    tolerance,
    stepSeconds,
    maxSeconds,
    reference = 'start',
    floor = () => 0,
    counter = 'unrecoveredRounds',
    giveUpAfter = Infinity,
    baselineWindows = 1,
    baselineGapSeconds = 0,
    sleep = pause,
    log = console.log,
    warn = console.warn,
  },
) => {
  const noun = counter === 'unrecoveredRounds' ? 'rounds' : 'samples';
  const state = {baselineMs: null, referenceMs: null, quietestMs: null, slowestMs: 0, waitedSeconds: 0, [counter]: 0};
  if (probe.primitive) state.primitive = probe.primitive;
  if (probe.approximate) state.approximate = true;
  let disabled = false;
  let floorMs = 0;
  let pendingLow = null;
  let unrecoveredInARow = 0;

  const fail = (label, error) => {
    disabled = true;
    state.fault = String(error.message ?? error).split('\n')[0];
    warn(`  ${label}: the ${probe.name} failed and is no longer waited on: ${state.fault}`);
  };

  try {
    const readings = [];
    for (let window = 0; window < baselineWindows; window++) {
      if (window > 0 && baselineGapSeconds > 0) await sleep(baselineGapSeconds);
      readings.push(await probe.probe());
    }
    state.baselineMs = state.referenceMs = state.quietestMs = median(readings);
    // Rounded as a reading is, so that a reference that rests on the floor stays a tidy number.
    floorMs = Math.round(floor(state.baselineMs) * 1000) / 1000;
  } catch (error) {
    fail('before the run', error);
  }

  const observe = (ms) => {
    state.slowestMs = Math.max(state.slowestMs, ms);
    state.quietestMs = Math.min(state.quietestMs, ms);
    if (reference !== 'repeated' || ms >= state.referenceMs) {
      pendingLow = null;
    } else if (pendingLow == null) {
      pendingLow = ms;
    } else {
      state.referenceMs = Math.max(pendingLow, ms, floorMs);
      pendingLow = null;
    }
  };

  const settle = async (label) => {
    const result = {ms: null, waitedSeconds: 0, recovered: true};
    if (disabled) return result;
    for (let waited = 0; ; waited += stepSeconds) {
      let ms;
      try {
        ms = await probe.probe();
      } catch (error) {
        fail(label, error);
        return result;
      }
      result.ms ??= ms;
      observe(ms);
      if (ms <= tolerance(state.referenceMs)) {
        unrecoveredInARow = 0;
        if (waited > 0) log(`  ${label}: waited ${waited} s for the ${probe.name}, from ${result.ms} ms to ${ms} ms`);
        return result;
      }
      if (state.gaveUp || waited >= maxSeconds) {
        state[counter] += 1;
        result.recovered = false;
        if (state.gaveUp) return result;
        warn(
          `  ${label}: starting with the ${probe.name} at ${ms} ms, ${(ms / state.referenceMs).toFixed(1)}x its reference of ${state.referenceMs} ms, ` +
            `after ${waited} s, from ${result.ms} ms`,
        );
        if (++unrecoveredInARow >= giveUpAfter) {
          state.gaveUp = true;
          warn(`  ${label}: the ${probe.name} has not recovered before ${giveUpAfter} ${noun} in a row, so it is no longer waited on; its readings are still recorded`);
        }
        return result;
      }
      state.waitedSeconds += stepSeconds;
      result.waitedSeconds += stepSeconds;
      await sleep(stepSeconds);
    }
  };

  // The state object itself, so that a report that holds it is current whenever it is saved.
  return {settle, report: () => state};
};
