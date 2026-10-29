# Releasing redcrab

1. Set the new version in the root `Cargo.toml` (`[workspace.package] version`), run
   `cargo build` so `Cargo.lock` follows, and commit.
2. Tag the commit and push the tag; the tag must be `v` plus that version:

   ```sh
   git tag -s v0.1.0 -m "redcrab 0.1.0"
   git push origin v0.1.0
   ```

3. [`.github/workflows/release.yml`](.github/workflows/release.yml) builds the macOS
   universal `.dmg`, the Linux `.tar.gz` and `.deb` for x86_64 and aarch64, and the
   Windows `.zip`, and the terminal app on its own for each (`redcrab-tui-*`), checks
   that each one starts, and creates a **draft** release with the files, a `SHA256SUMS`
   file and notes generated from the commits.
4. Read the draft on GitHub, edit the notes if needed, and publish it. Installed
   copies (0.2.0 and later) offer it within a day; they never see a draft.
5. Publishing runs [`.github/workflows/homebrew.yml`](.github/workflows/homebrew.yml):
   it points the [Homebrew tap](#homebrew-tap) at the release, after installing the
   cask and formula from a copy of it on macOS.

Packaging changes are tried out before a release: pushes to `main` that touch
`scripts/package/`, the release workflow, `assets/logo/` or the GUI's `build.rs` run the
same builds without publishing anything. The scripts also run locally (see the README's
"Packaging" section).

## Update signing key

Installed copies update themselves only to a release whose `SHA256SUMS` is signed with
the release key (`crates/redcrab-core/src/update/`), so a tag does not publish without
it. The private key is the repository secret `UPDATE_SIGNING_KEY`: the whole PEM
file, `-----BEGIN PRIVATE KEY-----` to `-----END PRIVATE KEY-----`. Its public half,
built into the app, is
[`release-key.pub.pem`](crates/redcrab-core/src/update/release-key.pub.pem). The
publish job signs with the secret and checks the signature against that file.

Keep a copy of the private key somewhere safe, such as a password manager: GitHub
does not show a secret again. If it is lost or leaks, make a new pair:

```sh
openssl genpkey -algorithm ed25519 -out update-signing-key.pem
openssl pkey -in update-signing-key.pem -pubout \
  -out crates/redcrab-core/src/update/release-key.pub.pem
```

Copies already installed trust only the old key, so they cannot update to a release
signed with the new one and need one manual download. When the old key is still safe
(a planned change), that is avoidable: release once more with the old key as the
secret and the new public key in the source, then switch the secret.

## Homebrew tap

[muratmirgun/homebrew-tap](https://github.com/muratmirgun/homebrew-tap) holds
`Casks/redcrab.rb` (the desktop app's disk image) and `Formula/redcrab-tui.rb` (the
terminal app's tarballs for macOS and Linux), next to OwnCode's formula. The Homebrew
workflow writes both with [`scripts/homebrew.sh`](scripts/homebrew.sh) from the
release's signed `SHA256SUMS`, so edit them there; it never takes the tap back to an
older version. Pushes to `main` that touch the script or the workflow check them against
the latest release without pushing. To point the tap at a release again, run the
workflow by hand (Actions > Homebrew > Run workflow) with its tag.

It pushes with a deploy key of the tap, the repository secret `HOMEBREW_TAP_SSH_KEY`.
To replace it:

```sh
ssh-keygen -t ed25519 -N "" -C "redcrab release" -f homebrew-tap-key
```

Add `homebrew-tap-key.pub` to the tap under Settings > Deploy keys with **Allow write
access**, the contents of `homebrew-tap-key` as the secret here, then delete both files.

## macOS signing and notarization

Without the secrets below the app is ad-hoc signed: it runs, but a downloaded copy needs
**Open Anyway** in System Settings > Privacy & Security the first time. With an Apple Developer account (Developer ID),
add these repository secrets (Settings > Secrets and variables > Actions) and the next
release is signed with the hardened runtime, notarized and stapled:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | The "Developer ID Application" certificate with its private key, exported from Keychain Access as `.p12`, then `base64 -i cert.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | The password chosen for that `.p12` export |
| `APPLE_NOTARY_KEY` | The contents of an App Store Connect API key file (`AuthKey_XXXXXXXXXX.p8`, from Users and Access > Integrations > App Store Connect API, with the Developer role) |
| `APPLE_NOTARY_KEY_ID` | That key's id |
| `APPLE_NOTARY_ISSUER` | The issuer id shown on the same page |

The signing identity is found in the imported certificate, so it needs no secret of its
own. To sign locally, set `APPLE_SIGNING_IDENTITY` (and the `APPLE_NOTARY_*` variables,
with `APPLE_NOTARY_KEY` as a path to the `.p8` file) before running
`scripts/package/macos.sh`.

## Windows

The executable carries the icon and version information (`crates/redcrab-gui/build.rs`)
but is not code-signed, so SmartScreen may warn on first start until it has built up
reputation or a signing certificate is added.
