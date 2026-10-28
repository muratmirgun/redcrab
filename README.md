<p align="center">
  <img src="assets/logo/icon.svg" width="128" height="128" alt="redcrab logo: a crab whose shell is a stack of database disks">
</p>

<h1 align="center">redcrab</h1>

<p align="center">
  A fast, native Redis GUI written in Rust.<br>
  GPU-rendered with <a href="https://www.gpui.rs">GPUI</a> via <a href="https://gpui-kit.com">GPUI Kit</a>; a terminal UI on the same core is planned.
</p>

## Install

Download a package from [Releases](https://github.com/muratmirgun/redcrab/releases):

| Platform | Package |
| --- | --- |
| macOS 11 or later, Apple silicon and Intel | `redcrab-<version>-macos-universal.dmg` |
| Debian 12+, Ubuntu 22.04+ | `redcrab_<version>_amd64.deb` or `_arm64.deb`: `sudo apt install ./redcrab_*.deb` |
| Other Linux with glibc 2.35+ | `redcrab-<version>-linux-<arch>.tar.gz`: run `./redcrab`; copy its `share/` into `~/.local/share` for a menu entry |
| Windows 10 and 11, x86_64 | `redcrab-<version>-windows-x86_64.zip` |

The Linux and Windows packages hold the [terminal app](#terminal-app), `redcrab-tui`,
too. On macOS it comes on its own, as `redcrab-tui-<version>-macos-universal.tar.gz`:
put `redcrab-tui` in a folder on your `PATH` (if macOS refuses to run a downloaded copy,
`xattr -d com.apple.quarantine redcrab-tui`).

`SHA256SUMS` lists every file's checksum (`sha256sum -c SHA256SUMS`).

- **macOS:** the app is not notarized yet, so the first start needs a right-click on
  redcrab.app, then **Open** (or `xattr -dr com.apple.quarantine /Applications/redcrab.app`).
- **Windows:** the executable is not code-signed yet; SmartScreen may say "Windows
  protected your PC": **More info**, then **Run anyway**.
- **Linux:** drawing needs a Vulkan driver (`mesa-vulkan-drivers` on most systems).

### Updates

From 0.2.0 on, redcrab looks for a new release when it starts and once a day after,
and asks before it installs one: **Install and Restart**, **Later**, or **Skip This
Version**. The title bar's ⋮ menu (or the command palette) checks right away, and
**Settings** turns the daily check off; so does `REDCRAB_NO_UPDATE_CHECK=1`. Only the GitHub API is asked, and
nothing about you or your connections is sent.

An update installs only if `SHA256SUMS` carries the release key's Ed25519 signature
and the download matches it, so a file swapped on GitHub or on the way is refused.
The new version replaces the app where it is: `redcrab.app` (which needs a folder you
can write to, such as Applications, not the disk image), the `.deb` (through
`pkexec`, which asks for an administrator's password) or the unpacked tarball or
zip. When it cannot, the dialog links to the download instead. Builds from source
never look for updates.

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
create string keys, change TTLs and rename keys; editing collections and creating keys of
other types needs `EVAL`, and after the first refusal the value panel stops offering it.

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

The console keeps the last 1,000 commands of each connection in `history/` in the same
directory, readable by its owner only. Commands that carry credentials are never written:
`AUTH`, `HELLO ... AUTH`, `ACL SETUSER`, `CONFIG SET` of `requirepass`, `masterauth` and
the like, `MIGRATE ... AUTH`, `MODULE LOAD` with arguments, scripts (`EVAL`,
`SCRIPT LOAD`, `FUNCTION LOAD`) that mention any of these, and any line with an argument
that looks like a password, a secret or a token (`access_token`, `bearer`, ...; which
also keeps out harmless lines such as `HGET user:1 password_hint` or `SET tokens:count 1`).

## Console

<kbd>⌘J</kbd> opens a console with a connection of its own, so `SELECT` and `MULTI` stay
in it. <kbd>↑</kbd> <kbd>↓</kbd> walk the history, <kbd>Tab</kbd> completes command
names, subcommands and the options a command takes at the cursor (again to cycle), the
rest of the command's syntax shows greyed out as you type, <kbd>Esc</kbd> closes the
suggestions or stops a running command, and <kbd>⌘K</kbd> clears the log.

Shortcuts use <kbd>⌘</kbd> on macOS and <kbd>Ctrl</kbd> on Linux and Windows
(<kbd>Ctrl</kbd>+<kbd>J</kbd>, <kbd>Ctrl</kbd>+<kbd>S</kbd>, ...).

## Server

The **Server** tab (<kbd>⌘2</kbd>; <kbd>⌘1</kbd> goes back to the keys) shows what the server
says about itself, read every two seconds while it is open and never while it is not:

- **Overview:** operations per second, memory (with its peak, RSS, fragmentation and
  `maxmemory`), clients and the hit rate, each with its last two minutes charted; keys
  per database, the server and its uptime, persistence and replication.
- **Slow Log:** the slowest commands as typed, how long they took, when and from which
  client, and how the log is set up; **Reset…** empties it.
- **Clients:** every connection, filtered by address, name, user or command, with
  redcrab's own marked; **Kill…** disconnects one.
- **INFO:** every field, filtered.

On a cluster it looks at one primary at a time, picked at the top right. Reset and Kill
are refused on a read-only connection, and they ask first. A user whose ACL lacks
`INFO`, `SLOWLOG` or `CLIENT` sees why instead of the figures.

## Memory

The **Memory** tab (<kbd>⌘3</kbd>) finds what takes the memory: it walks the keys with
`SCAN` and measures each with `MEMORY USAGE` (`TYPE` and `PTTL` in the same round trip),
when you click **Analyze**, never on its own. It shows

- the total, the keys, and how much of the memory is in keys without a TTL;
- the memory per type, and per key prefix, split on the connection's separator, three
  levels deep: click a prefix to look into it;
- the 100 biggest keys: click one to open it in the keys view.

On a large or busy server, measure **1 key in 10** or **1 in 100**: the totals are then
estimates (marked ≈), the measured figures times the sample. A cluster is walked
primary by primary. It only reads, so it works on a read-only connection; the user needs
the `MEMORY` command.

## Pub/Sub and MONITOR

The **Pub/Sub** tab (<kbd>⌘4</kbd>) subscribes to channels, and to patterns when the name
has `*`, `?` or `[` (PSUBSCRIBE), on a connection of its own, and lists the messages as
they come, with their local time, newest first and filtered by channel or payload. It
lists the channels that have subscribers (`PUBSUB CHANNELS`, one click subscribes), and
publishes a message, which a read-only connection does not: subscribers act on what
they receive.

**Monitor** shows every command the server runs (`MONITOR`), on a cluster primary of
your choice. It slows a busy server down, by as much as half, so it runs only when
started and stops when you leave the tab. Both keep the newest 10,000 entries.

## Command palette and settings

<kbd>⌘K</kbd> opens the command palette: everything the window can do, with its
shortcut, found by typing a few letters. Refresh or search the keys, add or delete one,
open the console, switch to another database, disconnect, connect to a saved connection
(in a new window when one is open), change the theme, look for updates. In the console
<kbd>⌘K</kbd> clears the log, as in a terminal; <kbd>⇧⌘P</kbd> opens the palette from
anywhere.

<kbd>⌘,</kbd> opens the settings, which are saved as `settings.json` next to the saved
connections:

| Setting | Default | |
| --- | --- | --- |
| Theme | System | System follows the light or dark mode of the OS as it changes. The title bar's sun/moon picks one and keeps it. |
| Text size | Medium | Small, Medium or Large; the whole interface scales with it. |
| Load at most | 2,000,000 keys | Where a scan of the key list stops (about 60 bytes per key). A filter finds keys past it. |
| Keys per SCAN | 2,000 | The `COUNT` of each `SCAN`: smaller keeps each round trip short on a busy server, larger loads faster. |
| Check for updates automatically | On | Packaged builds only. |

## Terminal app

`redcrab-tui` is redcrab in a terminal, for a server reached over SSH or a machine
without a GPU: the same saved connections and passwords, the key tree, values of every
type, editing and the console, on the same core.

```sh
redcrab-tui                          # pick a saved connection
redcrab-tui orders                   # connect to the saved connection "orders"
redcrab-tui redis://:pw@10.0.0.5:6379/2
```

The keys are vim's or the arrows: <kbd>j</kbd> <kbd>k</kbd> to move, <kbd>l</kbd>
<kbd>h</kbd> to open and close folders, <kbd>/</kbd> to filter, <kbd>Tab</kbd> to the
next pane, <kbd>:</kbd> for the console, <kbd>?</kbd> for all of them. In the value,
<kbd>f</kbd> cycles the format (Text, JSON, Hex, Base64, MessagePack, gzip),
<kbd>e</kbd> edits it in `$VISUAL` or `$EDITOR`, <kbd>t</kbd> sets the TTL,
<kbd>R</kbd> renames the key, <kbd>a</kbd> adds a row and <kbd>x</kbd> deletes the rows
picked with <kbd>Space</kbd>. <kbd>n</kbd> makes a key, <kbd>D</kbd> deletes one, and
<kbd>b</kbd> switches the database. A read-only connection stays read-only here too.
Connections are added and edited in the desktop app; the server tools, the memory
analysis and Pub/Sub are there too, for now.

## Layout

| Crate | What |
| --- | --- |
| `redcrab-core` | Connections, scanning, the key tree, value paging, formatting. No UI. |
| `redcrab-gui`  | The desktop app (`redcrab` binary). |
| `redcrab-tui`  | The terminal app (`redcrab-tui` binary), on ratatui. |

Logo sources live in [`assets/logo`](assets/logo): `mark.svg` (two-tone), `mark-mono.svg`
(one color), and the app icon (`icon.svg`, with `icon-small.svg` for 16 and 32 px).
`assets/logo/build-icons.sh` regenerates `icon.icns` (macOS), `icon.ico` (Windows) and
`icon.png` from them; Linux packages render their PNGs at packaging time.

## Packaging

`scripts/package/{macos,linux,windows}.sh` build the packages for the machine they run
on into `dist/`, each with a `.sha256` file, and check that the result starts
(`redcrab --version`):

```sh
scripts/package/macos.sh                       # .app in a .dmg, this Mac's architecture
TARGETS="aarch64-apple-darwin x86_64-apple-darwin" scripts/package/macos.sh   # universal
scripts/package/linux.sh                       # .tar.gz and .deb (needs dpkg-dev)
scripts/package/windows.sh                     # .zip, in Git Bash
```

[`.github/workflows/release.yml`](.github/workflows/release.yml) runs them on every platform
for a version tag and drafts the GitHub release; see [RELEASING.md](RELEASING.md).

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

The Metal shaders are compiled when the app starts, so on macOS the Command Line Tools
are enough to build it (no `xcrun metal`). GPUI Kit 0.8.1 turns that on in GPUI by
itself, so for now building with `--no-default-features` (dropping the default
`runtime-shaders` feature) changes nothing; once it no longer does, a full Xcode
install can precompile them that way.

On Linux the GUI links against xcb, xkbcommon, fontconfig and freetype (Wayland and
Vulkan are loaded at runtime). On Debian and Ubuntu:

```sh
sudo apt install pkg-config libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev libfontconfig-dev libfreetype-dev
```

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
(`connect`, `scan`, `keys`, `values`, `read_only`, `console_session`, `console_prompt`,
`browser_viewer`, `keyspace`, `edit`, `edit_viewer`, `cluster`, `sentinel`, `ssh`,
`unix_socket`, `credentials`), sharing `tests/common/mod.rs`: the endpoints, the `it!()`
gate, and a key prefix per test (`it:{test}:{pid}:{n}:`) whose keys are deleted when the
test ends, on every server and database it wrote to. Seeding and cleanup use plain
redis-rs connections, not the code under test. `REDCRAB_IT=1 cargo test` works
without `env` too; the helpers find `tests/docker/.generated/` on their own. Database
5 on `standalone` is shared by the console tests (prefixed keys only); 10, 11 and 15
belong to the `keyspace` tests and 12, 13 and 14 each to one test, so two copies of the
same suite running at once can collide there. The `sentinel` tests that use the service
run one at a time, also across test processes (a lock file in the temporary directory),
since one of them fails the master over (and back); the others play a sentinel of their
own. `unix_socket` reaches `standalone`, `auth` and a cluster node through socket proxies
of its own; the SSH tests keep their known_hosts files in temporary folders;
`credentials` uses `REDCRAB_CREDENTIAL_SERVICE` (never `redcrab`; `redcrab-test` when
unset) and deletes every entry it creates. A full run takes about 45 s, most of it in the
Sentinel failovers and in one test that waits out a TLS handshake against a plain-TCP
server.

## Continuous integration

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on pushes to `main`, on
pull requests and on demand. Linux checks formatting and clippy, starts the test
environment above and runs the whole suite with `REDCRAB_IT=1` (in a D-Bus session with
an unlocked gnome-keyring of its own, which the credential store test needs), then makes
a release build of the GUI. macOS runs the tests (GitHub's macOS runners have no Docker, so the
integration tests skip) and builds the GUI; Windows runs the core tests and builds the
GUI. A separate job runs [`cargo deny`](deny.toml): known vulnerabilities, licenses that
would not allow shipping redcrab as MIT OR Apache-2.0, and crates from outside crates.io.
Every step is a plain shell command, so a failure can be reproduced locally. The
Rust version is pinned in the workflow (`RUST_TOOLCHAIN`); bump it on purpose.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in redcrab, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
