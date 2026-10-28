#!/bin/sh
# Health check of the `sentinel` service. Healthy once Sentinel reports
# mymaster at 127.0.0.1:6410 or 127.0.0.1:6411, sees it up, has discovered
# the other one as its replica with a working link, and can reach its
# quorum. Either node may be the master: redcrab's failover test switches
# them (and switches back), and a run cut short may leave 6411 in charge.
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
case "$addr" in
  "127.0.0.1 6410 ") replica=6411 ;;
  "127.0.0.1 6411 ") replica=6410 ;;
  *) fail "mymaster is at '$addr', want 127.0.0.1 6410 or 6411" ;;
esac

master=$(redis-cli -p 26410 --raw sentinel master mymaster 2>&1 | tr -d '\r')
flags=$(echo "$master" | field flags)
[ "$flags" = master ] || fail "mymaster flags are '$flags'"

replicas=$(redis-cli -p 26410 --raw sentinel replicas mymaster 2>&1 | tr -d '\r')
name=$(echo "$replicas" | field name)
flags=$(echo "$replicas" | field flags)
link=$(echo "$replicas" | field master-link-status)
[ "$name" = "127.0.0.1:$replica" ] || fail "replica is '$name', want 127.0.0.1:$replica"
[ "$flags" = slave ] || fail "replica flags are '$flags'"
[ "$link" = ok ] || fail "replica link is '$link'"

redis-cli -p "$replica" info replication 2>&1 | tr -d '\r' | grep -qx 'master_link_status:up' ||
  fail "replica $replica: link to its master is down"

quorum=$(redis-cli -p 26410 sentinel ckquorum mymaster 2>&1)
case "$quorum" in
  OK*) ;;
  *) fail "ckquorum: $quorum" ;;
esac
