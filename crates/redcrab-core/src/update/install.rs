//! How the running copy of redcrab was installed, and how an update puts
//! the new version in its place.
//!
//! The new version is unpacked next to the old one first, in a hidden
//! `.redcrab-update-<pid>` file or folder, and asked for its version
//! (`--version`). Only then does it replace the old one, with a rename on
//! the same file system, so a failed update leaves the old copy as it was.

use std::{
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result, bail};

use super::Version;

/// How this copy of redcrab was installed, which decides how it updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// macOS: the app bundle, replaced with the one in the release's `.dmg`.
    MacApp { bundle: PathBuf },
    /// Linux, from the `.deb`: the new `.deb` goes in with `pkexec
    /// apt-get install`, which asks for an administrator's password.
    Deb { arch: &'static str },
    /// The Linux tarball or the Windows zip, unpacked in a folder this user
    /// can write to: the executable is replaced in place.
    Portable { exe: PathBuf },
    /// This copy cannot replace itself, for the reason given: the user
    /// downloads the new release from its page.
    Manual { reason: String },
}

impl Install {
    /// How the running copy was installed.
    pub fn detect() -> Self {
        match std::env::current_exe().and_then(fs::canonicalize) {
            Ok(exe) => Self::at(&exe),
            Err(err) => manual(format!("redcrab could not find its own executable: {err}")),
        }
    }

    fn at(exe: &Path) -> Self {
        if cfg!(target_os = "macos") {
            Self::mac(exe)
        } else if cfg!(target_os = "linux") {
            Self::linux(exe)
        } else if cfg!(windows) {
            Self::portable(exe)
        } else {
            manual("redcrab cannot update itself on this system")
        }
    }

    /// `<bundle>.app/Contents/MacOS/redcrab`, in a folder this user can
    /// write to.
    pub(super) fn mac(exe: &Path) -> Self {
        let bundle = exe
            .parent()
            .filter(|dir| dir.ends_with("Contents/MacOS"))
            .and_then(|dir| dir.parent()?.parent())
            .filter(|bundle| bundle.extension().is_some_and(|ext| ext == "app"));
        let Some(bundle) = bundle else {
            return manual("redcrab is not running from its app bundle");
        };
        // Gatekeeper runs a downloaded app that was never moved from a
        // random read-only place (App Translocation).
        if bundle
            .components()
            .any(|part| part.as_os_str() == "AppTranslocation")
        {
            return manual(
                "macOS runs this copy of redcrab from a temporary place; \
                 move redcrab to the Applications folder to update it automatically",
            );
        }
        let folder = bundle.parent().unwrap_or(Path::new("/"));
        if !writable(folder) {
            return manual(if folder.starts_with("/Volumes") {
                "redcrab is running from its disk image; drag it to the Applications \
                 folder to update it automatically"
                    .to_owned()
            } else {
                format!("redcrab cannot write to {}", folder.display())
            });
        }
        Self::MacApp {
            bundle: bundle.to_owned(),
        }
    }

    /// `/usr/bin/redcrab` from the `.deb`, or a tarball's binary.
    fn linux(exe: &Path) -> Self {
        if !exe.starts_with("/usr") {
            return Self::portable(exe);
        }
        if exe != Path::new("/usr/bin/redcrab")
            || !Path::new("/var/lib/dpkg/info/redcrab.list").exists()
        {
            return manual(
                "redcrab was installed by the system's package manager; update it there",
            );
        }
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => return manual("redcrab has no .deb for this processor"),
        };
        if find_program("pkexec").is_none() {
            return manual("installing the new .deb needs pkexec (polkit), which is not installed");
        }
        Self::Deb { arch }
    }

    pub(super) fn portable(exe: &Path) -> Self {
        match exe.parent() {
            Some(folder) if writable(folder) => Self::Portable {
                exe: exe.to_owned(),
            },
            Some(folder) => manual(format!("redcrab cannot write to {}", folder.display())),
            None => manual("redcrab could not find its own folder"),
        }
    }

    /// The release file this install updates from.
    pub(super) fn package(&self, version: &Version) -> Option<String> {
        self.package_for(version, std::env::consts::OS, std::env::consts::ARCH)
    }

    pub(super) fn package_for(&self, version: &Version, os: &str, arch: &str) -> Option<String> {
        match self {
            Self::MacApp { .. } => Some(format!("redcrab-{version}-macos-universal.dmg")),
            Self::Deb { arch } => Some(format!("redcrab_{version}_{arch}.deb")),
            Self::Portable { .. } => match os {
                "linux" => Some(format!("redcrab-{version}-linux-{arch}.tar.gz")),
                "windows" => Some(format!("redcrab-{version}-windows-{arch}.zip")),
                _ => None,
            },
            Self::Manual { .. } => None,
        }
    }

    /// Installs `version` from its downloaded `package`, using `work` for
    /// scratch space.
    pub(super) fn install(
        &self,
        package: &Path,
        version: &Version,
        work: &WorkDir,
    ) -> Result<Installed> {
        match self {
            Self::MacApp { bundle } => install_app(bundle, package, version, work),
            Self::Deb { .. } => install_deb(package, version),
            Self::Portable { exe } => install_portable(exe, package, version, work),
            Self::Manual { reason } => bail!("{reason}"),
        }
    }

    /// Removes what an earlier update left next to this copy: the old
    /// version, when it could not be removed while it ran (Windows), and
    /// the pieces of an update that was cut short. Only leftovers more
    /// than an hour old, so an update another copy runs right now is safe.
    pub fn clean_up(&self) {
        let folder = match self {
            Self::MacApp { bundle } => bundle.parent(),
            Self::Portable { exe } => exe.parent(),
            Self::Deb { .. } | Self::Manual { .. } => None,
        };
        let Some(entries) = folder.and_then(|folder| fs::read_dir(folder).ok()) else {
            return;
        };
        let an_hour_ago = SystemTime::now() - Duration::from_secs(60 * 60);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with(".redcrab-update-") || name.starts_with(".redcrab-old-")) {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified < an_hour_ago);
            if old {
                remove(&entry.path());
            }
        }
    }
}

