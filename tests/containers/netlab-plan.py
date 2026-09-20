#!/usr/bin/env python3
"""Read a topology file and print a flat plan the sh builder executes.

JSON rather than YAML: python3 is already in the image and `json` is stdlib,
while any YAML reader is a new dependency for a file format nobody reads under
pressure. The plan is lines rather than a data structure because the thing
that executes it is a POSIX shell.

Lines, in the order they must be applied:

    lan   <name> <bridge> <cidr>
    host  <name> <lan> <ip> <prefixlen> <gw|-> <role> <listen-port> <proxy-port> <find> <announce>
    extra <name> <lan> <ip> <prefixlen>

A host's `extra` list is a second (or third) veth into a namespace `host`
already created, for the one cast namespace membership does not cover: a Mac
with a LAN address and an overlay address at once. The primary line still
carries the role, the ports and the discovery opt-ins; an extra line carries
nothing but where the wire goes.
"""

import json
import sys


def main(path):
    topo = json.load(open(path, encoding="utf-8"))
    for lan in topo["lans"]:
        print(f"lan {lan['name']} {lan['bridge']} {lan['cidr']}")
    for host in topo["hosts"]:
        print(
            "host {name} {lan} {ip} {prefix} {gw} {role} {listen} {proxy} {find} {announce}".format(
                name=host["name"],
                lan=host["lan"],
                ip=host["ip"],
                prefix=host.get("prefix", 24),
                gw=host.get("gw", "-"),
                role=host.get("role", "node"),
                listen=host.get("listen", 7755),
                proxy=host.get("proxy", 8088),
                find=host.get("find", "off"),
                announce=host.get("announceName", "off"),
            )
        )
        for net in host.get("extra", []):
            print(
                "extra {name} {lan} {ip} {prefix}".format(
                    name=host["name"],
                    lan=net["lan"],
                    ip=net["ip"],
                    prefix=net.get("prefix", 24),
                )
            )


if __name__ == "__main__":
    main(sys.argv[1])
