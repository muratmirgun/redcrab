use std::{
    collections::HashMap,
    fs,
    io::{BufRead as _, BufReader, Write as _},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};

use ring::signature::{Ed25519KeyPair, KeyPair as _};

use super::{settings::Settings, *};

/// A scratch folder for one test, removed when dropped.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("redcrab-update-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        // The canonical path, as `current_exe` gives it (/private/var on
        // macOS). Not on Windows, where that is a `\\?\` path, in which `/`
        // does not separate.
        #[cfg(unix)]
        let path = fs::canonicalize(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A minimal HTTP/1.1 server: the files put in it, 404 for anything else.
struct Server {
    base: String,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files = Arc::new(Mutex::new(HashMap::new()));
        let served = files.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let files = served.clone();
                thread::spawn(move || answer(stream, &files));
            }
        });
        Self { base, files }
    }

    fn put(&self, path: &str, body: impl Into<Vec<u8>>) -> String {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), body.into());
        format!("{}{path}", self.base)
    }
}

fn answer(mut stream: TcpStream, files: &Mutex<HashMap<String, Vec<u8>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let body = files.lock().unwrap().get(path).cloned();
    let _ = match body {
        Some(body) => write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .and_then(|()| stream.write_all(&body)),
        None => write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    };
}

fn test_key() -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap()
}

fn public(key: &Ed25519KeyPair) -> [u8; 32] {
    key.public_key().as_ref().try_into().unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn version(v: &str) -> Version {
    Version::parse(v).unwrap()
}

/// Publishes a release of `v` on `server` with `package` and its
/// `SHA256SUMS` signed with `key`, as the release workflow lays them out.
fn publish(server: &Server, v: &str, name: &str, package: &[u8], key: &Ed25519KeyPair) -> Release {
    let sums = format!(
        "{}  {name}\n{}  other.zip\n",
        sha256_hex(package),
        "ab".repeat(32)
    );
    let signature = key.sign(sums.as_bytes());
    let asset = |name: &str, body: &[u8]| Asset {
        name: name.to_owned(),
        size: body.len() as u64,
        url: server.put(&format!("/download/v{v}/{name}"), body),
    };
    Release {
        version: version(v),
        notes: String::new(),
        page: format!("{}/releases/v{v}", server.base),
        assets: vec![
            asset(name, package),
            asset(SUMS, sums.as_bytes()),
            asset(SUMS_SIGNATURE, signature.as_ref()),
        ],
    }
}

/// Runs an update to its end, as [`start`] does.
#[cfg(unix)]
fn run_update(release: &Release, install: &Install, key: &[u8; 32]) -> Result<Option<Installed>> {
    let (reporter, _updates) = progress::channel();
    update(release, install, key, &reporter)
}

#[test]
fn the_release_key_is_an_ed25519_key() {
    assert_eq!(verify::release_key().len(), 32);
}

#[test]
fn public_keys_parse_from_pem() {
    let key = test_key();
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(key.public_key().as_ref());
    let pem = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &der)
    );
    assert_eq!(verify::public_key(&pem).unwrap(), public(&key));

    // Not base64, cut short, or another kind of key.
    assert!(verify::public_key("-----BEGIN PUBLIC KEY-----\n!!\n-----END").is_err());
    let short = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &der[..40]);
    assert!(verify::public_key(&short).is_err());
    der[8] = 0x71; // Ed448's OID
    let other = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &der);
    assert!(verify::public_key(&other).is_err());
}

#[test]
fn signatures_check_the_whole_file_and_the_key() {
    let key = test_key();
    let sums = b"00  a.dmg\n";
    let signature = key.sign(sums);
    verify::signature(&public(&key), sums, signature.as_ref()).unwrap();
    assert!(verify::signature(&public(&key), b"01  a.dmg\n", signature.as_ref()).is_err());
    assert!(verify::signature(&public(&key), sums, &signature.as_ref()[1..]).is_err());
    let other = Ed25519KeyPair::from_seed_unchecked(&[8; 32]).unwrap();
    assert!(verify::signature(&public(&other), sums, signature.as_ref()).is_err());
    // A signature by the test key is not one by the release key.
    assert!(verify::signature(&verify::release_key(), sums, signature.as_ref()).is_err());
}

