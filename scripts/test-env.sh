#!/bin/sh
# Runs the Docker services that redcrab's integration tests talk to
# (tests/docker/compose.yml, compose project "redcrab-it").
#
#   scripts/test-env.sh up      generate missing secrets, start every service and
#                               return once each one is really usable
#   scripts/test-env.sh down    remove the containers (the secrets stay)
#   scripts/test-env.sh status  each service's health and endpoint; exits
#                               non-zero unless all of them are healthy
#   scripts/test-env.sh env     the variables the tests read, for
#                               eval "$(scripts/test-env.sh env)"
#
# Every command is idempotent and safe to run concurrently. Secrets (a
# throwaway CA, server and client certificates, SSH keys) live in
# tests/docker/.generated/, which is gitignored; delete it to get new ones.
# Needs docker with the compose plugin, openssl and ssh-keygen. POSIX sh.
# REDCRAB_IT_WAIT_TIMEOUT (seconds, default 300) bounds how long `up` waits.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
DOCKER_DIR=$ROOT/tests/docker
GEN=$DOCKER_DIR/.generated
LOCK=$GEN/.lock
PROJECT=redcrab-it
SERVICES="standalone auth tls tls-mtls cluster sentinel ssh-redis ssh"
WAIT_TIMEOUT=${REDCRAB_IT_WAIT_TIMEOUT:-300}

die() {
  echo "test-env: $*" >&2
  exit 1
}

note() {
  echo "test-env: $*" >&2
}

usage() {
  echo "usage: scripts/test-env.sh {up|down|status|env}"
}

