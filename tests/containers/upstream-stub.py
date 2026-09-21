#!/usr/bin/env python3
"""The stub upstream: one canned 200, a per-request ledger, and a lever to
make it misbehave on command.

The in-process end-to-end answers with an axum router in the test binary
(`tests/peer_e2e.rs`). Containers need the same answer from a separate address,
so this is that router with nothing else in it.

Two endpoints beside the canned one:

    GET  /_ledger   the whole request ledger, as a JSON array
    POST /_lever     {"account": "<credential>", "mode": "429"|"stall"|"drop",
                       "retryAfter": N}   arms a misbehaviour for the next
                       request(s) that carry that credential; {"mode": "off"}
                       clears it

Every ordinary request is recorded as one row: `{account, node, request_id,
status, tokens, ms}`. `account` is the credential this request carried
(`authorization` or `x-api-key`, whichever is set, the same value a lever is
armed against), `node` is the `x-netlab-node` header the client sent (measured:
`build_upstream_headers`, src/proxy.rs, forwards it, it is not in that
function's drop list, which is `x-api-key`, `authorization`,
`accept-encoding`, hop-by-hop headers and the mesh's own routing marker), and
`request_id` is the `x-netlab-request-id` header the client sent, or a counter
when a caller does not set one. `tokens` is not a real usage count, nothing
here runs a model, it is `len(body) // 4`, a fixed and documented estimate a
scenario can use to tell "some request landed" from "no request landed",
never to check real billing.

A third, disjoint protocol under `/drop/<name>`: the dead drop's own surface,
a `PUT`/`GET` key-value store, never mixed into the ledger or the lever
above. `PUT /drop/<name>` stores the raw body under `<name>`; `GET
/drop/<name>` answers the stored bytes, or 404 when nothing is there yet, the
same "absence is not an error" shape `DeadDropStore::get` expects. Both need
the same bearer this stub's other endpoints already read with `_credential`,
checked against `DROP_TOKEN`, an obviously-fake lab constant: a request
carrying anything else, or nothing, is refused with 401 before it touches
`DROPS`.

Stdlib only, so the image is the stock python and there is nothing to build:
the stub runs in a namespace of the one netlab image, so a dependency here
would be a dependency in the node image too.

Listens on 0.0.0.0:8080.
"""

import itertools
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BODY = json.dumps({"type": "message", "id": "msg_fake"}).encode()

# The dead drop's own bearer, obviously fake: a scenario configures
# `tcr peer drop-store set http://.../drop/{name} --token DROP_TOKEN` and
# nothing this lab handles is a real credential.
DROP_TOKEN = "fake-netlab-drop-token"

LOCK = threading.Lock()
LEDGER = []
LEVERS = {}
DROPS = {}
REQUEST_IDS = itertools.count(1)


