<p align="center">
  <img src="assets/logo/icon.svg" width="112" height="112" alt="redcrab logo: a crab whose shell is a stack of database disks">
</p>

<h1 align="center">redcrab</h1>

<p align="center">
  <b>A fast, native Redis GUI, and a terminal UI, written in Rust.</b><br>
  GPU-rendered with <a href="https://www.gpui.rs">GPUI</a>: no Electron, no webview. Free and open source.
</p>

<p align="center">
  <a href="https://github.com/muratmirgun/redcrab/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/muratmirgun/redcrab?color=e5432d"></a>
  <a href="https://github.com/muratmirgun/redcrab/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/muratmirgun/redcrab/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="macOS, Linux and Windows" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-555">
  <a href="#license"><img alt="License: MIT or Apache-2.0" src="https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-blue"></a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/keys-dark.png">
    <img src="docs/images/keys-light.png" width="900" alt="redcrab showing a JSON document, with the key tree on the left">
  </picture>
</p>

## Highlights

- **Instant on millions of keys.** Keys stream in with `SCAN`, never `KEYS *`, into a
  folder tree that keeps about 60 bytes per key. Only the rows on screen are drawn, and
  only they are asked for their type and TTL.
- **Every type, viewed and edited.** Strings, hashes, lists, sets, sorted sets, streams
  and JSON, paged however big they are. Values show as text, JSON, hex, or decoded from
  Base64, MessagePack and gzip, and save back the way they came.
- **A real console.** A connection of its own, history, completion of commands and their
  options, and the rest of the syntax greyed out as you type.
- **Server tools.** Ops/sec, memory, clients and hit rate as they change, the slow log,
  the connected clients (kill one) and INFO.
- **Memory analysis.** What takes the memory: by type, by key prefix, and the 100
  biggest keys, measuring every key or a sample.
- **Pub/Sub and MONITOR.** Subscribe to channels and patterns, publish, watch every
  command the server runs.
- **Any way to reach a server.** Standalone, Cluster, Sentinel, TLS and mutual TLS, SSH
  tunnels, Unix sockets. Redis 6 and later, and Valkey.
- **Careful with production.** Read-only connections enforced in the core, colored
  environment tags, passwords in the system keychain, and edits that never overwrite a
  value that changed meanwhile.
- **A terminal app too.** `redcrab-tui` does all of the above over SSH, with vim keys.
- **Signed updates.** It asks before installing a new release, and checks it against
  redcrab's own key first.

## A closer look

<table>
  <tr>
    <td width="50%">
      <picture>
        <source media="(prefers-color-scheme: dark)" srcset="docs/images/server-dark.png">
        <img src="docs/images/server-light.png" alt="The server overview with live charts">
      </picture>
      <p align="center"><sub><b>Server</b>: live figures, the slow log, clients and INFO</sub></p>
    </td>
    <td width="50%">
      <picture>
        <source media="(prefers-color-scheme: dark)" srcset="docs/images/memory-dark.png">
        <img src="docs/images/memory-light.png" alt="The memory analysis by type and by key prefix">
      </picture>
      <p align="center"><sub><b>Memory</b>: what takes it, by type, prefix and key</sub></p>
    </td>
  </tr>
  <tr>
    <td width="50%">
      <picture>
        <source media="(prefers-color-scheme: dark)" srcset="docs/images/pubsub-dark.png">
        <img src="docs/images/pubsub-light.png" alt="Pub/Sub messages arriving on a pattern and a channel">
      </picture>
      <p align="center"><sub><b>Pub/Sub</b>: channels, patterns, publishing, MONITOR</sub></p>
    </td>
    <td width="50%">
      <picture>
        <source media="(prefers-color-scheme: dark)" srcset="docs/images/console-dark.png">
        <img src="docs/images/console-light.png" alt="The console under a hash">
      </picture>
      <p align="center"><sub><b>Console</b>: history, completion and hints</sub></p>
    </td>
  </tr>
</table>

## Install