compose() {
  docker compose -p "$PROJECT" -f "$DOCKER_DIR/compose.yml" "$@"
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

need_docker() {
  need docker
  docker compose version >/dev/null 2>&1 ||
    die "the docker compose plugin (compose v2) is required"
  docker info >/dev/null 2>&1 || die "cannot reach the Docker daemon; is Docker running?"
}

# Runs a command, showing its output only when it fails.
quiet() {
  if ! out=$("$@" 2>&1); then
    echo "$out" >&2
    die "failed: $*"
  fi
}

# --- locking -----------------------------------------------------------------
# One test-env.sh at a time may generate secrets or change containers. mkdir is
# atomic; the lock records its owner's PID so a killed run does not wedge it.

locked=""

lock() {
  mkdir -p "$GEN"
  # Docker creates a missing bind-mount source as root on Linux, e.g. after a
  # plain `docker compose up`; this script must own the folder.
  [ -w "$GEN" ] || die "$GEN is not writable; remove it and run scripts/test-env.sh up"
  waited=0
  until mkdir "$LOCK" 2>/dev/null; do
    owner=$(cat "$LOCK/pid" 2>/dev/null || :)
    # The owner died, or never got to write its PID (10 s is plenty for that).
    if { [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; } ||
      { [ -z "$owner" ] && [ "$waited" -ge 10 ]; }; then
      note "removing a stale lock${owner:+ left by process $owner}"
      rm -rf "$LOCK"
      waited=0
    elif [ "$waited" -eq 0 ]; then
      note "waiting for another test-env.sh to finish"
    fi
    [ "$waited" -lt 900 ] || die "gave up waiting for $LOCK; remove it if no test-env.sh is running"
    sleep 1
    waited=$((waited + 1))
  done
  echo "$$" >"$LOCK/pid"
  locked=1
}

unlock() {
  if [ -n "$locked" ]; then
    rm -rf "$GEN"/.tmp.* "$LOCK"
    locked=""
  fi
}

trap unlock EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# --- secrets -----------------------------------------------------------------

TLS_FILES="ca.crt ca.key server.crt server.key client.crt client.key"
SSH_FILES="id_ed25519 id_ed25519.pub ssh_host_ed25519_key ssh_host_ed25519_key.pub known_hosts"

# True when the TLS set is complete, chains to its CA and is not about to expire.
tls_ok() {
  for f in $TLS_FILES; do
    [ -s "$GEN/$f" ] || return 1
  done
  for f in ca.crt server.crt client.crt; do
    openssl x509 -checkend 2592000 -noout -in "$GEN/$f" >/dev/null 2>&1 || return 1
  done
  verified=$(openssl verify -CAfile "$GEN/ca.crt" "$GEN/server.crt" "$GEN/client.crt" 2>&1) || return 1
  [ "$(echo "$verified" | grep -c ': OK$')" -eq 2 ]
}

# A throwaway CA, a server certificate for localhost and 127.0.0.1, and a client
# certificate. RSA 2048 and leaf lifetimes of 825 days keep rustls, OpenSSL and
# the macOS TLS stack all happy.
gen_tls() {
  note "generating a throwaway CA and TLS certificates in $GEN"
  tmp=$(mktemp -d "$GEN/.tmp.XXXXXX")
  cat >"$tmp/openssl.cnf" <<'EOF'
[req]
distinguished_name = dn
[dn]
[v3_ca]
basicConstraints = critical, CA:TRUE
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[v3_server]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost, IP:127.0.0.1
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid, issuer
[v3_client]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid, issuer
EOF
  for name in ca server client; do
    quiet openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$tmp/$name.key"
  done
  quiet openssl req -new -x509 -config "$tmp/openssl.cnf" -extensions v3_ca \
    -key "$tmp/ca.key" -subj "/CN=redcrab test CA" -days 3650 -sha256 -out "$tmp/ca.crt"
  for name in server client; do
    quiet openssl req -new -config "$tmp/openssl.cnf" -key "$tmp/$name.key" \
      -subj "/CN=redcrab-it-$name" -out "$tmp/$name.csr"
    quiet openssl x509 -req -in "$tmp/$name.csr" -CA "$tmp/ca.crt" -CAkey "$tmp/ca.key" \
      -CAcreateserial -CAserial "$tmp/ca.srl" -days 825 -sha256 \
      -extfile "$tmp/openssl.cnf" -extensions "v3_$name" -out "$tmp/$name.crt"
  done
  for f in $TLS_FILES; do
    mv -f "$tmp/$f" "$GEN/$f"
  done
  rm -rf "$tmp"
}

ssh_ok() {
  for f in $SSH_FILES; do
    [ -s "$GEN/$f" ] || return 1
  done
}

# The client key for user `tunnel`, the server's host key, and a known_hosts
# file that pins that host key for 127.0.0.1:2222 and localhost:2222.
gen_ssh() {
  note "generating SSH keys in $GEN"
  tmp=$(mktemp -d "$GEN/.tmp.XXXXXX")
  quiet ssh-keygen -q -t ed25519 -N '' -C redcrab-it-client -f "$tmp/id_ed25519"
  quiet ssh-keygen -q -t ed25519 -N '' -C redcrab-it-host -f "$tmp/ssh_host_ed25519_key"
  hostkey=$(cut -d ' ' -f 1,2 "$tmp/ssh_host_ed25519_key.pub")
  printf '[127.0.0.1]:2222 %s\n[localhost]:2222 %s\n' "$hostkey" "$hostkey" >"$tmp/known_hosts"
  for f in $SSH_FILES; do
    mv -f "$tmp/$f" "$GEN/$f"
  done
  rm -rf "$tmp"
}

# The containers run as other users (redis is uid 999), so everything they
# mount must be world-readable. The client's SSH key stays private: ssh refuses
# keys others can read, and the ssh container copies it as root.
fix_modes() {
  chmod 755 "$GEN"
  for f in $TLS_FILES ssh_host_ed25519_key ssh_host_ed25519_key.pub id_ed25519.pub known_hosts; do
    chmod 644 "$GEN/$f"
  done
  chmod 600 "$GEN/id_ed25519"
}

# --- containers --------------------------------------------------------------

# Prints "service state health" for every container of the project, e.g.
# "cluster running healthy" or "tls exited". Reads the compose labels rather
# than `compose ps --format`, whose fields vary between compose versions.
service_states() {
  ids=$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT")
  [ -n "$ids" ] || return 0
  # shellcheck disable=SC2086 # ids is a list of words
  docker inspect -f '{{index .Config.Labels "com.docker.compose.service"}} {{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' $ids 2>/dev/null || :
}

# Shows why services are not healthy: their last health check output and logs.
report() {
  compose ps -a >&2 || :
  states=$(service_states)
  for svc in $SERVICES; do
    if echo "$states" | grep -qx "$svc running healthy"; then
      continue
    fi
    id=$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" \
      --filter "label=com.docker.compose.service=$svc" | head -n 1)
    [ -n "$id" ] || continue
    echo "--- $svc: last health check" >&2
    docker inspect -f '{{if .State.Health}}{{range .State.Health.Log}}{{.Output}}{{end}}{{end}}' "$id" |
      tail -n 5 >&2 || :
    echo "--- $svc: last log lines" >&2
    compose logs --no-color --tail 20 "$svc" >&2 || :
  done
}

up() {
  need_docker
  need openssl
  need ssh-keygen
  lock

  # Services that read secrets at startup restart when they are regenerated.
  stale=""
  if ! tls_ok; then
    gen_tls
    stale="tls tls-mtls"
  fi
  if ! ssh_ok; then
    gen_ssh
    stale="$stale ssh"
  fi
  fix_modes

  # The ssh image is built locally; this is a no-op when nothing changed.
  if ! docker image inspect "$PROJECT-ssh" >/dev/null 2>&1; then
    note "building the ssh server image"
  fi
  quiet compose build ssh

  # Containers that loaded old secrets, or that are unhealthy or crash-looping,
  # start over; healthy ones are left alone. Missing ones are created below.
  recreate=""
  states=$(service_states)
  for svc in $SERVICES; do
    line=$(echo "$states" | awk -v s="$svc" '$1 == s { print; exit }')
    [ -n "$line" ] || continue
    case " $line " in
      *" unhealthy "* | *" restarting "* | *" dead "*) recreate="$recreate $svc" ;;
      *)
        case " $stale " in
          *" $svc "*) recreate="$recreate $svc" ;;
        esac
        ;;
    esac
  done
  if [ -n "$recreate" ]; then
    note "recreating:$recreate"
    # shellcheck disable=SC2086 # recreate is a list of words
    compose up -d --no-deps --force-recreate $recreate
  fi

  if ! compose up -d --wait --wait-timeout "$WAIT_TIMEOUT" --remove-orphans; then
    report
    die "the test environment did not become healthy (see above)"
  fi
  unlock
  status
}

