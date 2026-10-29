# Security

What redcrab keeps on disk, where, and what it never writes down. For the risks of SSH
tunnels and of TLS through them, see [Connections](connections.md#ssh-tunnels).

## Passwords and saved connections

Saved connections live in `connections.json` in the platform config directory, without
passwords. Passwords go to the system credential store (the macOS Keychain, Windows
Credential Manager, or the Secret Service on Linux) under the service `redcrab`, and are
saved only after they have worked once. Leave the password field empty to use the saved one;
removing a connection removes its passwords too. Each secret has an entry of its own: the
Redis password under the connection's name (as older versions saved it), the sentinels'
password under `<name>#sentinel`, the SSH password under `<name>#ssh` and the SSH key's
passphrase under `<name>#ssh-key`. New connection names cannot contain `#`; connections
saved with one by older versions keep their name and password.

## Console history

The console keeps the last 1,000 commands of each connection in `history/` in the same
directory, readable by its owner only. Commands that carry credentials are never written:
`AUTH`, `HELLO ... AUTH`, `ACL SETUSER`, `CONFIG SET` of `requirepass`, `masterauth` and
the like, `MIGRATE ... AUTH`, `MODULE LOAD` with arguments, scripts (`EVAL`,
`SCRIPT LOAD`, `FUNCTION LOAD`) that mention any of these, and any line with an argument
that looks like a password, a secret or a token (`access_token`, `bearer`, ...; which
also keeps out harmless lines such as `HGET user:1 password_hint` or `SET tokens:count 1`).
