//! The checks an update must pass before it is installed: `SHA256SUMS` is
//! signed with the release key, and the package matches it.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use ring::signature::{ED25519, UnparsedPublicKey};

/// The public half of the key the release workflow signs `SHA256SUMS`
/// with, as `openssl pkey -pubout` writes it. The private half is the
/// repository secret `UPDATE_SIGNING_KEY` (see RELEASING.md).
const RELEASE_KEY: &str = include_str!("release-key.pub.pem");

/// What precedes the key in the DER of an Ed25519 public key (RFC 8410).
const ED25519_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The release key, as [`signature`] takes it.
pub(super) fn release_key() -> [u8; 32] {
    public_key(RELEASE_KEY).expect("release-key.pub.pem is an Ed25519 public key")
}

/// Checks that `signature` is `key`'s signature of `sums`.
pub(super) fn signature(key: &[u8; 32], sums: &[u8], signature: &[u8]) -> Result<()> {
    UnparsedPublicKey::new(&ED25519, key)
        .verify(sums, signature)
        .map_err(|_| anyhow!("SHA256SUMS is not signed with redcrab's release key"))
}

/// The raw key in a PEM Ed25519 public key.
pub(super) fn public_key(pem: &str) -> Result<[u8; 32]> {
    let base64: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(base64)
        .context("not a PEM key")?;
    der.strip_prefix(&ED25519_PREFIX)
        .and_then(|key| key.try_into().ok())
        .context("not an Ed25519 public key")
}

/// The SHA-256 that `sums`, in the format `sha256sum` writes, gives for
/// the file `name`.
pub(super) fn checksum_of(sums: &[u8], name: &str) -> Result<[u8; 32]> {
    let sums = std::str::from_utf8(sums).context("SHA256SUMS is not text")?;
    for line in sums.lines() {
        let Some((hex, file)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        // Two spaces in text mode, a space and `*` in binary mode.
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        if file != name {
            continue;
        }
        let bytes = data_encoding::HEXLOWER_PERMISSIVE
            .decode(hex.as_bytes())
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        return bytes.with_context(|| format!("SHA256SUMS has no valid checksum for {name}"));
    }
    bail!("SHA256SUMS has no checksum for {name}")
}