Download the package for your system from the
[latest release](https://github.com/muratmirgun/redcrab/releases/latest):

| System | Package | |
| --- | --- | --- |
| macOS 11 or later, Apple silicon and Intel | `redcrab-<version>-macos-universal.dmg` | Drag redcrab to Applications |
| Debian 12+, Ubuntu 22.04+ | `redcrab_<version>_amd64.deb` or `_arm64.deb` | `sudo apt install ./redcrab_*.deb` |
| Other Linux with glibc 2.35+ | `redcrab-<version>-linux-<arch>.tar.gz` | Unpack, run `./redcrab` |
| Windows 10 and 11, x86_64 | `redcrab-<version>-windows-x86_64.zip` | Unzip, run `redcrab.exe` |

The Linux and Windows packages hold the [terminal app](#terminal-app) too. For a server,
or anywhere without the desktop, it comes on its own, without the GUI's libraries:

| System | Terminal app alone |
| --- | --- |
| macOS | `redcrab-tui-<version>-macos-universal.tar.gz`: put `redcrab-tui` on your `PATH` |
| Debian, Ubuntu | `redcrab-tui_<version>_amd64.deb` or `_arm64.deb`: `sudo apt install ./redcrab-tui_*.deb` |
| Other Linux | `redcrab-tui-<version>-linux-<arch>.tar.gz` |
| Windows | `redcrab-tui-<version>-windows-x86_64.zip` |

The desktop `.deb` takes the place of `redcrab-tui`'s when both are installed.

From then on redcrab keeps itself up to date: it looks for a new release once a day and
asks before installing it ([more](docs/guide.md#updates)).

<details>
<summary>The first start, and checking a download</summary>

- **macOS:** the app is not notarized yet, so the first start needs a right-click on
  redcrab.app, then **Open** (or `xattr -dr com.apple.quarantine /Applications/redcrab.app`).
  For `redcrab-tui`: `xattr -d com.apple.quarantine redcrab-tui`.
- **Windows:** the executable is not code-signed yet; SmartScreen may say "Windows
  protected your PC": **More info**, then **Run anyway**.
- **Linux:** drawing needs a Vulkan driver (`mesa-vulkan-drivers` on most systems). For
  a menu entry from the tarball, copy its `share/` into `~/.local/share`.
- Every release has a `SHA256SUMS` file: `sha256sum -c SHA256SUMS`.

</details>

**From source**, with a current stable Rust (and on Linux a few libraries, see
[CONTRIBUTING.md](CONTRIBUTING.md#building-and-running)):

```sh
git clone https://github.com/muratmirgun/redcrab && cd redcrab
cargo run --release -p redcrab-gui    # the desktop app
cargo run --release -p redcrab-tui    # the terminal app
```

## Quick start

1. **Connect.** Start redcrab, fill in the form (a name, the host and port, a password
   if the server has one) and press <kbd>Enter</kbd>. The connection is saved and its
   password goes to the system keychain. From a terminal, a URL works too:
   `redcrab redis://user:password@host:6379/0` (`rediss://` for TLS).
2. **Browse.** Keys are grouped into folders on `:`. <kbd>⌘F</kbd> filters them, by a
   pattern (`user:*`) or any text. Pick a key to see its value, edit it in place, and
   <kbd>⌘S</kbd> saves.
3. **Look around.** <kbd>⌘J</kbd> opens the console; <kbd>⌘1</kbd> to <kbd>⌘4</kbd>
   switch between the keys, the server, the memory analysis and Pub/Sub.
4. **Find anything.** <kbd>⌘K</kbd> opens the command palette: everything the window
   can do, by name, with its shortcut.

| Keys | Does | Keys | Does |
| --- | --- | --- | --- |
| <kbd>⌘K</kbd> | Command palette | <kbd>⌘J</kbd> | Console |
| <kbd>⌘F</kbd> | Filter the keys | <kbd>⌘R</kbd> | Refresh |
| <kbd>⌘S</kbd> | Save the value | <kbd>⇧⌘N</kbd> | New key |
| <kbd>⌘⌫</kbd> | Delete the key | <kbd>⌘1</kbd>–<kbd>⌘4</kbd> | Keys, Server, Memory, Pub/Sub |
| <kbd>⌘,</kbd> | Settings | <kbd>⌘N</kbd> | New window |

On Linux and Windows, <kbd>Ctrl</kbd> takes the place of <kbd>⌘</kbd>.

## Terminal app

<p align="center">
  <img src="docs/images/tui-keys.svg" width="900" alt="redcrab-tui showing a hash, the key tree and the console in a terminal">
</p>

`redcrab-tui` is redcrab in a terminal, for a server you reach over SSH or a machine
without a GPU: the same saved connections and passwords, the same views, on the same core.

```sh
redcrab-tui                          # pick a saved connection
redcrab-tui orders                   # connect to the saved connection "orders"
redcrab-tui redis://:pw@10.0.0.5:6379/2
```

| Keys | Does |
| --- | --- |
| <kbd>j</kbd> <kbd>k</kbd> or arrows | Move; <kbd>l</kbd> <kbd>h</kbd> open and close a folder |
| <kbd>/</kbd> | Filter the keys |
| <kbd>Tab</kbd> | Next pane |
| <kbd>:</kbd> | Console |
| <kbd>1</kbd>–<kbd>4</kbd> | Keys, Server, Memory, Pub/Sub |
| <kbd>e</kbd> <kbd>f</kbd> <kbd>t</kbd> <kbd>R</kbd> | Edit in `$EDITOR`, change the format, the TTL, the name |
| <kbd>n</kbd> <kbd>D</kbd> <kbd>b</kbd> | New key, delete the key, switch the database |
| <kbd>?</kbd> | Every key of the view |

## Documentation

- **[Using redcrab](docs/guide.md):** editing, the console, the server tools, the
  memory analysis, Pub/Sub, the palette and settings, updates, and the terminal app.
- **[Connections](docs/connections.md):** Cluster, Sentinel, Unix sockets, TLS and SSH
  tunnels.
- **[Security](docs/security.md):** where passwords and history are kept, and what is
  never written down.
- **[Contributing](CONTRIBUTING.md):** building, testing, screenshots, packaging, and
  [releasing](RELEASING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in redcrab, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.
