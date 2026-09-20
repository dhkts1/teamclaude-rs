#!/bin/sh
# native-inner.sh: what one `unshare --net --mount --pid --fork --mount-proc`
# does for one scenario under `peer-containers.sh --native`, the same three
# things one `docker run -d --network none --cap-add NET_ADMIN --cap-add
# SYS_ADMIN` plus `docker exec` gives a scenario under `--netlab`:
#
#   - a private network (the unshare flag, nothing here)
#   - a private mount table, so /lab-src and /lab mean exactly what
#     Dockerfile.netlab makes them mean, and neither a scenario nor
#     lib/netlab.sh nor lib/assert.sh changes by one character
#   - a private pid table, so a scenario that dies mid-way cannot leave a
#     `tcr` process behind on the runner
#
# Never invoked directly. peer-containers.sh execs it as the unshare target,
# by which point the network, mount and pid namespaces already exist and this
# process is pid 1 of the pid namespace.
#
# $1 scenario name, $2 the tests/containers directory on the host, $3 the
# directory holding the tcr binary this run was built with.
set -eu

name="$1"
containers="$2"
tcr_dir="$3"

# /lab-src and /lab already exist as empty directories: peer-containers.sh
# creates them once, outside every namespace, because a bind mount needs a
# target that is there before the mount syscall runs. The mounts below are
# private to this mount namespace and never touch the runner's own /lab-src
# or /lab.
mount --bind "$containers" /lab-src
mount -t tmpfs tmpfs /lab
# netlab down (tests/containers/netlab:465-467) deletes every named netns it
# can see. Without a private /run/netns a native run on somebody's Linux box
# would delete their namespaces too.
mkdir -p /run/netns
mount -t tmpfs tmpfs /run/netns
# A fresh net namespace comes up with loopback down; a container does not.
ip link set lo up

PATH="$tcr_dir:$PATH"
export PATH
NETLAB=1
export NETLAB

exec /lab-src/scenarios/"$name".sh
