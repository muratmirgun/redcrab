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
cargo run -p redcrab-core --release --example scan_bench  # core timings
cargo run -p redcrab-gui
```

Without a full Xcode install (Command Line Tools only), the default `runtime-shaders`
feature compiles Metal shaders at startup. With Xcode, build releases with
`--no-default-features` to precompile them.
