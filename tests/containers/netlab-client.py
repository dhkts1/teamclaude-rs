#!/usr/bin/env python3
"""netlab-client: behave like Claude Code toward one node's own proxy.

Nothing else in this lab drives a node's proxy the way a real client does , 
every other scenario asks the `tcr` binary itself questions (`peer ls`,
`invite`, `join`) rather than sending it `POST /v1/messages`. This is that
missing half: a load generator that speaks the one HTTP shape the proxy
serves, from stdlib `http.client`, python3 is already in the image, and
`hey`/`oha` would be a new binary on every machine that runs this for a load
generator that is a hundred lines and has to print one line per request
either way.

Run as a process in the node's OWN namespace, never a namespace beside it:
`src/server.rs:1977-1979` binds `127.0.0.1`, so a client anywhere else could
not reach the proxy at all, and moving that bind is not this tool's call.

Usage:
    netlab-client.py <config.json> <rate-per-second> <body-bytes> <count>

<config.json> is the node's own teamclaude.json (this script reads
proxy.port, proxy.apiKey and accounts[0].name off it, nothing is passed
twice). One line per request on stdout:

    <request_id> <account> <status> <latency-ms>
"""

import http.client
import json
import sys
import time
import uuid


def main():
    if len(sys.argv) != 5:
        print(__doc__, file=sys.stderr)
        return 2
    config_path, rate, body_bytes, count = sys.argv[1:]
    rate = float(rate)
    body_bytes = int(body_bytes)
    count = int(count)

    with open(config_path, encoding="utf-8") as f:
        config = json.load(f)
    port = config.get("proxy", {}).get("port", 8088)
    api_key = config.get("proxy", {}).get("apiKey", "")
    account = config.get("accounts", [{}])[0].get("name", "-")
    node = config.get("accounts", [{}])[0].get("name", "-").removesuffix("-fake")

    interval = 1.0 / rate if rate > 0 else 0
    filler = "x" * body_bytes
    body = json.dumps(
        {
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 64,
            "stream": True,
            "messages": [{"role": "user", "content": filler}],
        }
    ).encode()

    for i in range(count):
        request_id = f"{node}-{uuid.uuid4().hex[:8]}"
        start = time.monotonic()
        status = "error"
        # A retry on 503, honouring the proxy's own `retry-after`, capped:
        # a real client (this is standing in for one) does the same. `netlab`
        # itself waits for a REAL cross-namespace TCP connect to the upstream
        # stub before starting this process at all (`wait_reachable`), so
        # this is a backstop for the ordinary case a real client handles too,
        # not the boot race that gate exists for.
        for attempt in range(3):
            try:
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
                conn.request(
                    "POST",
                    "/v1/messages",
                    body=body,
                    headers={
                        "content-type": "application/json",
                        "x-api-key": api_key,
                        "x-netlab-node": node,
                        "x-netlab-request-id": request_id,
                    },
                )
                resp = conn.getresponse()
                resp.read()
                status = str(resp.status)
                retry_after = resp.getheader("retry-after")
                conn.close()
            except OSError as exc:
                status = f"error:{exc.__class__.__name__}"
                retry_after = None
            if status != "503" or attempt == 2:
                break
            time.sleep(min(float(retry_after or 0.5), 1.0))
        latency_ms = int((time.monotonic() - start) * 1000)
        print(f"{request_id} {account} {status} {latency_ms}", flush=True)
        if interval:
            remaining = interval - (time.monotonic() - start)
            if remaining > 0:
                time.sleep(remaining)
    return 0


if __name__ == "__main__":
    sys.exit(main())