fn manual(reason: impl Into<String>) -> Install {
    Install::Manual {
        reason: reason.into(),
    }
}

/// A new version, installed in place of the running one.
#[derive(Debug)]
pub struct Installed {
    version: Version,
    /// The new version's executable.
    exe: PathBuf,
}

impl Installed {
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Starts the new version, with this one's environment; the running
    /// one should quit right after. On macOS too the executable starts
    /// directly, not through `open`, which would drop the environment.
    pub fn relaunch(&self) -> Result<()> {
        Command::new(&self.exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(drop)
            .context("could not start the new version")
    }
}

/// A scratch folder for one update, in the temporary directory, removed
/// when dropped.
pub(super) struct WorkDir(PathBuf);

impl WorkDir {
    pub(super) fn new() -> Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "redcrab-update-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        remove(&path);
        fs::create_dir_all(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        Ok(Self(path))
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        remove(&self.0);
    }
}

/// A hidden file or folder next to the installed copy that the new version
/// is staged in, removed when dropped unless it was moved into place.
struct Staging(PathBuf);

impl Staging {
    fn new(folder: &Path, suffix: &str) -> Self {
        let path = folder.join(format!(".redcrab-update-{}{suffix}", std::process::id()));
        remove(&path);
        Self(path)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        remove(&self.0);
    }
}

/// The executable in redcrab's app bundle (`CFBundleExecutable`).
const APP_EXE: &str = "Contents/MacOS/redcrab";

/// macOS: copies the app out of the disk image next to `bundle`, then
/// swaps the two.
fn install_app(bundle: &Path, dmg: &Path, version: &Version, work: &WorkDir) -> Result<Installed> {
    let folder = bundle.parent().context("the app has no folder")?;
    let mount = work.path().join("mount");
    fs::create_dir(&mount)?;
    run(
        Command::new("/usr/bin/hdiutil")
            .args(["attach", "-nobrowse", "-noautoopen", "-readonly"])
            // The checksum covered the whole image already.
            .args(["-noverify", "-quiet", "-mountpoint"])
            .arg(&mount)
            .arg(dmg),
        "could not open the disk image",
    )?;
    let staging = Staging::new(folder, "");
    let copied = copy_app_from(&mount, &staging.0);
    detach(&mount);
    let new = copied?;

    check_version(&new.join(APP_EXE), version)?;
    // The old bundle moves into the staging folder, which goes with it.
    swap(bundle, &new, &staging.0.join("previous.app"))?;
    Ok(Installed {
        version: version.clone(),
        exe: bundle.join(APP_EXE),
    })
}

/// Copies the one app in the mounted image into the folder `staging`.
fn copy_app_from(mount: &Path, staging: &Path) -> Result<PathBuf> {
    let app = fs::read_dir(mount)?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
        .context("the disk image holds no app")?;
    fs::create_dir(staging).with_context(|| format!("could not create {}", staging.display()))?;
    let new = staging.join("redcrab.app");
    // ditto keeps the bundle exactly, signature and extended attributes
    // included.
    run(
        Command::new("/usr/bin/ditto").arg(&app).arg(&new),
        "could not copy the new app",
    )?;
    Ok(new)
}

fn detach(mount: &Path) {
    let detached = |force: bool| {
        let mut command = Command::new("/usr/bin/hdiutil");
        command.args(["detach", "-quiet"]).arg(mount);
        if force {
            command.arg("-force");
        }
        run(&mut command, "could not close the disk image")
    };
    if let Err(err) = detached(false).or_else(|_| detached(true)) {
        tracing::warn!("{err:#}");
    }
}

/// Linux: installs the `.deb` as root, through polkit.
fn install_deb(deb: &Path, version: &Version) -> Result<Installed> {
    let output = Command::new(find_program("pkexec").context("pkexec is not installed")?)
        .args(["/usr/bin/apt-get", "install", "-y"])
        .arg(deb)
        .stdin(Stdio::null())
        .output()
        .context("could not run pkexec")?;
    match output.status.code() {
        Some(0) => {}
        // The password dialog was dismissed, or the user may not do this.
        Some(126 | 127) => bail!("installing the update was not authorized"),
        _ => bail!(
            "apt-get could not install the update: {}",
            last_line(&output.stderr)
        ),
    }
    let exe = PathBuf::from("/usr/bin/redcrab");
    check_version(&exe, version)?;
    Ok(Installed {
        version: version.clone(),
        exe,
    })
}

/// The Linux tarball or the Windows zip: unpacks it and puts its
/// executable in place of `exe`.
fn install_portable(
    exe: &Path,
    archive: &Path,
    version: &Version,
    work: &WorkDir,
) -> Result<Installed> {
    let folder = exe
        .parent()
        .context("redcrab could not find its own folder")?;
    let unpacked = work.path().join("unpacked");
    fs::create_dir(&unpacked)?;
    unpack(archive, &unpacked)?;
    // redcrab-<version>-<os>-<arch>/redcrab[.exe], as the packaging
    // scripts lay it out.
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .context("the package has no name")?;
    let top = name
        .strip_suffix(".tar.gz")
        .or_else(|| name.strip_suffix(".zip"))
        .context("the package is not a .tar.gz or .zip")?;
    let file_name = exe
        .file_name()
        .context("redcrab's executable has no name")?;
    let new = unpacked.join(top).join(file_name);
    if !new.is_file() {
        bail!("{name} holds no {}", Path::new(file_name).display());
    }

    let staging = Staging::new(folder, std::env::consts::EXE_SUFFIX);
    // A copy, since the temporary directory may be on another file system;
    // it keeps the executable bit.
    fs::copy(&new, &staging.0)
        .with_context(|| format!("could not write {}", staging.0.display()))?;
    check_version(&staging.0, version)?;
    if cfg!(windows) {
        // Windows will not replace or remove a running executable, but it
        // renames one; the old copy is removed on a later start
        // (`Install::clean_up`).
        let old = folder.join(format!(".redcrab-old-{}.exe", std::process::id()));
        swap(exe, &staging.0, &old)?;
    } else {
        fs::rename(&staging.0, exe)
            .with_context(|| format!("could not replace {}", exe.display()))?;
    }
    Ok(Installed {
        version: version.clone(),
        exe: exe.to_owned(),
    })
}

/// Unpacks a `.tar.gz` or `.zip` with the system's tar: GNU tar on Linux,
/// and on Windows the bsdtar that ships with it, which reads zip files.
pub(super) fn unpack(archive: &Path, into: &Path) -> Result<()> {
    let tar = if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Path::new(&root).join(r"System32\tar.exe")
    } else {
        PathBuf::from("tar")
    };
    run(
        Command::new(tar)
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(into),
        "could not unpack the update",
    )
}