#[test]
fn checksums_are_read_as_sha256sum_writes_them() {
    let a = "a".repeat(64);
    let b = "B".repeat(64);
    let sums = format!("{a}  redcrab.dmg\n{b} *redcrab.zip\n12  short.deb\n\n");
    assert_eq!(
        verify::checksum_of(sums.as_bytes(), "redcrab.dmg").unwrap(),
        [0xaa; 32]
    );
    assert_eq!(
        verify::checksum_of(sums.as_bytes(), "redcrab.zip").unwrap(),
        [0xbb; 32]
    );
    assert!(verify::checksum_of(sums.as_bytes(), "short.deb").is_err());
    assert!(verify::checksum_of(sums.as_bytes(), "missing.tar.gz").is_err());
    // A name is matched whole.
    assert!(verify::checksum_of(sums.as_bytes(), "redcrab").is_err());
    assert!(verify::checksum_of(b"\xff\xfe", "redcrab.dmg").is_err());
}

#[test]
fn the_latest_release_is_read_from_github() {
    let server = Server::start();
    let url = server.put(
        "/releases/latest",
        r#"{
            "tag_name": "v1.2.3",
            "body": "* Faster",
            "html_url": "https://github.com/muratmirgun/redcrab/releases/tag/v1.2.3",
            "draft": false,
            "prerelease": false,
            "assets": [{"name": "SHA256SUMS", "size": 10, "browser_download_url": "https://x/SHA256SUMS", "state": "uploaded"}]
        }"#,
    );
    let release = latest(&url).unwrap().unwrap();
    assert_eq!(release.version, version("1.2.3"));
    assert_eq!(release.notes, "* Faster");
    assert_eq!(
        release.page,
        "https://github.com/muratmirgun/redcrab/releases/tag/v1.2.3"
    );
    assert_eq!(release.asset(SUMS).unwrap().size, 10);

    // No release yet.
    assert_eq!(latest(&format!("{}/nothing", server.base)).unwrap(), None);
    // Never a draft or a pre-release, should one show up.
    let url = server.put(
        "/draft",
        r#"{"tag_name": "v2.0.0", "html_url": "x", "draft": true, "assets": []}"#,
    );
    assert_eq!(latest(&url).unwrap(), None);
    let url = server.put(
        "/pre",
        r#"{"tag_name": "v2.0.0-rc.1", "html_url": "x", "prerelease": true}"#,
    );
    assert_eq!(latest(&url).unwrap(), None);
    // A tag that is not a version, and an answer that is not a release.
    let url = server.put("/odd", r#"{"tag_name": "nightly", "html_url": "x"}"#);
    assert!(latest(&url).is_err());
    let url = server.put("/html", "<html>rate limited</html>");
    assert!(latest(&url).is_err());
}

#[test]
fn downloads_report_progress_and_stop_when_asked() {
    let temp = Temp::new("download");
    let server = Server::start();
    let body: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
    let asset = Asset {
        name: "big.zip".into(),
        size: body.len() as u64,
        url: server.put("/big.zip", body.clone()),
    };
    let file = temp.0.join("big.zip");

    let mut seen = Vec::new();
    let digest = download(&asset, &file, |done| {
        seen.push(done);
        true
    })
    .unwrap()
    .unwrap();
    assert_eq!(data_encoding::HEXLOWER.encode(&digest), sha256_hex(&body));
    assert_eq!(fs::read(&file).unwrap(), body);
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(seen.last(), Some(&(body.len() as u64)));

    let mut calls = 0;
    let stopped = download(&asset, &file, |_| {
        calls += 1;
        false
    })
    .unwrap();
    assert_eq!((stopped, calls), (None, 1));

    // GitHub said it is smaller than it is.
    let short = Asset {
        size: 1000,
        ..asset
    };
    assert!(download(&short, &file, |_| true).is_err());
}

