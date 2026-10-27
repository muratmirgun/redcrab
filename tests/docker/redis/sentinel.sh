#!/bin/sh
# Entry point of the `sentinel` test service: a master on 6410, its replica on
# 6411 and one Sentinel on 26410 that watches the master as `mymaster` with
# quorum 1. Everything announces 127.0.0.1 and the ports are published 1:1, so
# the addresses Sentinel hands out work from the host.
#
# State starts fresh on every container start (Sentinel rewrites its config
# file, e.g. after a failover).
set -eu
# shellcheck source=lib.sh # mounted at /redcrab
. /redcrab/lib.sh

ROOT=/data/sentinel
rm -rf "$ROOT"
mkdir -p "$ROOT/6410" "$ROOT/6411"

# shellcheck disable=SC2086 # MODULES is a list of words
spawn redis-server --port 6410 --dir "$ROOT/6410" \
  --save "" --protected-mode no $MODULES
# shellcheck disable=SC2086
spawn redis-server --port 6411 --dir "$ROOT/6411" \
  --replicaof 127.0.0.1 6410 \
  --replica-announce-ip 127.0.0.1 --replica-announce-port 6411 \
  --save "" --protected-mode no $MODULES
wait_for_ping 6410

# Sentinel needs a config file it can rewrite.
cat >"$ROOT/sentinel.conf" <<'EOF'
port 26410
protected-mode no
dir /data/sentinel
sentinel announce-ip 127.0.0.1
sentinel announce-port 26410
sentinel monitor mymaster 127.0.0.1 6410 1
sentinel down-after-milliseconds mymaster 5000
sentinel failover-timeout mymaster 15000
sentinel parallel-syncs mymaster 1
EOF
spawn redis-sentinel "$ROOT/sentinel.conf"

supervise
