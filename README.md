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

## Security

Saved connections live in `connections.json` in the platform config directory, without
passwords. Passwords go to the system credential store (the macOS Keychain, Windows
Credential Manager, or the Secret Service on Linux) under the service `redcrab`, and are
saved only after they have worked once. Leave the password field empty to use the saved one;
removing a connection removes its password too.

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
`browser_viewer`), sharing `tests/common/mod.rs`: the endpoints, the `it!()` gate, and
a key prefix per test (`it:{test}:{pid}:{n}:`) whose keys are deleted when the test
ends, on every server and database it wrote to. Seeding and cleanup use plain
redis-rs connections, not the code under test. `REDCRAB_IT=1 cargo test` works
without `env` too; the helpers find `tests/docker/.generated/` on their own. Database
5 on `standalone` is shared by the console tests (prefixed keys only); 12, 13 and 14
each belong to one test, so two copies of the same suite running at once can collide
there. A full run takes about 15 s, 10 of them in one test that waits out a TLS
handshake against a plain-TCP server.