#[test]
fn packages_are_named_as_the_packaging_scripts_name_them() {
    let v = version("0.2.0");
    let app = Install::MacApp {
        bundle: "/Applications/redcrab.app".into(),
    };
    let portable = Install::Portable {
        exe: "/opt/redcrab/redcrab".into(),
    };
    assert_eq!(
        app.package_for(&v, "macos", "aarch64").unwrap(),
        "redcrab-0.2.0-macos-universal.dmg"
    );
    assert_eq!(
        Install::Deb { arch: "arm64" }
            .package_for(&v, "linux", "aarch64")
            .unwrap(),
        "redcrab_0.2.0_arm64.deb"
    );
    assert_eq!(
        portable.package_for(&v, "linux", "x86_64").unwrap(),
        "redcrab-0.2.0-linux-x86_64.tar.gz"
    );
    assert_eq!(
        portable.package_for(&v, "windows", "x86_64").unwrap(),
        "redcrab-0.2.0-windows-x86_64.zip"
    );
    assert_eq!(portable.package_for(&v, "freebsd", "x86_64"), None);
    let manual = Install::Manual {
        reason: "no".into(),
    };
    assert_eq!(manual.package_for(&v, "linux", "x86_64"), None);
}

#[test]
fn a_release_says_why_it_cannot_be_installed() {
    let server = Server::start();
    let key = test_key();
    let install = Install::Deb { arch: "amd64" };
    let mut release = publish(&server, "0.2.0", "redcrab_0.2.0_amd64.deb", b"deb", &key);
    assert_eq!(release.installable(&install), Ok(()));

    let manual = Install::Manual {
        reason: "redcrab cannot write to /opt".into(),
    };
    assert_eq!(
        release.installable(&manual),
        Err("redcrab cannot write to /opt".into())
    );
    assert_eq!(
        release.installable(&Install::Deb { arch: "arm64" }),
        Err("redcrab 0.2.0 has no redcrab_0.2.0_arm64.deb".into())
    );
    release.assets.retain(|asset| asset.name != SUMS_SIGNATURE);
    assert_eq!(
        release.installable(&install),
        Err("redcrab 0.2.0 is not signed".into())
    );
}

#[test]
fn checks_run_daily_and_skipped_releases_are_not_offered() {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let mut settings = Settings::default();
    assert!(settings.due(start));
    settings.checked(start);
    assert!(!settings.due(start + Duration::from_secs(60)));
    assert!(settings.due(start + CHECK_EVERY));
    // A clock set back does not hold the checks off.
    assert!(settings.due(start - Duration::from_secs(60)));
    settings.automatic = false;
    assert!(!settings.due(start + CHECK_EVERY));

    let server = Server::start();
    let release = publish(&server, "0.2.0", "x.dmg", b"", &test_key());
    assert!(settings.offers(&release));
    settings.skipped = Some(version("0.2.0"));
    assert!(!settings.offers(&release));
    let newer = publish(&server, "0.3.0", "x.dmg", b"", &test_key());
    assert!(settings.offers(&newer));
}

#[test]
fn settings_survive_a_restart() {
    let temp = Temp::new("settings");
    let path = temp.0.join("dir/update.json");
    assert_eq!(Settings::load_from(&path), Settings::default());
    let settings = Settings {
        automatic: false,
        last_check: 42,
        skipped: Some(version("0.3.1")),
    };
    settings.save_to(&path).unwrap();
    assert_eq!(Settings::load_from(&path), settings);
    // A file that does not read falls back to the defaults, as does a
    // field from a later version.
    fs::write(&path, "{").unwrap();
    assert_eq!(Settings::load_from(&path), Settings::default());
    fs::write(&path, r#"{"automatic": false, "channel": "beta"}"#).unwrap();
    assert!(!Settings::load_from(&path).automatic);
}

#[test]
fn a_mac_app_updates_when_its_folder_is_writable() {
    let temp = Temp::new("detect-mac");
    let exe = temp
        .0
        .join("Applications/redcrab.app/Contents/MacOS/redcrab");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, "").unwrap();
    assert_eq!(
        Install::mac(&exe),
        Install::MacApp {
            bundle: temp.0.join("Applications/redcrab.app")
        }
    );
    assert!(matches!(
        Install::mac(&temp.0.join("target/release/redcrab")),
        Install::Manual { .. }
    ));
    let translocated = temp
        .0
        .join("AppTranslocation/X/d/redcrab.app/Contents/MacOS/redcrab");
    assert!(
        matches!(Install::mac(&translocated), Install::Manual { reason } if reason.contains("Applications"))
    );
}

