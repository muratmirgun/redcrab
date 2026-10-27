#!/bin/sh
# Healthy once `tunnel` can log in with the generated key and a forwarded
# connection (direct-tcpip, what `ssh -L` uses) reaches ssh-redis:6379.
# QUIT makes Redis close the connection, which ends `ssh -W`.
set -eu

reply=$(printf 'PING\r\nQUIT\r\n' | ssh -F /dev/null \
  -o BatchMode=yes -o ConnectTimeout=3 -o LogLevel=ERROR \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -i /root/healthcheck_key -p 22 -W ssh-redis:6379 tunnel@127.0.0.1 2>&1) || :
case "$reply" in
  *+PONG*) ;;
  *)
    echo "ssh: forward to ssh-redis:6379 failed: $reply"
    exit 1
    ;;
esac
