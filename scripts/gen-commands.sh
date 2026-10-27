#!/usr/bin/env bash
# Regenerates crates/redcrab-core/src/commands/table.rs, the command table
# behind the read-only gate, the console and autocomplete, from the
# COMMAND INFO and COMMAND DOCS of a live Redis 8 server (only reads from it).
#
#   scripts/gen-commands.sh                      # throwaway redis:8-alpine container
#   scripts/gen-commands.sh redis://host:port    # an existing server
#
# The image bundles the modules (JSON, Search, TimeSeries, Bloom, vector
# sets), so their commands land in the table too. Set REDIS_IMAGE to use
# another image.
set -euo pipefail

cd "$(dirname "$0")/.."
out=crates/redcrab-core/src/commands/table.rs
url=${1:-}

if [[ -z $url ]]; then
    image=${REDIS_IMAGE:-redis:8-alpine}
    name=redcrab-gen-commands-$$
    # Bound to localhost on a random free port, removed on exit.
    docker run --detach --rm --name "$name" --publish 127.0.0.1::6379 "$image" >/dev/null
    trap 'docker rm --force "$name" >/dev/null 2>&1 || true' EXIT
    for _ in $(seq 100); do
        docker exec "$name" redis-cli ping 2>/dev/null | grep -q PONG && break
        sleep 0.1
    done
    port=$(docker port "$name" 6379/tcp | head -n 1 | sed 's/.*://')
    url=redis://127.0.0.1:$port
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"; [[ -n ${name:-} ]] && docker rm --force "$name" >/dev/null 2>&1 || true' EXIT
cargo run --quiet -p redcrab-core --example gen_commands -- "$url" >"$tmp"
mv "$tmp" "$out"
rustfmt --edition 2024 "$out"
echo "wrote $out from $url"