#[cfg(unix)]
#[test]
fn a_copy_in_a_read_only_folder_is_updated_by_hand() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = Temp::new("detect-ro");
    let exe = temp.0.join("bin/redcrab");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(&exe, "").unwrap();
    assert_eq!(
        Install::portable(&exe),
        Install::Portable { exe: exe.clone() }
    );
    let dir = exe.parent().unwrap();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    let detected = Install::portable(&exe);
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    // Root writes anywhere; the check is about this user.
    if !matches!(detected, Install::Portable { .. }) {
        assert!(matches!(detected, Install::Manual { reason } if reason.contains("cannot write")));
    }
}

#[test]
fn clean_up_removes_only_old_leftovers() {
    let temp = Temp::new("clean-up");
    let exe = temp.0.join("redcrab");
    let old = temp.0.join(".redcrab-old-1.exe");
    let staged = temp.0.join(".redcrab-update-2");
    let fresh = temp.0.join(".redcrab-update-3");
    let other = temp.0.join(".redcrab-notes");
    for path in [&exe, &old, &staged, &fresh, &other] {
        fs::write(path, "").unwrap();
    }
    let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
    for path in [&exe, &old, &staged, &other] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();
    }

    Install::Portable { exe: exe.clone() }.clean_up();
    assert!(!old.exists());
    assert!(!staged.exists());
    assert!(fresh.exists());
    assert!(other.exists());
    assert!(exe.exists());
}

