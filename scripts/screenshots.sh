#!/usr/bin/env bash
# Renders the README's screenshots into docs/images: the desktop app in a
# window off the screen (macOS only; `redcrab --screenshots`, see
# crates/redcrab-gui/src/screenshots.rs) and the terminal app as SVG,
# against a throwaway Redis in Docker that scripts/screenshots/seed.py
# fills with a small shop's data. Nothing shows on the screen, and the
# saved connections and passwords are left alone.
#
#   scripts/screenshots.sh [folder]     # docs/images by default
set -euo pipefail
cd "$(dirname "$0")/.."

NAME=redcrab-screenshots
PORT=6420
OUT=${1:-docs/images}

work=$(mktemp -d "${TMPDIR:-/tmp}/redcrab-screenshots.XXXXXX")
load=""
cleanup() {
  if [ -n "$load" ]; then
    kill "$load" 2>/dev/null || true
    wait "$load" 2>/dev/null || true
  fi
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "${work:?}"
}
trap cleanup EXIT

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p "127.0.0.1:$PORT:$PORT" redis:8-alpine \
  redis-server --port "$PORT" --save "" --protected-mode no >/dev/null
until docker exec "$NAME" redis-cli -p "$PORT" ping 2>/dev/null | grep -q PONG; do sleep 0.2; done
python3 -I scripts/screenshots/seed.py data | docker exec -i "$NAME" redis-cli -p "$PORT" --pipe >/dev/null

# Traffic in bursts, for the server tools' charts.
python3 -I scripts/screenshots/seed.py load 4000 >"$work/load.resp"
(
  while true; do
    docker exec -i "$NAME" redis-cli -p "$PORT" --pipe <"$work/load.resp" >/dev/null
    sleep "$((RANDOM % 3)).$((RANDOM % 10))"
  done
) &
load=$!

mkdir -p "$OUT"
# Absolute: the tests run in their crate's folder.
OUT=$(cd "$OUT" && pwd)
export REDCRAB_CONFIG_DIR="$work/config" REDCRAB_CREDENTIAL_SERVICE=redcrab-screenshots
cargo run -q -p redcrab-gui --features screenshots -- \
  --screenshots "$OUT" "redis://127.0.0.1:$PORT"
REDCRAB_SCREENSHOTS="$OUT" REDCRAB_SCREENSHOTS_URL="redis://127.0.0.1:$PORT" \
  cargo test -q -p redcrab-tui screenshots -- --ignored
