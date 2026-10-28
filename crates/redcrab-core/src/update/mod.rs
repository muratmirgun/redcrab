//! Updates redcrab from its GitHub releases.
//!
//! [`check`] asks GitHub for the latest published release (drafts and
//! pre-releases never show up there) and returns it when it is newer than
//! this build. [`start`] then downloads the package that fits how this copy
//! was installed ([`Install`]), checks it, puts it in place of the running
//! copy, and reports what to start instead ([`Installed::relaunch`]).
//!
//! Nothing is installed unless it matches the release's `SHA256SUMS`, and
//! `SHA256SUMS` must carry an Ed25519 signature (`SHA256SUMS.sig`) made by
//! the release workflow with the key whose public half is built into the app
//! (`release-key.pub.pem`). A file someone else uploaded to a release, or
//! changed on the way, fails that check, so an update never runs code that
//! was not signed with the release key.
//!
//! Only the builds the packaging scripts make update themselves: they set
//! `REDCRAB_SELF_UPDATE` at build time (see [`enabled`]). A build from
//! source, or one a distribution packaged, never looks for updates.
//! `REDCRAB_NO_UPDATE_CHECK` turns the checks off at run time, and
//! `REDCRAB_UPDATE_URL` asks another server for the latest release (tests);
//! the signature check holds either way.

mod install;
mod settings;
mod verify;

use std::{
    io::Read as _,
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
pub use semver::Version;
use serde::Deserialize;

pub use install::{Install, Installed};
pub use settings::{CHECK_EVERY, Settings};

use crate::{
    progress::{self, Reporter, Updates},
    runtime,
};

/// Where [`check`] asks for the latest release.
const LATEST: &str = "https://api.github.com/repos/muratmirgun/redcrab/releases/latest";

/// The checksum file of a release, and its signature.
const SUMS: &str = "SHA256SUMS";
const SUMS_SIGNATURE: &str = "SHA256SUMS.sig";

/// The version of this build.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

/// Whether this build looks for updates: one the packaging scripts made
/// (`REDCRAB_SELF_UPDATE` set at build time), unless `REDCRAB_NO_UPDATE_CHECK`
/// is set now.
pub fn enabled() -> bool {
    option_env!("REDCRAB_SELF_UPDATE").is_some_and(|v| !v.is_empty() && v != "0")
        && std::env::var_os("REDCRAB_NO_UPDATE_CHECK").is_none_or(|v| v.is_empty() || v == "0")
}

/// A published release of redcrab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The release notes, in Markdown.
    pub notes: String,
    /// The release's page on GitHub, to download it by hand.
    pub page: String,
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct Asset {
    name: String,
    size: u64,
    #[serde(rename = "browser_download_url")]
    url: String,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    /// Whether this release has what `install` needs to update to it: its
    /// package, and the signed checksums. When it does not, the user has to
    /// download it from [`Release::page`].
    pub fn installable(&self, install: &Install) -> Result<(), String> {
        if let Install::Manual { reason } = install {
            return Err(reason.clone());
        }
        let name = install
            .package(&self.version)
            .ok_or("redcrab cannot update itself on this system")?;
        if self.asset(&name).is_none() {
            return Err(format!("redcrab {} has no {name}", self.version));
        }
        if self.asset(SUMS).is_none() || self.asset(SUMS_SIGNATURE).is_none() {
            return Err(format!("redcrab {} is not signed", self.version));
        }
        Ok(())
    }
}

/// The latest published release, if it is newer than this build: `None`
/// when it is not, and when GitHub has no published release at all.
pub fn check() -> impl Future<Output = Result<Option<Release>>> + Send + 'static {
    let url = latest_url();
    runtime::blocking(move || {
        let release = latest(&url)?;
        Ok(release.filter(|release| release.version > current_version()))
    })
}

fn latest_url() -> String {
    std::env::var("REDCRAB_UPDATE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| LATEST.to_owned())
}

/// The release GitHub calls the latest at `url`, `None` when it has none.
fn latest(url: &str) -> Result<Option<Release>> {
    let mut response = match agent()
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(err) => return Err(http_error(err)).context("could not look for updates"),
    };
    let json = response
        .body_mut()
        .with_config()
        .limit(1 << 20)
        .read_to_vec()
        .context("could not look for updates")?;
    Release::from_github(&json)
}

impl Release {
    /// A release as GitHub's API describes it in JSON; `None` for a draft
    /// or a pre-release, which are never offered.
    pub fn from_github(json: &[u8]) -> Result<Option<Self>> {
        #[derive(Deserialize)]
        struct GitHubRelease {
            tag_name: String,
            #[serde(default)]
            body: Option<String>,
            html_url: String,
            #[serde(default)]
            draft: bool,
            #[serde(default)]
            prerelease: bool,
            #[serde(default)]
            assets: Vec<Asset>,
        }

        let release: GitHubRelease = serde_json::from_slice(json)
            .context("GitHub's answer about the latest release is not what redcrab expects")?;
        if release.draft || release.prerelease {
            return Ok(None);
        }
        let version =
            Version::parse(release.tag_name.trim_start_matches('v')).with_context(|| {
                format!("the latest release, {}, is not a version", release.tag_name)
            })?;
        Ok(Some(Self {
            version,
            notes: release.body.unwrap_or_default(),
            page: release.html_url,
            assets: release.assets,
        }))
    }
}