/// A stand-in for redcrab's executable: a script that answers
/// `--version` as redcrab does.
#[cfg(unix)]
fn fake_redcrab(path: &Path, v: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\necho 'redcrab {v}'\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn say_version(exe: &Path) -> String {
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The Linux tarball of `v`, with a fake redcrab saying `says`.
#[cfg(target_os = "linux")]
fn tarball(temp: &Temp, v: &str, says: &str) -> (String, Vec<u8>) {
    let top = format!("redcrab-{v}-linux-{}", std::env::consts::ARCH);
    let src = temp.0.join(format!("src-{says}"));
    fake_redcrab(&src.join(&top).join("redcrab"), says);
    let name = format!("{top}.tar.gz");
    let out = temp.0.join(&name);
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(&src)
        .arg(&top)
        .status()
        .unwrap();
    assert!(status.success());
    (name, fs::read(out).unwrap())
}

#[cfg(target_os = "linux")]
#[test]
fn a_tarball_install_updates_in_place() {
    let temp = Temp::new("portable");
    let server = Server::start();
    let key = test_key();
    let exe = temp.0.join("app/redcrab");
    fake_redcrab(&exe, "0.1.0");
    let install = Install::Portable { exe: exe.clone() };

    let (name, package) = tarball(&temp, "9.0.0", "9.0.0");
    let release = publish(&server, "9.0.0", &name, &package, &key);
    let installed = run_update(&release, &install, &public(&key))
        .unwrap()
        .unwrap();
    assert_eq!(installed.version(), &version("9.0.0"));
    assert_eq!(say_version(&exe), "redcrab 9.0.0");
    // Nothing is left next to it.
    let left: Vec<_> = fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left, ["redcrab"]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_bad_update_leaves_the_installed_copy_alone() {
    let temp = Temp::new("bad");
    let server = Server::start();
    let key = test_key();
    let exe = temp.0.join("app/redcrab");
    fake_redcrab(&exe, "0.1.0");
    let install = Install::Portable { exe: exe.clone() };
    let (name, package) = tarball(&temp, "9.0.0", "9.0.0");
    let failed = |release: &Release, key: &[u8; 32]| {
        let error = run_update(release, &install, key).unwrap_err();
        assert_eq!(say_version(&exe), "redcrab 0.1.0");
        format!("{error:#}")
    };

    // Signed with another key.
    let release = publish(&server, "9.0.0", &name, &package, &key);
    let other = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
    assert!(failed(&release, &public(&other)).contains("not signed with redcrab's release key"));

    // The package changed after it was signed.
    let mut tampered = package.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    server.put(&format!("/download/v9.0.0/{name}"), tampered);
    assert!(failed(&release, &public(&key)).contains("does not match its signed checksum"));

    // Cut short.
    server.put(&format!("/download/v9.0.0/{name}"), &package[..10]);
    assert!(failed(&release, &public(&key)).contains("ended early"));

    // The program in it is another version.
    let (name, package) = tarball(&temp, "9.0.0", "8.0.0");
    let release = publish(&server, "9.0.0", &name, &package, &key);
    assert!(failed(&release, &public(&key)).contains("is not redcrab 9.0.0"));
    let left: Vec<_> = fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left, ["redcrab"]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_stopped_update_changes_nothing() {
    let temp = Temp::new("stopped");
    let server = Server::start();
    let key = test_key();
    let exe = temp.0.join("app/redcrab");
    fake_redcrab(&exe, "0.1.0");
    let (name, package) = tarball(&temp, "9.0.0", "9.0.0");
    let release = publish(&server, "9.0.0", &name, &package, &key);

    let (reporter, updates) = progress::channel();
    updates.stopper().stop();
    let result = update(
        &release,
        &Install::Portable { exe: exe.clone() },
        &public(&key),
        &reporter,
    );
    assert!(result.unwrap().is_none());
    assert_eq!(say_version(&exe), "redcrab 0.1.0");
}

#[cfg(target_os = "macos")]
#[test]
fn a_mac_app_is_replaced_with_the_one_in_the_disk_image() {
    let temp = Temp::new("mac");
    let server = Server::start();
    let key = test_key();
    let bundle = temp.0.join("Applications/redcrab.app");
    fake_redcrab(&bundle.join("Contents/MacOS/redcrab"), "0.1.0");
    let install = Install::MacApp {
        bundle: bundle.clone(),
    };

    let dmg = |says: &str| {
        let src = temp.0.join(format!("dmg-{says}"));
        fake_redcrab(&src.join("redcrab.app/Contents/MacOS/redcrab"), says);
        std::os::unix::fs::symlink("/Applications", src.join("Applications")).unwrap();
        let out = temp.0.join(format!("{says}.dmg"));
        let status = std::process::Command::new("/usr/bin/hdiutil")
            .args([
                "create",
                "-quiet",
                "-volname",
                "redcrab",
                "-format",
                "UDZO",
                "-srcfolder",
            ])
            .arg(&src)
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        fs::read(out).unwrap()
    };
    let name = "redcrab-9.0.0-macos-universal.dmg";

    // The program in it is another version: nothing changes.
    let release = publish(&server, "9.0.0", name, &dmg("8.0.0"), &key);
    let error = run_update(&release, &install, &public(&key)).unwrap_err();
    assert!(format!("{error:#}").contains("is not redcrab 9.0.0"));
    assert_eq!(
        say_version(&bundle.join("Contents/MacOS/redcrab")),
        "redcrab 0.1.0"
    );

    let release = publish(&server, "9.0.0", name, &dmg("9.0.0"), &key);
    let installed = run_update(&release, &install, &public(&key))
        .unwrap()
        .unwrap();
    assert_eq!(installed.version(), &version("9.0.0"));
    assert_eq!(
        say_version(&bundle.join("Contents/MacOS/redcrab")),
        "redcrab 9.0.0"
    );
    let left: Vec<_> = fs::read_dir(temp.0.join("Applications"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left, ["redcrab.app"]);
}

#[cfg(windows)]
#[test]
fn a_zip_unpacks_with_the_tar_windows_ships() {
    let temp = Temp::new("zip");
    let top = temp.0.join("src/redcrab-9.0.0-windows-x86_64");
    fs::create_dir_all(&top).unwrap();
    fs::write(top.join("redcrab.exe"), "exe").unwrap();
    let zip = temp.0.join("redcrab.zip");
    let root = std::env::var_os("SystemRoot").unwrap();
    let status = std::process::Command::new(Path::new(&root).join(r"System32\tar.exe"))
        .arg("-a")
        .arg("-cf")
        .arg(&zip)
        .arg("-C")
        .arg(temp.0.join("src"))
        .arg("redcrab-9.0.0-windows-x86_64")
        .status()
        .unwrap();
    assert!(status.success());
    let into = temp.0.join("unpacked");
    fs::create_dir(&into).unwrap();
    install::unpack(&zip, &into).unwrap();
    assert_eq!(
        fs::read(into.join("redcrab-9.0.0-windows-x86_64/redcrab.exe")).unwrap(),
        b"exe"
    );
}
