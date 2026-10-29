# Connections

How redcrab reaches a server: the kinds of servers, TLS, and SSH tunnels. The desktop
app's connection form and `redcrab-tui` share the same saved connections.

## Kinds of servers

The connection form's **Type** picks how the servers are arranged:

- **Standalone**: one server at host and port, or, with **Connect through a Unix socket**,
  at a local socket file (no TLS or SSH over a socket). A server that turns out to be a
  node of a cluster (e.g. `redcrab redis://node:7000`) is connected to as that cluster, so
  its keys are not mistaken for all of them; through an SSH tunnel or a socket, which cannot
  reach the other nodes, that is refused.
- **Cluster**: host and port name the first seed node; **More seed nodes** (`host:port`,
  comma-separated) are tried when it does not answer. The key browser scans every primary,
  type/TTL and deletes go out as one pipeline per primary, and the status bar shows how many
  primaries there are. A cluster has only database 0. A key can only be renamed within its
  hash slot (give both names the same hash tag, e.g. `{user:42}:a` and `{user:42}:b`). The
  console routes each command by its key like `redis-cli -c`; commands without a key
  (`ROLE`, `TIME`, `CONFIG GET`, `EVAL` with no keys, ...) go to one primary, the seed node
  when it is one, and the ones that ask every node (`DBSIZE`, `KEYS`, `INFO`) still do. It
  refuses `SELECT`, transactions and `SCAN` (whose cursor belongs to one node; the key
  browser scans every primary). A primary that is down shows up as an error in the key
  browser, never as keys that are silently missing.
- **Sentinel**: the master's name and the sentinels to ask (`host:port`, in order), plus a
  username and password if the sentinels have their own, and optionally TLS for them. The
  master is found with `SENTINEL GET-MASTER-ADDR-BY-NAME`; after a failover (`READONLY`,
  `MASTERDOWN` or a broken connection) redcrab asks again, switches to the new master and
  resends the request, a few times with growing pauses. A write is only resent when it
  certainly did not run.

Settings that cannot work together (a socket with TLS, a cluster on database 3, SSH with a
cluster, ...) are refused in the form, next to the field, before anything connects.

## TLS

Turn on **Use TLS** in the connection form, or connect with a `rediss://` URL. By default
the server is verified against the system's trusted certificates; a CA file can be given
instead, plus a client certificate and key for servers that require mutual TLS. **Accept
untrusted certificates** (`rediss://host#insecure`) skips verification and is meant only for
self-signed development servers.

```sh
redcrab rediss://default:password@host:6380/0
redcrab-tui rediss://default:password@host:6380/0
```

## SSH tunnels

**Connect through an SSH tunnel** reaches a standalone server that only a bastion host can
see, like `ssh -L`: redcrab listens on a free port on 127.0.0.1 and forwards every
connection to host and port *as the SSH server sees them* (e.g. `10.0.0.5:6379`, or
`localhost:6379` for a Redis on the bastion itself). It logs in with a password, a private
key file (OpenSSH format, optionally protected by a passphrase), or the keys of the running
`ssh-agent` (`SSH_AUTH_SOCK`). The tunnel lives as long as the connection.

**The tunnel's port is open to this whole computer**, as with `ssh -L`: any program on it,
and any other user of it, that finds the port can reach Redis through the tunnel under your
SSH login while redcrab is connected. Marking the connection read-only does not change that
(redcrab enforces read-only itself; the tunnel forwards whatever it is sent). So give the
Redis behind a bastion a password or ACL users rather than relying on the tunnel, and prefer
not to tunnel from shared multi-user machines.

Prefer an Ed25519 key. RSA keys work, but the RSA code SSH uses for them (the `rsa` crate)
is not constant-time ([RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071),
the Marvin attack, with no fixed release yet); redcrab signs with an RSA key once per SSH
login. `deny.toml` and `.cargo/audit.toml` record that accepted risk for `cargo deny` and
`cargo audit`.

The SSH server's host key is always checked, against `~/.ssh/known_hosts` (only read) and
redcrab's own `known_hosts` file in its config directory. **Known hosts only** refuses
servers neither file has, showing the fingerprint of the key the server presented; after
comparing it with the one the server's administrator gives, **Trust this key** records
exactly that key in redcrab's file and connects again, so a different key on that next
connection is still refused. **Trust new hosts** records a server seen for the first time
without asking, like OpenSSH's `StrictHostKeyChecking accept-new`. A key that differs from
the recorded one is always refused, with a message saying how to remove the old entry if
the server really got a new key.

Clusters and Sentinel are not tunneled for now: they hand out the addresses of other nodes,
which point inside the remote network.

**TLS through a tunnel.** The client then connects to 127.0.0.1 while the server's
certificate names the Redis host, and redis-rs checks the name against the address it
connects to. Through a tunnel only, redcrab skips that name check
(`set_danger_accept_invalid_hostnames`) and verifies the certificate chain alone. The SSH
host key authenticates the bastion, not the hop from it to Redis, so the chain is all
that protects that hop: TLS through a tunnel therefore needs the **CA file** that signs
your Redis servers' certificates (a private CA is best; with a public CA any certificate
it signed, for any name, would be accepted), or an explicit **Accept untrusted
certificates**. The system's trusted certificates alone are refused there. Without a
tunnel the name is always checked.