class Stub(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    # --- the ledger and the lever, read and armed by a scenario ------------

    def _serve_ledger(self):
        with LOCK:
            body = json.dumps(LEDGER).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _serve_lever(self, raw):
        # `raw` is the body `_answer` already read off `self.rfile` before it
        # knew which endpoint this was: a second `rfile.read()` here would
        # block waiting for bytes the client already sent and is not sending
        # again, which is exactly the hang a lever call produced before this
        # took a parameter instead of reading twice.
        try:
            req = json.loads(raw)
        except json.JSONDecodeError:
            req = {}
        account = req.get("account", "")
        mode = req.get("mode", "off")
        with LOCK:
            if mode == "off" or not account:
                LEVERS.pop(account, None)
            else:
                LEVERS[account] = {
                    "mode": mode,
                    "retryAfter": int(req.get("retryAfter", 1)),
                }
        body = json.dumps({"ok": True, "account": account, "mode": mode}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # --- the dead drop: a disjoint PUT/GET key-value surface ---------------

    def _drop_authorized(self):
        return self._credential() == f"Bearer {DROP_TOKEN}"

    def _serve_drop_put(self, name, raw):
        if not self._drop_authorized():
            self.send_response(401)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        with LOCK:
            DROPS[name] = raw
        self.send_response(204)
        self.send_header("content-length", "0")
        self.end_headers()

    def _serve_drop_get(self, name):
        if not self._drop_authorized():
            self.send_response(401)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        with LOCK:
            body = DROPS.get(name)
        if body is None:
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("content-type", "application/octet-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # --- the ordinary canned answer, and what a lever does to it -----------

    def _credential(self):
        return self.headers.get("authorization", "") or self.headers.get("x-api-key", "")

    def _record(self, account, node, request_id, status, tokens, ms):
        row = {
            "account": account,
            "node": node,
            "request_id": request_id,
            "status": status,
            "tokens": tokens,
            "ms": ms,
        }
        with LOCK:
            LEDGER.append(row)
        print(
            f"upstream: {self.command} {self.path} account={account} node={node} "
            f"request_id={request_id} status={status} tokens={tokens} ms={ms}",
            flush=True,
        )

    def _answer(self):
        start = time.monotonic()
        length = int(self.headers.get("content-length", "0") or 0)
        body_in = self.rfile.read(length) if length else b""
        account = self._credential()
        node = self.headers.get("x-netlab-node", "-")
        request_id = self.headers.get("x-netlab-request-id") or f"gen-{next(REQUEST_IDS)}"
        tokens = len(body_in) // 4

        if self.path.startswith("/_ledger") and self.command == "GET":
            self._serve_ledger()
            return
        if self.path.startswith("/_lever") and self.command == "POST":
            self._serve_lever(body_in)
            return
        if self.path.startswith("/drop/") and self.command in ("PUT", "GET"):
            drop_name = self.path[len("/drop/") :]
            if self.command == "PUT":
                self._serve_drop_put(drop_name, body_in)
            else:
                self._serve_drop_get(drop_name)
            return

        with LOCK:
            lever = LEVERS.get(account)

        if lever and lever["mode"] == "drop":
            # A connection closed without a response: no send_response, no
            # headers, just stop. The client sees a reset, never a status.
            self._record(account, node, request_id, "dropped", tokens, self._ms(start))
            self.close_connection = True
            return

        if lever and lever["mode"] == "429":
            body = json.dumps(
                {"type": "error", "error": {"type": "rate_limit_error", "message": "stubbed 429"}}
            ).encode()
            self.send_response(429)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.send_header("retry-after", str(lever["retryAfter"]))
            self.end_headers()
            self.wfile.write(body)
            self._record(account, node, request_id, 429, tokens, self._ms(start))
            return

        if lever and lever["mode"] == "stall":
            # A stream that stalls mid-body: headers and half the canned body
            # go out, then nothing more, ever. The connection stays open
            # rather than closing, which is the one thing that tells a client
            # timeout apart from a drop.
            half = BODY[: len(BODY) // 2]
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.end_headers()
            self.wfile.write(half)
            self.wfile.flush()
            self._record(account, node, request_id, "stalled", tokens, self._ms(start))
            while True:
                time.sleep(3600)

        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(BODY)))
        self.send_header("anthropic-ratelimit-unified-status", "allowed")
        self.send_header("anthropic-ratelimit-unified-5h-utilization", "0.10")
        self.send_header("anthropic-ratelimit-unified-7d-utilization", "0.10")
        self.send_header("anthropic-ratelimit-unified-7d_oi-utilization", "0.10")
        self.end_headers()
        self.wfile.write(BODY)
        self._record(account, node, request_id, 200, tokens, self._ms(start))

    @staticmethod
    def _ms(start):
        return int((time.monotonic() - start) * 1000)

    do_GET = _answer
    do_POST = _answer
    do_PUT = _answer

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    print(f"upstream: listening on 0.0.0.0:{port}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Stub).serve_forever()
