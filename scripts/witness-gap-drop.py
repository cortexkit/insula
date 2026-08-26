#!/usr/bin/env python3
"""Make a quota drop happen ACROSS A GAP, so `observedContinuously: false` can be
witnessed on the real wire instead of waited for.

WHY. `quota_drop` stamps every record with whether the two readings that produced
it sat one polling interval apart, or straddled a gap. Across two hosts and
thirteen recorded drops the flag has never once read `false` -- every drop has
been observed continuously. A field that has only ever taken one value is
indistinguishable from a field that is stuck, and its own doc comment says a
reader seeing a long run of `true` would reasonably conclude the false arm is
broken.

The arm is unit-tested and mutation-proved. That is a different claim from "the
production path produces it", and this closes the gap between them.

Sibling of `witness-transient.py`, same trick, one more phase:

    phase 1   serve a healthy window at HIGH_PERCENT
    phase 2   503 for longer than the continuity horizon, so the refresher
              stale-serves and `last_success_at` ages past it
    phase 3   serve a healthy window at LOW_PERCENT

The drop lands between two SUCCESSES separated by more than the horizon, which is
exactly the shape the false arm exists to describe. 503 is transient, so the
window survives the outage rather than being cleared -- a non-transient failure
would reset the slot and destroy the prior reading, which is why an expired
credential cannot produce this and why nobody has seen it by accident.

THE HORIZON IS READ FROM SOURCE, NOT RESTATED. `BASE_INTERVAL` (refresh.rs) times
`CONTINUOUS_MULTIPLIER` (quota_drop.rs). If either constant moves, this harness
moves with it; a hardcoded 120 would keep passing while testing nothing.

USAGE, against the deployed module:

    python3 scripts/witness-gap-drop.py &
    ck module rescan && ck module restart insula     # RESCAN, not just restart
    # then watch:
    ck quota --json | grep -A3 sub2api
    python3 scripts/health.py quotaDropsByProvider quotaDropsObservedContinuously

Expected on the wire: one drop recorded for `sub2api`, and
`quotaDropsObservedContinuously` NOT incrementing with it -- the count of drops
rises while the continuous count does not.

WHAT IT DOES NOT PROVE. That any real provider will ever produce this. It proves
the producer path emits the false arm when handed the shape, which is the half
that was untested outside unit tests.
"""

from __future__ import annotations

import datetime
import json
import pathlib
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

REPO = pathlib.Path(__file__).resolve().parent.parent


def read_const(relpath: str, name: str, pattern: str) -> int:
    """Read a constant out of the source rather than restating it here."""
    text = (REPO / relpath).read_text()
    m = re.search(pattern, text)
    if not m:
        print(
            f"could not check: {name} not found in {relpath} -- the constant was "
            f"renamed or moved, and a guessed value would test nothing",
            file=sys.stderr,
        )
        raise SystemExit(2)
    return int(m.group(1))


BASE_INTERVAL_SECS = read_const(
    "crates/quota-core/src/refresh.rs",
    "BASE_INTERVAL",
    r"pub const BASE_INTERVAL: Duration = Duration::from_secs\((\d+)\)",
)
CONTINUOUS_MULTIPLIER = read_const(
    "crates/quota-core/src/quota_drop.rs",
    "CONTINUOUS_MULTIPLIER",
    r"const CONTINUOUS_MULTIPLIER: u32 = (\d+)",
)

HORIZON_SECS = BASE_INTERVAL_SECS * CONTINUOUS_MULTIPLIER

# Enough 503s to carry the gap past the horizon with margin. The refresher backs
# off exponentially on transient failures, so the wall-clock gap grows faster
# than one interval per refusal -- this is a floor, not an estimate.
OUTAGE_REQUESTS = max(3, (HORIZON_SECS // BASE_INTERVAL_SECS) + 2)

# Far apart so the drop cannot be confused with rounding, and both inside the
# range wire-sanity accepts, so the entry does not trip a checker instead.
HIGH_PERCENT = 80.0
LOW_PERCENT = 10.0

_state = {"served": 0}
_lock = threading.Lock()


def payload(used: float) -> bytes:
    reset_at = (
        datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=2)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    return json.dumps(
        {
            "isValid": True,
            "rate_limits": [
                {
                    "window": "5h",
                    "limit": 100.0,
                    "used": used,
                    "remaining": 100.0 - used,
                    "reset_at": reset_at,
                }
            ],
        }
    ).encode()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        with _lock:
            _state["served"] += 1
            n = _state["served"]

        if n == 1:
            body, note = payload(HIGH_PERCENT), f"healthy {HIGH_PERCENT}% (before)"
        elif n <= 1 + OUTAGE_REQUESTS:
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            left = 1 + OUTAGE_REQUESTS - n
            sys.stderr.write(
                f"  request {n}: 503 transient ({left} more, gap must exceed "
                f"{HORIZON_SECS}s)\n"
            )
            sys.stderr.flush()
            return
        else:
            body, note = payload(LOW_PERCENT), f"healthy {LOW_PERCENT}% (THE DROP)"

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        sys.stderr.write(f"  request {n}: {note}\n")
        sys.stderr.flush()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    sys.stderr.write(
        f"  horizon {HORIZON_SECS}s (BASE_INTERVAL {BASE_INTERVAL_SECS}s x "
        f"CONTINUOUS_MULTIPLIER {CONTINUOUS_MULTIPLIER}, both read from source)\n"
        f"  plan: 1 healthy at {HIGH_PERCENT}%, {OUTAGE_REQUESTS} x 503, "
        f"then healthy at {LOW_PERCENT}%\n"
        f"  listening on 127.0.0.1:8477\n"
    )
    sys.stderr.flush()
    HTTPServer(("127.0.0.1", 8477), Handler).serve_forever()
