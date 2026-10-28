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
   Windows `.zip`, checks that each one starts, and creates a **draft** release with the
   files, a `SHA256SUMS` file and notes generated from the commits.
4. Read the draft on GitHub, edit the notes if needed, and publish it.

Packaging changes are tried out before a release: pushes to `main` that touch
`scripts/package/`, the release workflow, `assets/logo/` or the GUI's `build.rs` run the
same builds without publishing anything. The scripts also run locally (see the README's
"Packaging" section).

## macOS signing and notarization

Without the secrets below the app is ad-hoc signed: it runs, but a downloaded copy needs
a right-click > **Open** the first time. With an Apple Developer account (Developer ID),
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
