#!/usr/bin/env python3
"""Ask the LAN who is running `_tcr-peer._tcp` and print every answer.

This exists because nothing in the CLI renders a discovered list: `tcr peer
find on` browses once and throws the result away (`src/main.rs`, `RealPeerFinder`),
and `tcr peer ls` lists pinned peers only. So a scenario that wants to know
whether a beacon crossed a network has to read the wire, which is what this
does: one multicast PTR query, then every packet that comes back for the window.

Stdlib only, no daemon, no dependency. Prints one line per answer:

    mdns: <source-ip>: <instance name>

and exits 0 when at least one answer arrived, 1 when none did.
"""

import socket
import struct
import sys
import time

GROUP = "224.0.0.251"
PORT = 5353
SERVICE = "_tcr-peer._tcp.local."
WINDOW = float(sys.argv[1]) if len(sys.argv) > 1 else 10.0
# Which interface to ask on, named by its own address.
#
# The default, INADDR_ANY, leaves the choice to the routing table, and a
# container on a Docker network marked `internal: true` has no default route at
# all: the group join then fails outright with ENODEV and the send with
# ENETUNREACH, on a bridge that forwards multicast perfectly well. Naming the
# address the harness assigned this container skips the routing table, which is
# the whole of the fix.
INTERFACE = sys.argv[2] if len(sys.argv) > 2 else "0.0.0.0"


def encode_name(name):
    out = b""
    for label in name.rstrip(".").split("."):
        out += bytes([len(label)]) + label.encode()
    return out + b"\x00"


def read_name(data, offset):
    labels = []
    hops = 0
    while True:
        if offset >= len(data):
            return "", offset
        length = data[offset]
        if length == 0:
            return ".".join(labels), offset + 1
        if length & 0xC0 == 0xC0:
            pointer = struct.unpack("!H", data[offset : offset + 2])[0] & 0x3FFF
            hops += 1
            if hops > 8:
                return ".".join(labels), offset + 2
            tail, _ = read_name(data, pointer)
            if tail:
                labels.append(tail)
            return ".".join(labels), offset + 2
        labels.append(data[offset + 1 : offset + 1 + length].decode("utf-8", "replace"))
        offset += 1 + length


query = struct.pack("!HHHHHH", 0, 0, 1, 0, 0, 0) + encode_name(SERVICE) + struct.pack("!HH", 12, 1)

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
sock.bind(("", PORT))
sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(INTERFACE))
sock.setsockopt(
    socket.IPPROTO_IP,
    socket.IP_ADD_MEMBERSHIP,
    struct.pack("4s4s", socket.inet_aton(GROUP), socket.inet_aton(INTERFACE)),
)
sock.settimeout(1.0)
sock.sendto(query, (GROUP, PORT))

deadline = time.time() + WINDOW
answers = 0
while time.time() < deadline:
    try:
        data, (source, _) = sock.recvfrom(9000)
    except socket.timeout:
        continue
    if len(data) < 12:
        continue
    counts = struct.unpack("!HHHHHH", data[:12])
    if counts[3] == 0 and counts[2] == 0:
        continue
    offset = 12
    for _ in range(counts[2]):
        _, offset = read_name(data, offset)
        offset += 4
    for _ in range(counts[3]):
        name, offset = read_name(data, offset)
        if offset + 10 > len(data):
            break
        rtype, _, _, rdlength = struct.unpack("!HHIH", data[offset : offset + 10])
        offset += 10
        body = data[offset : offset + rdlength]
        offset += rdlength
        if rtype == 12 and SERVICE.rstrip(".") in name:
            target, _ = read_name(data, len(data) - len(body) if False else offset - rdlength)
            print(f"mdns: {source}: {target}", flush=True)
            answers += 1

print(
    f"mdns: answers={answers} window={WINDOW}s service={SERVICE} interface={INTERFACE}",
    flush=True,
)
sys.exit(0 if answers else 1)
