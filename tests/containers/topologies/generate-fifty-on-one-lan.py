#!/usr/bin/env python3
"""Regenerate fifty-on-one-lan.json. Run this and commit its output; the
topology itself is not hand-edited.

Fifty nodes on one /24, deliberately 10.77.1.0/24 — the same subnet
two-on-one-lan.json and every other single-LAN topology in this directory
already uses — so that bringing this lab up beside one of those is the proof
two labs cannot collide: each is a bridge inside its own container
(`--network none`), never a network the Docker daemon itself routes.
"""

import json
from pathlib import Path

HOSTS = [
    {
        "name": f"node-a{i}",
        "lan": "home-a",
        "ip": f"10.77.1.{i + 10}",
        "role": "node",
        "listen": 7755,
        "proxy": 8088,
        "find": "on",
        "announceName": "on",
    }
    for i in range(1, 51)
]
HOSTS.append({"name": "observer", "lan": "home-a", "ip": "10.77.1.250", "role": "observer"})
HOSTS.append(
    {"name": "upstream", "lan": "home-a", "ip": "10.77.1.253", "role": "upstream", "listen": 8080}
)

TOPOLOGY = {
    "lans": [{"name": "home-a", "bridge": "br-home-a", "cidr": "10.77.1.0/24"}],
    "hosts": HOSTS,
}


def main():
    out = Path(__file__).parent / "fifty-on-one-lan.json"
    out.write_text(json.dumps(TOPOLOGY, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {len(HOSTS)} hosts to {out}")


if __name__ == "__main__":
    main()
