<p align="center">
  <img src="assets/logo/icon.svg" width="128" height="128" alt="redcrab logo: a crab whose shell is a stack of database disks">
</p>

<h1 align="center">redcrab</h1>

<p align="center">
  A fast, native Redis GUI written in Rust.<br>
  GPU-rendered with <a href="https://www.gpui.rs">GPUI</a> via <a href="https://gpui-kit.com">GPUI Kit</a>; a terminal UI on the same core is planned.
</p>

## Why

Existing free clients are Electron or webview apps. redcrab aims to stay instant on
keyspaces with millions of keys while using a fraction of the memory.

- Keys are walked with `SCAN` (never `KEYS *`) and streamed into a folder tree on
  background threads; the UI thread only reads.
- The key list is virtualized: only visible rows are rendered, and type/TTL is fetched
  for those rows alone, in one pipelined round trip.
- Collections are paged (`HSCAN`, `SSCAN`, `LRANGE`, `ZRANGE`, `XRANGE`), so opening
  a hash with a million fields does not block anything.
- Deleting a folder, or every key a filter matches, counts the keys first, asks with the
  exact pattern and count, then `UNLINK`s what `SCAN` returns in pipelined batches, with
  progress and a Stop button; a stopped delete waits for its last round trip and reports
  exactly how many keys went. Folder names are matched literally, glob characters and
  all.

## Editing

Values of every type can be edited: strings and JSON documents as text, collection rows
in a detail pane, rows added and deleted, keys created, renamed and given a TTL. Values
show as text, JSON, a hex dump, or decoded from Base64, MessagePack or gzip, and are
saved back in the same format. Bytes that are not text are edited the way `redis-cli`
writes them (`"\xff\x00"`).

Every change is one step on one key (mostly a Lua script), so it is atomic and checks what
it relies on: a key deleted or replaced since it was loaded is never recreated or
overwritten, a list element is only changed while it still holds what was loaded, a new key
never replaces an existing one. A read-only connection refuses every change in core, not
just in the UI. Users whose ACL denies scripts (`-@scripting`) can still save strings,
change TTLs and rename keys; editing collections and creating keys needs `EVAL`.

## TLS

Turn on **Use TLS** in the connection form, or connect with a `rediss://` URL. By default
the server is verified against the system's trusted certificates; a CA file can be given
instead, plus a client certificate and key for servers that require mutual TLS. **Accept
untrusted certificates** (`rediss://host#insecure`) skips verification and is meant only for
self-signed development servers.

```sh
cargo run -p redcrab-core --example ping -- rediss://host:6380 --ca ca.crt
cargo run -p redcrab-gui -- rediss://default:password@host:6380/0
```

## Connections

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

### SSH tunnels

**Connect through an SSH tunnel** reaches a standalone server that only a bastion host can
see, like `ssh -L`: redcrab listens on a free port on 127.0.0.1 and forwards every
connection to host and port *as the SSH server sees them* (e.g. `10.0.0.5:6379`, or
`localhost:6379` for a Redis on the bastion itself). It logs in with a password, a private
key file (OpenSSH format, optionally protected by a passphrase), or the keys of the running
`ssh-agent` (`SSH_AUTH_SOCK`). The tunnel lives as long as the connection.

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

## Security

Saved connections live in `connections.json` in the platform config directory, without
passwords. Passwords go to the system credential store (the macOS Keychain, Windows
Credential Manager, or the Secret Service on Linux) under the service `redcrab`, and are
saved only after they have worked once. Leave the password field empty to use the saved one;
removing a connection removes its passwords too. Each secret has an entry of its own: the
Redis password under the connection's name (as older versions saved it), the sentinels'
password under `<name>#sentinel`, the SSH password under `<name>#ssh` and the SSH key's
passphrase under `<name>#ssh-key`. New connection names cannot contain `#`; connections
saved with one by older versions keep their name and password.

## Layout

| Crate | What |
| --- | --- |
| `redcrab-core` | Connections, scanning, the key tree, value paging, formatting. No UI. |
| `redcrab-gui`  | The desktop app (`redcrab` binary). |

Logo sources live in [`assets/logo`](assets/logo): `mark.svg` (two-tone), `mark-mono.svg`
(one color), and the macOS app icon (`icon.svg`, `icon-small.svg` for 16/32 px).
`assets/logo/build-icns.sh` regenerates `icon.icns` and `icon.png` from them.

## Development

```sh
docker run -d --name redcrab-redis -p 127.0.0.1:6379:6379 redis:8-alpine
cargo run -p redcrab-core --release --example seed        # ~1M keys of every type
cargo run -p redcrab-core --release --example scan_bench  # core timings and memory per key
cargo run -p redcrab-core --release --example scan_bench -- --synthetic 1000000  # no server needed
cargo run -p redcrab-core --release --example delete_bench -- redis://127.0.0.1:6400/15  # writes 500k keys, then deletes them
cargo run -p redcrab-gui
```

