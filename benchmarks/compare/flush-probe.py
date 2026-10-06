# The flush that probes.mjs times on macOS, issued from python because Node cannot issue it.
# Chromium's OPFS flush is fcntl(F_BARRIERFSYNC), with fsync as the fallback, which has the drive
# write the file's data before anything queued after it; Node's fs.fsync reaches
# fcntl(F_FULLFSYNC) through libuv, which also asks the drive to empty its write cache and takes
# several times as long. Python's fcntl module names only F_FULLFSYNC, so the barrier is passed
# by its number from the macOS SDK's sys/fcntl.h.
#
# Started with a directory, a window size and a byte count, this writes that many bytes to a
# scratch file in the directory and flushes them, once per flush of the warm-up and then once
# per flush of each window. It prints `ready` and the name of the flush it issues after the
# warm-up, answers each `probe` line on its standard input with the median of a window of timed
# writes and flushes in milliseconds, and removes the scratch file when its input ends.

import fcntl
import os
import sys
import time

F_BARRIERFSYNC = 85

directory, window, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
path = os.path.join(directory, 'flush-probe')
fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o600)
buffer = bytearray(b'\x5a' * size)
primitive = 'F_BARRIERFSYNC'
count = 0


def flush():
    global primitive
    if primitive == 'F_BARRIERFSYNC':
        try:
            fcntl.fcntl(fd, F_BARRIERFSYNC)
            return
        except OSError:
            # A file system that refuses the barrier gets fsync, as it does from Chromium.
            primitive = 'fsync'
    os.fsync(fd)


def timed_flush():
    global count
    # The counter changes the file's content with every write, as a commit's would.
    count += 1
    buffer[0:4] = (count & 0xFFFFFFFF).to_bytes(4, 'little')
    start = time.perf_counter()
    os.pwrite(fd, buffer, 0)
    flush()
    return (time.perf_counter() - start) * 1000


def window_median():
    times = sorted(timed_flush() for _ in range(window))
    middle = window // 2
    if window % 2:
        return times[middle]
    return (times[middle - 1] + times[middle]) / 2


try:
    for _ in range(window):
        timed_flush()
    print('ready', primitive, flush=True)
    for line in sys.stdin:
        if line.strip() == 'probe':
            print(f'{window_median():.3f}', flush=True)
finally:
    os.close(fd)
    os.remove(path)