/// What a running update reports. The last report says how it ended:
/// [`UpdateProgress::Installed`], [`UpdateProgress::Failed`] or
/// [`UpdateProgress::Stopped`].
#[derive(Debug)]
pub enum UpdateProgress {
    /// Downloading the package: bytes so far, of `total`.
    Downloading {
        done: u64,
        total: u64,
    },
    /// Checking the package and putting it in place. Stopping no longer
    /// works from here on.
    Installing,
    /// The new version is in place of this one, which keeps running until
    /// it quits; [`Installed::relaunch`] starts the new one.
    Installed(Installed),
    Failed(String),
    /// Stopped while downloading, as asked; nothing changed.
    Stopped,
}

/// Downloads `release` and installs it as `install` says, on a thread of
/// its own. Dropping the updates, or their stopper, stops it while it
/// downloads; once it installs, it finishes.
pub fn start(release: Release, install: Install) -> Updates<UpdateProgress> {
    let (reporter, updates) = progress::channel();
    runtime::detach_blocking(move || {
        let ending = match update(&release, &install, &verify::release_key(), &reporter) {
            Ok(Some(installed)) => UpdateProgress::Installed(installed),
            Ok(None) => UpdateProgress::Stopped,
            Err(err) => {
                tracing::warn!("update to {} failed: {err:#}", release.version);
                UpdateProgress::Failed(format!("{err:#}"))
            }
        };
        reporter.update(ending);
    });
    updates
}

/// The update itself, trusting `SHA256SUMS` signed with `key`: `None` when
/// it was stopped.
fn update(
    release: &Release,
    install: &Install,
    key: &[u8; 32],
    reporter: &Reporter<UpdateProgress>,
) -> Result<Option<Installed>> {
    release
        .installable(install)
        .map_err(|reason| anyhow!(reason))?;
    let name = install.package(&release.version).expect("installable");
    let package = release.asset(&name).expect("installable");

    // The signed checksums first: a release that fails them is not worth
    // downloading.
    let sums = fetch(release.asset(SUMS).expect("installable"), 1 << 16)?;
    let signature = fetch(release.asset(SUMS_SIGNATURE).expect("installable"), 1 << 10)?;
    verify::signature(key, &sums, &signature)?;
    let expected = verify::checksum_of(&sums, &name)?;

    let work = install::WorkDir::new()?;
    let file = work.path().join(&name);
    let total = package.size;
    reporter.update(UpdateProgress::Downloading { done: 0, total });
    let Some(digest) = download(package, &file, |done| {
        reporter.update(UpdateProgress::Downloading { done, total });
        !(reporter.stop_requested() || reporter.is_abandoned())
    })?
    else {
        return Ok(None);
    };
    if digest != expected {
        bail!("the download of {name} does not match its signed checksum");
    }

    reporter.update(UpdateProgress::Installing);
    install.install(&file, &release.version, &work).map(Some)
}

/// A small file of a release, whole.
fn fetch(asset: &Asset, limit: u64) -> Result<Vec<u8>> {
    let mut response = agent()
        .get(&asset.url)
        .call()
        .map_err(http_error)
        .with_context(|| format!("could not download {}", asset.name))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("could not download {}", asset.name))
}

/// Downloads `asset` into `file`, calling `go_on` with the bytes so far
/// after each piece; it returns `false` to stop. The file's SHA-256, or
/// `None` when stopped.
fn download(
    asset: &Asset,
    file: &std::path::Path,
    mut go_on: impl FnMut(u64) -> bool,
) -> Result<Option<[u8; 32]>> {
    use std::io::Write as _;

    let context = || format!("could not download {}", asset.name);
    let mut response = agent()
        .get(&asset.url)
        .call()
        .map_err(http_error)
        .with_context(context)?;
    // One byte past the size GitHub gave, so that a longer body is caught.
    let mut body = response
        .body_mut()
        .with_config()
        .limit(asset.size + 1)
        .reader();
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(file)
            .with_context(|| format!("could not write {}", file.display()))?,
    );
    let mut sha = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0; 64 * 1024];
    let mut done = 0;
    loop {
        let n = body.read(&mut buf).with_context(context)?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > asset.size {
            bail!("{} is larger than the release says", asset.name);
        }
        sha.update(&buf[..n]);
        out.write_all(&buf[..n])
            .with_context(|| format!("could not write {}", file.display()))?;
        if !go_on(done) {
            return Ok(None);
        }
    }
    if done != asset.size {
        bail!("the download of {} ended early", asset.name);
    }
    out.flush()
        .with_context(|| format!("could not write {}", file.display()))?;
    Ok(Some(
        sha.finish()
            .as_ref()
            .try_into()
            .expect("SHA-256 is 32 bytes"),
    ))
}

/// The HTTP client: the platform's trust store, rustls with ring, and
/// GitHub's redirects to its download host. Plain HTTP only when
/// `REDCRAB_UPDATE_URL` asks for it (tests); the signature check does not
/// rely on the transport anyway.
fn agent() -> &'static ureq::Agent {
    static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
        use ureq::tls::{RootCerts, TlsConfig};

        ureq::Agent::config_builder()
            .user_agent(concat!("redcrab/", env!("CARGO_PKG_VERSION")))
            .https_only(!cfg!(test) && latest_url().starts_with("https://"))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .unversioned_rustls_crypto_provider(Arc::new(
                        rustls::crypto::ring::default_provider(),
                    ))
                    .build(),
            )
            .build()
            .new_agent()
    });
    &AGENT
}

/// An HTTP error as the user reads it.
fn http_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::StatusCode(403 | 429) => {
            anyhow!("GitHub is limiting requests from this network for now; try again later")
        }
        ureq::Error::StatusCode(code) => anyhow!("GitHub answered with HTTP status {code}"),
        err => anyhow::Error::new(err),
    }
}

#[cfg(test)]
mod tests;
