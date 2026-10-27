#!/bin/sh
# Entry point of the `cluster` test service: six redis-server processes on
# ports 7000-7005 joined into one cluster of 3 masters with a replica each.
# Every node announces 127.0.0.1 and the ports are published 1:1, so MOVED/ASK
# redirects and CLUSTER SLOTS/SHARDS replies are usable from the host.
#
# The cluster is built from scratch on every container start: nodes.conf files
# left by an earlier run would describe a cluster that no longer exists.
set -eu
# shellcheck source=lib.sh # mounted at /redcrab
. /redcrab/lib.sh

PORTS="7000 7001 7002 7003 7004 7005"
ROOT=/data/cluster
rm -rf "$ROOT"

for port in $PORTS; do
  mkdir -p "$ROOT/$port"
  # shellcheck disable=SC2086 # MODULES is a list of words
  spawn redis-server --port "$port" --dir "$ROOT/$port" \
    --cluster-enabled yes --cluster-config-file nodes.conf \
    --cluster-announce-ip 127.0.0.1 --cluster-node-timeout 5000 \
    --save "" --protected-mode no $MODULES
done

nodes=""
for port in $PORTS; do
  wait_for_ping "$port"
  nodes="$nodes 127.0.0.1:$port"
done

# shellcheck disable=SC2086 # nodes is a list of words
if ! redis-cli --cluster create $nodes --cluster-replicas 1 --cluster-yes; then
  echo "creating the cluster failed" >&2
  stop 1
fi

supervise
