#!/bin/sh
# Installs the generated host key and the client's public key with the owners
# and modes sshd insists on (bind-mounted files keep the host's), then runs sshd.
set -eu

GEN=/generated
for f in ssh_host_ed25519_key ssh_host_ed25519_key.pub id_ed25519 id_ed25519.pub; do
  if [ ! -s "$GEN/$f" ]; then
    echo "missing $GEN/$f; run scripts/test-env.sh up" >&2
    exit 1
  fi
done

install -m 600 -o root -g root "$GEN/ssh_host_ed25519_key" /etc/ssh/ssh_host_ed25519_key
install -m 644 -o root -g root "$GEN/ssh_host_ed25519_key.pub" /etc/ssh/ssh_host_ed25519_key.pub
install -d -m 700 -o tunnel -g tunnel /home/tunnel/.ssh
install -m 600 -o tunnel -g tunnel "$GEN/id_ed25519.pub" /home/tunnel/.ssh/authorized_keys
# The health check logs in with the client key.
install -m 600 -o root -g root "$GEN/id_ed25519" /root/healthcheck_key

exec /usr/sbin/sshd -D -e
