#!/bin/sh
# Health check of the `sentinel` service. Healthy once Sentinel reports
# mymaster at 127.0.0.1:6410, sees it up, has discovered the replica at
# 127.0.0.1:6411 with a working link, and can reach its quorum.
set -eu

fail() {
  echo "sentinel: $*"
  exit 1
}

# Prints the value after the line "$1" in a flat field/value reply.
field() {
  awk -v key="$1" 'prev == key { print; exit } { prev = $0 }'
}

addr=$(redis-cli -p 26410 --raw sentinel get-master-addr-by-name mymaster 2>&1 | tr -d '\r' | tr '\n' ' ')
[ "$addr" = "127.0.0.1 6410 " ] || fail "mymaster is at '$addr', want 127.0.0.1 6410"

master=$(redis-cli -p 26410 --raw sentinel master mymaster 2>&1 | tr -d '\r')
flags=$(echo "$master" | field flags)
[ "$flags" = master ] || fail "mymaster flags are '$flags'"

replicas=$(redis-cli -p 26410 --raw sentinel replicas mymaster 2>&1 | tr -d '\r')
name=$(echo "$replicas" | field name)
flags=$(echo "$replicas" | field flags)
link=$(echo "$replicas" | field master-link-status)
[ "$name" = 127.0.0.1:6411 ] || fail "replica is '$name', want 127.0.0.1:6411"
[ "$flags" = slave ] || fail "replica flags are '$flags'"
[ "$link" = ok ] || fail "replica link is '$link'"

redis-cli -p 6411 info replication 2>&1 | tr -d '\r' | grep -qx 'master_link_status:up' ||
  fail "replica 6411: link to its master is down"

quorum=$(redis-cli -p 26410 sentinel ckquorum mymaster 2>&1)
case "$quorum" in
  OK*) ;;
  *) fail "ckquorum: $quorum" ;;
esac
