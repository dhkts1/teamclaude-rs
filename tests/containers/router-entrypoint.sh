#!/bin/sh
# Become the NAT between one inside network and one outside network, then stay
# up. Optionally serve UPnP, never NAT-PMP.
#
# Env:
#   INSIDE_SUBNET   CIDR of the home network, e.g. 10.77.1.0/24 (required)
#   OUTSIDE_SUBNET  CIDR of the internet network, e.g. 10.77.9.0/24 (required)
#   UPNP            `on` to run miniupnpd on the outside interface
#
# The interfaces are DERIVED from the subnets, never assumed to be eth0/eth1:
# Docker attaches a container's networks in an order this harness does not
# control, so a scenario that hard-coded the names would pass or fail by luck.
set -eu

: "${INSIDE_SUBNET:?INSIDE_SUBNET is required}"
: "${OUTSIDE_SUBNET:?OUTSIDE_SUBNET is required}"
UPNP="${UPNP:-off}"

iface_for() {
  # $1: a CIDR. Prints the interface holding an address inside it.
  prefix="${1%.*/*}."
  ip -o -4 addr show | awk -v want="$prefix" '$4 ~ "^"want {print $2; exit}'
}

wait_for_iface() {
  # $1: a CIDR. Prints its interface once it exists, or nothing after 30 s.
  # A container started with `docker run` holds one network and gets the second
  # from a later `network connect`, so the interface a scenario needs can be
  # seconds behind this process. Compose attaches both at start and hits this
  # loop once.
  i=0
  while [ "$i" -lt 30 ]; do
    found="$(iface_for "$1")"
    if [ -n "$found" ]; then
      echo "$found"
      return 0
    fi
    i=$((i + 1))
    sleep 1
  done
  return 1
}

INSIDE_IF="$(wait_for_iface "$INSIDE_SUBNET" || true)"
OUTSIDE_IF="$(wait_for_iface "$OUTSIDE_SUBNET" || true)"
if [ -z "$INSIDE_IF" ] || [ -z "$OUTSIDE_IF" ]; then
  echo "router: FAIL: no interface for inside=$INSIDE_SUBNET ($INSIDE_IF) outside=$OUTSIDE_SUBNET ($OUTSIDE_IF)" >&2
  ip -o -4 addr show >&2
  exit 1
fi

# `docker run --sysctl net.ipv4.ip_forward=1` (and compose's `sysctls:`) sets
# this at creation and leaves /proc/sys read-only, so writing it here fails on a
# correctly-started container. Read first, write only if it is off, and refuse
# loudly rather than NAT with forwarding disabled.
if [ "$(cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo 0)" != "1" ]; then
  sysctl -w net.ipv4.ip_forward=1 >/dev/null 2>&1 || {
    echo "router: FAIL: ip_forward is off and this container cannot set it; start it with sysctls net.ipv4.ip_forward=1" >&2
    exit 1
  }
fi
iptables -t nat -A POSTROUTING -s "$INSIDE_SUBNET" -o "$OUTSIDE_IF" -j MASQUERADE
iptables -A FORWARD -i "$INSIDE_IF" -o "$OUTSIDE_IF" -j ACCEPT
iptables -A FORWARD -i "$OUTSIDE_IF" -o "$INSIDE_IF" -m state --state RELATED,ESTABLISHED -j ACCEPT

echo "router: up: inside=$INSIDE_IF($INSIDE_SUBNET) outside=$OUTSIDE_IF($OUTSIDE_SUBNET) upnp=$UPNP natpmp=off"

if [ "$UPNP" = "on" ]; then
  cat > /etc/miniupnpd/miniupnpd.conf <<CONF
ext_ifname=${OUTSIDE_IF}
listening_ip=${INSIDE_IF}
enable_natpmp=no
enable_upnp=yes
secure_mode=yes
system_uptime=yes
CONF

  # miniupnpd does not create its own chains; it errors "chain MINIUPNPD not
  # found" on the first AddPortMapping if they are missing. This is what its
  # own miniupnpd/netfilter/iptables_init.sh (miniupnpd_2_3_3 tag) sets up
  # for the same three chains and jumps:
  #   iptables -t nat -N $CHAIN
  #   iptables -t nat -A PREROUTING -i $EXTIF -j $CHAIN
  #   iptables -t filter -N MINIUPNPD
  #   iptables -t filter -A FORWARD -i $EXTIF ! -o $EXTIF -j $CHAIN
  #   iptables -t nat -N $CHAIN-POSTROUTING
  #   iptables -t nat -A POSTROUTING -o $EXTIF -j $CHAIN-POSTROUTING
  #
  # `iptables-legacy`, not the plain `iptables` above: this image's `iptables`
  # is the nft-compat build (linked against libnftnl, managing nftables over
  # netlink), while miniupnpd links against libip4tc, Alpine's legacy library
  # that talks to the `ip_tables` kernel module directly. The two do not share
  # state, so a chain the first creates reads as absent to the second
  # ("chain MINIUPNPD not found" on the first AddPortMapping, every time,
  # however the chain was made); `iptables-legacy` writes where miniupnpd
  # reads.
  iptables-legacy -t nat -N MINIUPNPD
  iptables-legacy -t nat -A PREROUTING -i "$OUTSIDE_IF" -j MINIUPNPD
  iptables-legacy -t filter -N MINIUPNPD
  iptables-legacy -t filter -A FORWARD -i "$OUTSIDE_IF" ! -o "$OUTSIDE_IF" -j MINIUPNPD
  iptables-legacy -t nat -N MINIUPNPD-POSTROUTING
  iptables-legacy -t nat -A POSTROUTING -o "$OUTSIDE_IF" -j MINIUPNPD-POSTROUTING

  miniupnpd -d -f /etc/miniupnpd/miniupnpd.conf &
  echo "router: upnp: miniupnpd serving on ${INSIDE_IF}, natpmp refused"
fi

# Nothing to do but hold the namespace open.
while true; do
  sleep 3600
done
