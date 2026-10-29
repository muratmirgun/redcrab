# Contributing to redcrab

Issues and pull requests are welcome. This is how the project is laid out, built, tested
and released.

- [Layout](#layout)
- [Building and running](#building-and-running)
- [Testing](#testing)
- [Continuous integration](#continuous-integration)
- [Screenshots](#screenshots)
- [Packaging and releases](#packaging-and-releases)
- [License](#license)

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

UI-agnostic logic lives in `redcrab-core`'s view-models (`browser`, `viewer`, `console`,
`server`, `analysis`, `feed`, `settings`), so the two front ends stay thin and behave
alike.

## Building and running

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

## Screenshots

The pictures in the README and in [docs/](docs) are made by
[`scripts/screenshots.sh`](scripts/screenshots.sh), on macOS with Docker: it starts a
throwaway Redis on port 6420, fills it with a small shop's data
([`seed.py`](scripts/screenshots/seed.py)) and some traffic, then

- runs the desktop app built with the `screenshots` feature (`redcrab --screenshots
  <folder> <url>`, [`screenshots.rs`](crates/redcrab-gui/src/screenshots.rs)): the window
  is drawn off the screen by GPUI's visual test platform and read back from Metal, so
  nothing shows and the focus stays where it is; every scene is shot in the dark and the
  light theme;
- renders the terminal app into SVG
  ([`screenshots.rs`](crates/redcrab-tui/src/screenshots.rs), an ignored test).

```sh
scripts/screenshots.sh              # into docs/images
scripts/screenshots.sh /tmp/shots   # somewhere else, to compare
```

It uses a configuration folder and a credential service of its own, so saved
connections and passwords are left alone. Run it again after a change that shows.

## Packaging and releases

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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in redcrab, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