down() {
  need_docker
  lock
  compose down --remove-orphans --volumes --timeout 10
  unlock
}

status() {
  need_docker
  states=$(service_states)
  unhealthy=""
  printf '%-10s  %-9s  %s\n' SERVICE STATE ENDPOINT
  for svc in $SERVICES; do
    line=$(echo "$states" | awk -v s="$svc" '$1 == s { print; exit }')
    state=$(echo "$line" | awk '{ print ($2 == "running" && $3 != "") ? $3 : $2 }')
    state=${state:-missing}
    [ "$state" = healthy ] || unhealthy="$unhealthy $svc"
    case $svc in
      standalone) endpoint="redis://127.0.0.1:6400" ;;
      auth) endpoint="redis://127.0.0.1:6401  password rootpass; ACL user app/apppass (app:* only)" ;;
      tls) endpoint="rediss://127.0.0.1:6402  CA ca.crt; client certificate optional" ;;
      tls-mtls) endpoint="rediss://127.0.0.1:6403  CA ca.crt; client.crt + client.key required" ;;
      cluster) endpoint="127.0.0.1:7000-7005  3 masters + 3 replicas" ;;
      sentinel) endpoint="127.0.0.1:26410  master mymaster at :6410, replica :6411" ;;
      ssh-redis) endpoint="ssh-redis:6379  only through the ssh service" ;;
      ssh) endpoint="ssh://tunnel@127.0.0.1:2222  password tunnelpass or id_ed25519" ;;
    esac
    printf '%-10s  %-9s  %s\n' "$svc" "$state" "$endpoint"
  done
  echo "secrets: $GEN"
  if [ -n "$unhealthy" ]; then
    echo "not healthy:$unhealthy (scripts/test-env.sh up starts or repairs them)"
    return 1
  fi
}

# Single-quotes a value for sh.
shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

env_vars() {
  [ -s "$GEN/ca.crt" ] || note "no secrets in $GEN yet; run scripts/test-env.sh up"
  echo "export REDCRAB_IT=1"
  echo "export REDCRAB_IT_DIR=$(shell_quote "$GEN")"
  echo "export REDCRAB_IT_CA_CERT=$(shell_quote "$GEN/ca.crt")"
  echo "export REDCRAB_IT_CLIENT_CERT=$(shell_quote "$GEN/client.crt")"
  echo "export REDCRAB_IT_CLIENT_KEY=$(shell_quote "$GEN/client.key")"
  echo "export REDCRAB_IT_SSH_KEY=$(shell_quote "$GEN/id_ed25519")"
  echo "export REDCRAB_IT_SSH_KNOWN_HOSTS=$(shell_quote "$GEN/known_hosts")"
}

case ${1:-} in
  up) up ;;
  down) down ;;
  status) status ;;
  env) env_vars ;;
  -h | --help | help) usage ;;
  *)
    usage >&2
    exit 2
    ;;
esac
