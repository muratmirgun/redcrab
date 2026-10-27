# Shared by cluster.sh and sentinel.sh, which run several Redis processes in
# one container (sourced, not executed; busybox sh).

# The modules the stock redis:8 entrypoint loads (JSON, Search, TimeSeries,
# Bloom), so these servers behave like the single-process test services.
MODULES=""
for module in /usr/local/lib/redis/modules/*.so; do
  if [ -s "$module" ]; then
    MODULES="$MODULES --loadmodule $module"
  fi
done

PIDS=""

# Starts a background process and remembers its PID for stop/supervise.
spawn() {
  "$@" &
  PIDS="$PIDS $!"
}

# Stops every spawned process and exits with $1 (docker stop, or a crash).
stop() {
  trap - TERM INT
  # shellcheck disable=SC2086 # PIDS is a list of words
  kill -TERM $PIDS 2>/dev/null || :
  wait
  exit "${1:-0}"
}
trap 'stop 0' TERM INT

wait_for_ping() {
  until redis-cli -p "$1" ping 2>/dev/null | grep -q PONG; do
    sleep 0.2
  done
}

# Runs until docker stops the container. If any process dies, stops the rest
# and exits non-zero so the restart policy starts the service over.
supervise() {
  while :; do
    for pid in $PIDS; do
      if ! kill -0 "$pid" 2>/dev/null; then
        echo "process $pid exited; stopping the service" >&2
        stop 1
      fi
    done
    # A background sleep lets the TERM trap run without waiting it out.
    sleep 2 &
    wait $! || :
  done
}