/// Puts `new` at `target`, moving what was there to `old`, and puts it
/// back if that fails.
fn swap(target: &Path, new: &Path, old: &Path) -> Result<()> {
    fs::rename(target, old).with_context(|| format!("could not move {}", target.display()))?;
    if let Err(err) = fs::rename(new, target) {
        if let Err(back) = fs::rename(old, target) {
            tracing::error!("could not put {} back: {back}", target.display());
        }
        return Err(err).with_context(|| format!("could not replace {}", target.display()));
    }
    // The old copy is no longer needed; Windows keeps it while it runs.
    remove(old);
    Ok(())
}

/// Runs `exe --version` and checks that it is redcrab `version`.
pub(super) fn check_version(exe: &Path, version: &Version) -> Result<()> {
    let mut tries = 0;
    let mut child = loop {
        let spawned = Command::new(exe)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            // A file just written can be busy for a moment on Linux: a
            // process forked meanwhile holds it open until it execs.
            Err(err) if err.kind() == io::ErrorKind::ExecutableFileBusy && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            spawned => {
                break spawned.with_context(|| {
                    format!("could not start the new version ({})", exe.display())
                })?;
            }
        }
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the new version did not answer");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut said = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut said);
    }
    let expected = format!("redcrab {version}");
    if !status.success() || said.trim() != expected {
        bail!(
            "the downloaded program is not {expected}: it says {:?}",
            said.trim()
        );
    }
    Ok(())
}

fn run(command: &mut Command, what: &str) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("{what}: could not run {}", command.get_program().display()))?;
    if !output.status.success() {
        bail!("{what}: {}", last_line(&output.stderr));
    }
    Ok(())
}

/// The last line of a program's error output, to say why it failed.
fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no reason given")
        .to_owned()
}

/// Whether this user can create files in `folder`. Permission bits do not
/// tell (ACLs, read-only volumes), so it tries.
fn writable(folder: &Path) -> bool {
    let probe = folder.join(format!(".redcrab-update-probe-{}", std::process::id()));
    match fs::create_dir(&probe) {
        Ok(()) => {
            let _ = fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Removes a file or a folder, if there is one; failures are only logged.
fn remove(path: &Path) {
    let removed = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => return,
    };
    if let Err(err) = removed {
        tracing::debug!("could not remove {}: {err}", path.display());
    }
}
