#!/bin/sh
# Health check of the `cluster` service. Healthy once every node reports
# cluster_state:ok with all slots served, the cluster has 3 masters owning
# slots and 3 connected replicas, every node announces 127.0.0.1:700x (what
# MOVED redirects point at), every replica's link to its master is up, and
# every node's CLUSTER SLOTS lists all six nodes.
set -eu

fail() {
  echo "cluster: $*"
  exit 1
}

PORTS="7000 7001 7002 7003 7004 7005"

for port in $PORTS; do
  info=$(redis-cli -p "$port" cluster info 2>&1 | tr -d '\r')
  for want in cluster_state:ok cluster_slots_ok:16384 cluster_known_nodes:6 cluster_size:3; do
    echo "$info" | grep -qx "$want" || fail "node $port: no $want"
  done
done

nodes=$(redis-cli -p 7000 cluster nodes 2>&1 | tr -d '\r')
# Fields: id addr flags master ping-sent pong-recv epoch link-state slot...
masters=$(echo "$nodes" | awk '$3 ~ /master/ && $3 !~ /fail/ && $8 == "connected" && NF >= 9 { n++ } END { print n + 0 }')
replicas=$(echo "$nodes" | awk '$3 ~ /slave/ && $3 !~ /fail/ && $8 == "connected" { n++ } END { print n + 0 }')
[ "$masters" -eq 3 ] || fail "$masters masters with slots, want 3"
[ "$replicas" -eq 3 ] || fail "$replicas connected replicas, want 3"
foreign=$(echo "$nodes" | awk '$2 !~ /^127\.0\.0\.1:700[0-5]@1700[0-5]/ { n++ } END { print n + 0 }')
[ "$foreign" -eq 0 ] || fail "$foreign nodes do not announce 127.0.0.1:700x"

for port in $PORTS; do
  repl=$(redis-cli -p "$port" info replication 2>&1 | tr -d '\r')
  if echo "$repl" | grep -qx 'role:slave'; then
    echo "$repl" | grep -qx 'master_link_status:up' || fail "replica $port: link to its master is down"
  fi
  # CLUSTER SLOTS (how clients such as redis-rs discover nodes) leaves a replica
  # out until gossip has carried its replication offset, which takes a moment
  # after the replica has synced. Every node must list all six.
  endpoints=$(redis-cli -p "$port" cluster slots 2>&1 | tr -d '\r' |
    awk 'prev == "127.0.0.1" && !($0 in seen) { seen[$0] = 1; n++ } { prev = $0 } END { print n + 0 }')
  [ "$endpoints" -eq 6 ] || fail "node $port: CLUSTER SLOTS lists $endpoints of 6 nodes"
done