`REDCRAB_CONFIG_DIR=<dir>` and `REDCRAB_CREDENTIAL_SERVICE=<name>` make a run keep its saved
connections and passwords apart from your own, e.g. for a quick check of a development build.

Without a full Xcode install (Command Line Tools only), the default `runtime-shaders`
feature compiles Metal shaders at startup. With Xcode, build releases with
`--no-default-features` to precompile them.

## Testing

`cargo test --workspace` runs the unit tests anywhere; the integration tests print
"skipped" unless `REDCRAB_IT=1`. They run against throwaway servers in Docker:

```sh
scripts/test-env.sh up                                  # start or repair; returns when all are usable
eval "$(scripts/test-env.sh env)" && cargo test --workspace
scripts/test-env.sh status                              # health per service; non-zero unless all healthy
scripts/test-env.sh down                                # remove the containers
```

`up` needs Docker with the compose plugin, `openssl` and `ssh-keygen`, and works the
same on macOS and on GitHub's Ubuntu runners. It generates a throwaway CA, server and
client certificates and SSH keys into `tests/docker/.generated/` (gitignored; delete it
for new ones), then starts [`tests/docker/compose.yml`](tests/docker/compose.yml)
(compose project `redcrab-it`, containers `redcrab-it-*`). Every port listens on
127.0.0.1 only:

| Service | Endpoint | Setup |
| --- | --- | --- |
| standalone | `127.0.0.1:6400` | no auth |
| auth | `127.0.0.1:6401` | `requirepass rootpass`; ACL user `app` / `apppass` limited to `app:*` keys, no `@dangerous` commands |
| tls | `127.0.0.1:6402` | TLS only, server certificate for `localhost` and `127.0.0.1` signed by `ca.crt`; client certificate optional |
| tls-mtls | `127.0.0.1:6403` | TLS only, `client.crt` / `client.key` required |
| cluster | `127.0.0.1:7000`-`7005` | 3 masters + 3 replicas; MOVED and `CLUSTER SLOTS` point at `127.0.0.1:700x` |
| sentinel | `127.0.0.1:26410` | master `mymaster` at `127.0.0.1:6410`, replica at `6411`, quorum 1 |
| ssh | `127.0.0.1:2222` | user `tunnel`, password `tunnelpass` or key `id_ed25519`; forwards to `ssh-redis:6379`, which has no published port |

`env` prints `REDCRAB_IT=1`, `REDCRAB_IT_DIR` (the generated folder),
`REDCRAB_IT_CA_CERT`, `REDCRAB_IT_CLIENT_CERT`, `REDCRAB_IT_CLIENT_KEY`,
`REDCRAB_IT_SSH_KEY` and `REDCRAB_IT_SSH_KNOWN_HOSTS` (pins the SSH host key for
`127.0.0.1:2222` and `localhost:2222`). Tests only touch keys under their own prefix,
so suites can share the servers. Cluster and Sentinel start from scratch on every
container start; if a test leaves one broken (say, after a failover), `up` recreates
it.

The integration tests are in `crates/redcrab-core/tests/`, one file per area
(`connect`, `scan`, `keys`, `values`, `read_only`, `console_session`,
`browser_viewer`, `cluster`, `sentinel`, `ssh`, `unix_socket`, `credentials`), sharing
`tests/common/mod.rs`: the endpoints, the `it!()` gate, and
a key prefix per test (`it:{test}:{pid}:{n}:`) whose keys are deleted when the test
ends, on every server and database it wrote to. Seeding and cleanup use plain
redis-rs connections, not the code under test. `REDCRAB_IT=1 cargo test` works
without `env` too; the helpers find `tests/docker/.generated/` on their own. Database
5 on `standalone` is shared by the console tests (prefixed keys only); 12, 13 and 14
each belong to one test, so two copies of the same suite running at once can collide
there. The `sentinel` tests that use the service run one at a time, also across test
processes (a lock file in the temporary directory), since one of them fails the master
over (and back); the others play a sentinel of their own. `unix_socket` reaches
`standalone`, `auth` and a cluster node through socket proxies of its own; the SSH
tests keep their known_hosts files in temporary folders; `credentials` uses
`REDCRAB_CREDENTIAL_SERVICE` (never `redcrab`; `redcrab-test` when unset) and deletes
every entry it creates. A full run takes about 45 s, most of it in the Sentinel
failovers and in one test that waits out a TLS handshake against a plain-TCP server.
