#!/usr/bin/env python3
"""The stub upstream: one canned 200, and a record of which credential asked.

The in-process end-to-end answers with an axum router in the test binary
(`tests/peer_e2e.rs`). Containers need the same answer from a separate address,
so this is that router with nothing else in it: every request gets the canned
message body and the rate-limit headers the proxy reads, and every request's
Authorization / x-api-key is appended to a log the scenarios read back to prove
whose credential served a borrow.

Stdlib only, so the image is the stock python alpine and there is nothing to
build. Listens on 0.0.0.0:8080.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SEEN = "/seen.log"
BODY = json.dumps({"type": "message", "id": "msg_fake"}).encode()


class Stub(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _record(self):
        credential = self.headers.get("authorization", "") or self.headers.get("x-api-key", "")
        with open(SEEN, "a", encoding="utf-8") as log:
            log.write(f"{self.command} {self.path} credential={credential}\n")
        print(f"upstream: {self.command} {self.path} credential={credential}", flush=True)

    def _answer(self):
        length = int(self.headers.get("content-length", "0") or 0)
        if length:
            self.rfile.read(length)
        self._record()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(BODY)))
        self.send_header("anthropic-ratelimit-unified-status", "allowed")
        self.send_header("anthropic-ratelimit-unified-5h-utilization", "0.10")
        self.send_header("anthropic-ratelimit-unified-7d-utilization", "0.10")
        self.send_header("anthropic-ratelimit-unified-7d_oi-utilization", "0.10")
        self.end_headers()
        self.wfile.write(BODY)

    do_GET = _answer
    do_POST = _answer

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    print(f"upstream: listening on 0.0.0.0:{port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Stub).serve_forever()
