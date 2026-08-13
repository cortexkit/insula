#!/usr/bin/env python3
"""Make a provider fail transiently on purpose, so preserve-the-window
behaviour can be witnessed on the real wire instead of waited for.

Serves a healthy sub2api-shaped payload for the first N requests, then 503
forever. 503 classifies as transient, so the refresher keeps serving the last
good window and publishes the `stale` disclosure beside it.

WHY THIS EXISTS. A field that only appears during an upstream failure cannot be
verified by watching a healthy host: "no entry claims to be stale" is what a
correct field and a field that never populates both look like. Waiting for a
real outage means the first live read happens during an incident, which is the
worst moment to discover the shape is wrong.

`sub2api` is the lever because it accepts a LOOPBACK HTTP base URL by
environment (`SUB2API_BASE_URL`, validated to https-or-loopback) — a real
shipped provider pointed at a server you control, with no production credential
or network involved. The same trick reaches any transient-class behaviour, not
just this one field.

USAGE, against the deployed module:

    python3 scripts/witness-transient.py 1 &
    # add to the insula block of ~/.config/cortexkit/subc.jsonc:
    #   "env": { "SUB2API_API_KEY": "witness-key",
    #            "SUB2API_BASE_URL": "http://127.0.0.1:8477/v1" }
    ck module rescan && ck module restart insula

A RESCAN IS REQUIRED, not just a restart: a restart alone respawns with the
previously-loaded config and the new environment never reaches the process.

BACK UP subc.jsonc FIRST and restore it afterwards, then verify the running
process carries no SUB2API vars — a lever left in place silently replaces a real
provider with a fixture.
"""
import datetime
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

HEALTHY_REQUESTS = int(sys.argv[1]) if len(sys.argv) > 1 else 1
_state = {"served": 0}
_lock = threading.Lock()
RESET_AT = (
    datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(hours=2)
).strftime("%Y-%m-%dT%H:%M:%SZ")

PAYLOAD = {
    "isValid": True,
    "rate_limits": [
        {
            "window": "5h",
            "limit": 100.0,
            "used": 42.0,
            "remaining": 58.0,
            "reset_at": RESET_AT,
        }
    ],
}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        with _lock:
            _state["served"] += 1
            n = _state["served"]
        if n <= HEALTHY_REQUESTS:
            body = json.dumps(PAYLOAD).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            sys.stderr.write(f"  request {n}: served healthy window\n")
        else:
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            sys.stderr.write(f"  request {n}: 503 (transient)\n")
        sys.stderr.flush()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", 8477), Handler)
    sys.stderr.write(f"  listening on 127.0.0.1:8477, healthy for {HEALTHY_REQUESTS} request(s)\n")
    sys.stderr.flush()
    server.serve_forever()
