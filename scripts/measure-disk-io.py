"""Measure a process's disk reads and the cookie-store copies it makes.

The counter is `proc_pid_rusage`'s ri_diskio_bytesread, read by byte offset
rather than by restating the whole rusage_info_v4 struct: a field-order mistake
in a restated struct yields plausible zeros rather than an error, which is the
failure this measurement is trying to avoid making.

Usage: measure_cookie_reads.py <pid> <seconds>
"""

import ctypes
import glob
import os
import sys
import pathlib
import re
import tempfile
import time

lib = ctypes.CDLL("/usr/lib/libSystem.dylib")
BUF = ctypes.create_string_buffer(4096)

# ri_uuid[16], then 16 u64 fields, then ri_diskio_bytesread and _byteswritten.
DISKIO_OFFSET = 16 + 16 * 8


def counters(pid):
    if lib.proc_pid_rusage(ctypes.c_int(pid), ctypes.c_int(4), ctypes.byref(BUF)) != 0:
        raise SystemExit("proc_pid_rusage failed for pid %d" % pid)
    raw = BUF.raw
    read = ctypes.c_uint64.from_buffer_copy(raw, DISKIO_OFFSET).value
    written = ctypes.c_uint64.from_buffer_copy(raw, DISKIO_OFFSET + 8).value
    return read, written


def control():
    """Prove the counter moves, so a zero reading means idle rather than broken."""
    me = os.getpid()
    before = counters(me)[1]
    path = os.path.join(tempfile.gettempdir(), "ck_counter_control.bin")
    with open(path, "wb") as handle:
        handle.write(b"x" * (64 * 1024 * 1024))
        handle.flush()
        os.fsync(handle.fileno())
    after = counters(me)[1]
    os.unlink(path)
    moved = (after - before) / 2**20
    print("  control: wrote 64 MB, counter moved %.1f MB" % moved)
    if moved < 32:
        raise SystemExit("  counter is not tracking writes; readings below are void")


SOURCE = pathlib.Path(__file__).resolve().parent.parent / "crates/quota-core/src/browser_cookies.rs"


def snapshot_prefix():
    """Read the temp-copy filename prefix out of the module that creates them.

    Restating it here would make a rename in that file reduce the copy count to
    zero with no other symptom -- and zero is what this script prints when the
    snapshot sharing is working perfectly, so the broken instrument and the best
    possible result would be indistinguishable in the output.

    Refuses rather than falling back to a literal: a default would restore
    exactly the silent-zero failure this exists to prevent, and would do it at
    the moment the constant moved.
    """
    text = SOURCE.read_text(encoding="utf-8")
    match = re.search(r'COOKIE_SNAPSHOT_PREFIX: &str = "([^"]+)"', text)
    if not match:
        sys.exit(
            "refusing: no COOKIE_SNAPSHOT_PREFIX in %s.\n"
            "The copy count cannot be measured without it, and a zero here would\n"
            "read as a perfect result rather than as a broken instrument." % SOURCE
        )
    return match.group(1)


def main():
    pid = int(sys.argv[1])
    window = float(sys.argv[2]) if len(sys.argv) > 2 else 180.0
    control()

    prefix = snapshot_prefix()
    pattern = os.path.join(tempfile.gettempdir(), "%s-%d-*.db" % (prefix, pid))
    start = counters(pid)
    t0 = time.time()
    seen = set()
    while time.time() - t0 < window:
        seen.update(glob.glob(pattern))
        time.sleep(0.05)
    end = counters(pid)
    elapsed = time.time() - t0

    read_mb = (end[0] - start[0]) / 2**20
    written_mb = (end[1] - start[1]) / 2**20
    print("  pid %d over %.0fs:" % (pid, elapsed))
    print("    read           %8.1f MB   -> %.3f GB/hour" % (read_mb, read_mb * 3600 / elapsed / 1024))
    print("    written        %8.1f MB   -> %.3f GB/hour" % (written_mb, written_mb * 3600 / elapsed / 1024))
    print("    store copies   %8d" % len(seen))


if __name__ == "__main__":
    main()
