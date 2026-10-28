//! The console's command history: one per connection, kept in a file in
//! the config directory, browsed with Up / Down like a shell's.
//!
//! Lines that carry secrets (see [`super::is_sensitive`]) are never added,
//! and are dropped again when a file is read, in case one got in some other
//! way (an older version, a hand edit).

use std::{
    collections::VecDeque,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{OnceLock, mpsc},
};

use anyhow::{Context as _, Result};

use super::secrets::is_sensitive_line;

/// How many commands a history keeps; older ones are dropped.
pub const HISTORY_LIMIT: usize = 1000;

/// The commands run in a console, oldest first, and where Up / Down are in
/// them.
///
/// Browsing works like a shell's: [`History::older`] remembers the line being
/// typed (the draft) and walks back through older commands; [`History::newer`]
/// walks forward and gives the draft back after the newest one. Adding a
/// command ends browsing.
#[derive(Clone, Debug)]
pub struct History {
    entries: VecDeque<String>,
    limit: usize,
    /// The entry Up / Down last showed, while browsing.
    browsing: Option<usize>,
    /// What was typed before browsing started.
    draft: String,
}

impl Default for History {
    fn default() -> Self {
        Self::with_limit(HISTORY_LIMIT)
    }
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// A history that keeps at most `limit` (at least one) commands.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            limit: limit.max(1),
            browsing: None,
            draft: String::new(),
        }
    }

    /// Adds `entries` (oldest first) as [`History::add`] would, one by one.
    pub fn from_entries<S: AsRef<str>>(entries: impl IntoIterator<Item = S>) -> Self {
        let mut history = Self::new();
        history.extend(entries);
        history
    }

    fn extend<S: AsRef<str>>(&mut self, entries: impl IntoIterator<Item = S>) {
        for entry in entries {
            self.add(entry.as_ref());
        }
    }

    /// Records a command that was run. Returns whether it was kept: blank
    /// lines, lines that carry a secret, and the same line as the newest one
    /// are not. Leading and trailing spaces are dropped. Ends browsing.
    pub fn add(&mut self, line: &str) -> bool {
        self.browsing = None;
        let line = line.trim();
        if line.is_empty()
            || self.entries.back().is_some_and(|last| last == line)
            || is_sensitive_line(line)
        {
            return false;
        }
        if self.entries.len() >= self.limit {
            self.entries.pop_front();
        }
        self.entries.push_back(line.to_string());
        true
    }

    /// Puts `older` (oldest first, as read from a file) before the commands
    /// already here, under the same rules as [`History::add`]. Commands run
    /// while the file was still being read stay the newest. Ends browsing.
    pub fn merge_older<S: AsRef<str>>(&mut self, older: impl IntoIterator<Item = S>) {
        let newer = std::mem::take(&mut self.entries);
        self.browsing = None;
        self.extend(older);
        self.extend(newer);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The commands, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator {
        self.entries.iter().map(String::as_str)
    }

    /// Steps back to the previous (older) command. `current` is the line as
    /// it is now; when browsing starts it is kept as the draft that
    /// [`History::newer`] eventually gives back. `None` when there is nothing
    /// older: the line stays as it is.
    pub fn older(&mut self, current: &str) -> Option<&str> {
        let ix = match self.browsing {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_string();
                self.entries.len()
            }
            Some(ix) => ix,
        };
        let prev = ix.checked_sub(1)?;
        self.browsing = Some(prev);
        Some(&self.entries[prev])
    }

    /// Steps forward to the next (newer) command, or, past the newest one,
    /// back to the draft, which ends browsing. `None` when not browsing.
    pub fn newer(&mut self) -> Option<&str> {
        let ix = self.browsing? + 1;
        if ix < self.entries.len() {
            self.browsing = Some(ix);
            Some(&self.entries[ix])
        } else {
            self.browsing = None;
            Some(&self.draft)
        }
    }

    /// Whether Up / Down are showing older commands.
    pub fn is_browsing(&self) -> bool {
        self.browsing.is_some()
    }

    /// Stops browsing, keeping the line as it is.
    pub fn stop_browsing(&mut self) {
        self.browsing = None;
    }
}

/// Where a console history is kept: one file per connection, one command
/// per line (see [`encode`]).
///
/// Every command run is appended, so consoles of the same connection in
/// several windows share their history, as shells do. Reading the file
/// rewrites it once it holds well over [`HISTORY_LIMIT`] lines, so it never
/// grows without bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryFile {
    path: PathBuf,
}

impl HistoryFile {
    /// The history of the connection named `connection`, in the config
    /// directory (`REDCRAB_CONFIG_DIR` when set, as for saved connections).
    /// `None` when the platform has no config directory.
    pub fn for_connection(connection: &str) -> Option<Self> {
        let dir = crate::store::config_dir()?.join("history");
        Some(Self::at(dir.join(file_name(connection))))
    }

    /// A history kept at `path`.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the history. A missing file is an empty history; lines that
    /// carry a secret are dropped (and the file rewritten without them).
    /// Blocks on file I/O: call it off the UI thread.
    pub fn load(&self) -> Result<History> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(History::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("could not read {}", self.path.display()));
            }
        };
        let lines: Vec<String> = bytes
            .split(|&b| b == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
            .filter(|line| !line.is_empty())
            .map(|line| decode(&String::from_utf8_lossy(line)))
            .collect();
        let secrets = lines.iter().any(|line| is_sensitive_line(line));
        let history = History::from_entries(&lines);
        if secrets || lines.len() > 2 * HISTORY_LIMIT {
            self.rewrite(&history)?;
        }
        Ok(history)
    }

    /// Appends one command. Blocks on file I/O; see
    /// [`HistoryFile::append_in_background`].
    pub fn append(&self, line: &str) -> Result<()> {
        let mut entry = encode(line);
        entry.push('\n');
        self.create_dir()?;
        let mut file = options()
            .append(true)
            .open(&self.path)
            .with_context(|| format!("could not open {}", self.path.display()))?;
        // One write, so that appends from several windows do not interleave.
        file.write_all(entry.as_bytes())
            .with_context(|| format!("could not write {}", self.path.display()))
    }

    /// Appends one command on a background thread, after every command
    /// appended before (to any history file); failures are logged. Never
    /// blocks.
    pub fn append_in_background(&self, line: String) {
        let job = Job::Append(self.clone(), line);
        if let Err(mpsc::SendError(job)) = writer().send(job) {
            // No writer thread: write right here rather than lose it.
            job.run();
        }
    }

    /// Waits until everything [`HistoryFile::append_in_background`] was
    /// given before this call is written.
    pub fn flush_background() {
        let (done, wait) = mpsc::channel();
        if writer().send(Job::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }

    /// Replaces the file with `history`, all at once (write and rename),
    /// so a crash never leaves half a history.
    fn rewrite(&self, history: &History) -> Result<()> {
        self.create_dir()?;
        let mut text = String::new();
        for line in history.iter() {
            text.push_str(&encode(line));
            text.push('\n');
        }
        let tmp = self.path.with_extension("tmp");
        let mut file = options()
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        file.write_all(text.as_bytes())?;
        drop(file);
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))
    }

    fn create_dir(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Opens for writing, creating the file readable by its owner only: what is
/// typed into a console can be private even without passwords.
fn options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

enum Job {
    Append(HistoryFile, String),
    Flush(mpsc::Sender<()>),
}

impl Job {
    fn run(self) {
        match self {
            Job::Append(file, line) => {
                if let Err(err) = file.append(&line) {
                    tracing::warn!("could not save the console history: {err:#}");
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// The one thread that appends to history files, so that appends land in
/// the order they were made.
fn writer() -> &'static mpsc::Sender<Job> {
    static WRITER: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let (jobs, queue) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("redcrab-history".into())
            .spawn(move || {
                for job in queue {
                    job.run();
                }
            });
        if let Err(err) = spawned {
            // The receiver is gone with the closure, so sends fail and
            // callers write themselves.
            tracing::warn!("could not start the history writer: {err}");
        }
        jobs
    })
}

/// A file name for a connection's history that any file system accepts:
/// lower-case letters, digits, `-` and `_` as they are, every other byte as
/// `%XX` (so no name can be `..` or contain a separator). Upper-case letters
/// are encoded too: connection names differ by case (`Prod` and `prod` are
/// two connections), file names often do not (macOS, Windows). Long names
/// are cut and end in `~` and a hash of the whole name, so they stay apart.
/// No two names share a file, whatever case the file system ignores.
fn file_name(connection: &str) -> String {
    const MAX: usize = 96;
    let mut name = String::new();
    for &byte in connection.as_bytes() {
        let plain = byte.is_ascii_lowercase() || byte.is_ascii_digit();
        let piece = if plain || byte == b'-' || byte == b'_' {
            (byte as char).to_string()
        } else {
            format!("%{byte:02X}")
        };
        if name.len() + piece.len() > MAX {
            name.push_str(&format!("~{:016x}", fnv1a(connection.as_bytes())));
            break;
        }
        name.push_str(&piece);
    }
    if name.is_empty() {
        // Not `_`, which is the connection named `_`.
        name.push('~');
    }
    name.push_str(".history");
    name
}

/// FNV-1a: a hash that never changes between versions or runs, so a cut
/// file name always finds its file again.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// One command as one line of the history file: backslashes, line feeds
/// and carriage returns escaped as `\\`, `\n` and `\r`, everything else as
/// it is.
pub(crate) fn encode(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Reads back a line [`encode`] wrote. Any other backslash is kept as it
/// is, so a hand-edited file still loads.
pub(crate) fn decode(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            _ => {
                out.push('\\');
                continue;
            }
        }
        chars.next();
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use proptest::prelude::*;

    use super::*;

    /// A directory of the test's own, removed when it ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "redcrab-history-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn file(&self, name: &str) -> HistoryFile {
            HistoryFile::at(self.0.join(name))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entries(history: &History) -> Vec<&str> {
        history.iter().collect()
    }

    #[test]
    fn keeps_what_was_run() {
        let mut history = History::new();
        assert!(history.add("GET a"));
        assert!(history.add("  SET a 1  "), "trimmed");
        assert!(!history.add("SET a 1"), "same as the newest");
        assert!(!history.add("   "));
        assert!(!history.add(""));
        assert!(history.add("GET a"), "only consecutive duplicates collapse");
        assert!(!history.add("AUTH s3cret"));
        assert!(!history.add("CONFIG SET requirepass s3cret"));
        assert_eq!(entries(&history), ["GET a", "SET a 1", "GET a"]);
    }

    #[test]
    fn drops_the_oldest_over_the_limit() {
        let mut history = History::with_limit(3);
        for i in 0..5 {
            history.add(&format!("GET {i}"));
        }
        assert_eq!(entries(&history), ["GET 2", "GET 3", "GET 4"]);

        let mut history = History::new();
        for i in 0..HISTORY_LIMIT + 10 {
            history.add(&format!("GET {i}"));
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.iter().next(), Some("GET 10"));
        assert_eq!(history.iter().next_back(), Some("GET 1009"));
        assert_eq!(History::with_limit(0).limit, 1);
    }

    #[test]
    fn browses_like_a_shell() {
        let mut history = History::from_entries(["GET a", "GET b", "GET c"]);
        assert_eq!(history.newer(), None, "not browsing yet");
        assert_eq!(history.older("draft"), Some("GET c"));
        assert!(history.is_browsing());
        assert_eq!(history.older("GET c"), Some("GET b"));
        assert_eq!(history.older("GET b"), Some("GET a"));
        assert_eq!(history.older("GET a"), None, "nothing older");
        assert_eq!(history.newer(), Some("GET b"));
        assert_eq!(history.newer(), Some("GET c"));
        assert_eq!(history.newer(), Some("draft"), "the draft comes back");
        assert!(!history.is_browsing());
        assert_eq!(history.newer(), None);

        // An edited entry does not become the draft: browsing goes on from
        // where it was, and the draft is still the line typed first.
        assert_eq!(history.older("typed"), Some("GET c"));
        assert_eq!(history.older("GET c edited"), Some("GET b"));
        assert_eq!(history.newer(), Some("GET c"));
        assert_eq!(history.newer(), Some("typed"));

        // Running a command ends browsing; the next Up starts at the newest.
        assert_eq!(history.older(""), Some("GET c"));
        assert_eq!(history.older(""), Some("GET b"));
        history.add("GET b");
        assert!(!history.is_browsing());
        assert_eq!(history.older(""), Some("GET b"));

        history.stop_browsing();
        assert_eq!(history.newer(), None);

        let mut empty = History::new();
        assert_eq!(empty.older("x"), None);
        assert!(!empty.is_browsing());
        assert_eq!(empty.newer(), None);
    }

    #[test]
    fn merges_a_file_read_late() {
        let mut history = History::new();
        history.add("GET new");
        history.older("");
        history.merge_older(["GET old", "AUTH s3cret", "GET old", "GET new"]);
        assert!(!history.is_browsing());
        // The file may already hold the command run meanwhile.
        assert_eq!(entries(&history), ["GET old", "GET new"]);
    }

    #[test]
    fn persists_per_connection() {
        let dir = TempDir::new("persist");
        let file = dir.file("nested/conn.history");
        assert_eq!(file.load().unwrap().len(), 0, "a missing file is empty");

        let mut history = History::new();
        for line in [
            "SET greeting \"hello\nworld\"",
            r#"GET "back\slash""#,
            "SET ü 🦀",
            "SET k \"a\\r\\nb\"",
        ] {
            assert!(history.add(line));
            file.append(line).unwrap();
        }
        let loaded = file.load().unwrap();
        assert_eq!(entries(&loaded), entries(&history));
        let text = fs::read_to_string(file.path()).unwrap();
        assert_eq!(text.lines().count(), 4, "one line per command: {text:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(file.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn connections_differing_in_case_keep_their_own_files() {
        // On a file system that ignores case (macOS and Windows by default)
        // `Prod.history` would be `prod.history`.
        let dir = TempDir::new("case");
        let upper = dir.file(&file_name("Prod"));
        let lower = dir.file(&file_name("prod"));
        upper.append("GET prod-key").unwrap();
        lower.append("FLUSHALL").unwrap();
        assert_eq!(entries(&upper.load().unwrap()), ["GET prod-key"]);
        assert_eq!(entries(&lower.load().unwrap()), ["FLUSHALL"]);
    }

    #[test]
    fn background_appends_keep_their_order() {
        let dir = TempDir::new("background");
        let a = dir.file("a.history");
        let b = dir.file("b.history");
        for i in 0..200 {
            a.append_in_background(format!("GET a{i}"));
            b.append_in_background(format!("GET b{i}"));
        }
        HistoryFile::flush_background();
        let a = a.load().unwrap();
        assert_eq!(a.len(), 200);
        assert!(
            a.iter()
                .enumerate()
                .all(|(i, line)| line == format!("GET a{i}"))
        );
        assert_eq!(b.load().unwrap().iter().next_back(), Some("GET b199"));
    }

    #[test]
    fn loading_drops_secrets_and_compacts() {
        let dir = TempDir::new("compact");
        let file = dir.file("c.history");
        fs::create_dir_all(&dir.0).unwrap();
        // Written by hand or an older version: a secret, duplicates, CRLF
        // line ends, blank lines, bytes that are not UTF-8.
        fs::write(
            file.path(),
            b"GET a\r\nAUTH s3cret\nGET a\n\nGET \xff\nCONFIG SET masterauth x\n",
        )
        .unwrap();
        let loaded = file.load().unwrap();
        assert_eq!(entries(&loaded), ["GET a", "GET \u{fffd}"]);
        let text = fs::read_to_string(file.path()).unwrap();
        assert!(
            !text.contains("s3cret") && !text.contains("masterauth"),
            "{text}"
        );

        // A file far over the limit is cut back when it is read.
        let many: String = (0..2 * HISTORY_LIMIT + 5)
            .map(|i| format!("GET {i}\n"))
            .collect();
        fs::write(file.path(), many).unwrap();
        let loaded = file.load().unwrap();
        assert_eq!(loaded.len(), HISTORY_LIMIT);
        assert_eq!(
            fs::read_to_string(file.path()).unwrap().lines().count(),
            HISTORY_LIMIT
        );
        assert_eq!(loaded.iter().next_back(), Some("GET 2004"));
        assert!(!file.path().with_extension("tmp").exists());

        // Up to twice the limit it is left alone (appends stay cheap).
        let some: String = (0..HISTORY_LIMIT + 5)
            .map(|i| format!("GET {i}\n"))
            .collect();
        fs::write(file.path(), &some).unwrap();
        assert_eq!(file.load().unwrap().len(), HISTORY_LIMIT);
        assert_eq!(fs::read_to_string(file.path()).unwrap(), some);
    }

    #[test]
    fn unreadable_files_are_errors() {
        let dir = TempDir::new("unreadable");
        fs::create_dir_all(dir.0.join("dir.history")).unwrap();
        assert!(dir.file("dir.history").load().is_err());
        assert!(dir.file("dir.history").append("GET a").is_err());
    }

    #[test]
    fn names_files_safely() {
        assert_eq!(file_name("localhost"), "localhost.history");
        assert_eq!(
            file_name("127.0.0.1:6379"),
            "127%2E0%2E0%2E1%3A6379.history"
        );
        assert_eq!(file_name(".."), "%2E%2E.history");
        assert_eq!(file_name("a/b\\c"), "a%2Fb%5Cc.history");
        assert_eq!(file_name(""), "~.history");
        assert_eq!(file_name("_"), "_.history");
        assert_eq!(file_name("prod ü"), "prod%20%C3%BC.history");
        // Names that differ in case are two connections, and two files on a
        // file system that ignores case.
        assert_eq!(file_name("Prod"), "%50rod.history");
        assert!(!file_name("Prod").eq_ignore_ascii_case(&file_name("prod")));
        assert!(!file_name("PROD").eq_ignore_ascii_case(&file_name("Prod")));
        let long = "x".repeat(300);
        let cut = file_name(&long);
        assert!(cut.len() < 128, "{cut}");
        assert_ne!(cut, file_name(&"x".repeat(301)), "cut names stay apart");
        assert_eq!(cut, file_name(&long), "and are stable");
        assert_eq!(fnv1a(b"redcrab"), 0x7a2a_858e_5425_7352);
    }

    #[test]
    fn for_connection_honors_the_config_dir() {
        // `REDCRAB_CONFIG_DIR` is set for every test and smoke run; only
        // check the layout under whatever it is.
        if let Some(file) = HistoryFile::for_connection("prod:6379") {
            assert!(file.path().ends_with("history/prod%3A6379.history"));
        }
    }

    /// `name`, and `name` with the case of some ASCII letters swapped.
    fn and_other_case(name: String) -> impl Strategy<Value = (String, String)> {
        let len = name.chars().count();
        proptest::collection::vec(any::<bool>(), len).prop_map(move |flip| {
            let other = name
                .chars()
                .zip(flip)
                .map(|(c, flip)| match (flip, c.is_ascii_lowercase()) {
                    (false, _) => c,
                    (true, true) => c.to_ascii_uppercase(),
                    (true, false) => c.to_ascii_lowercase(),
                })
                .collect();
            (name.clone(), other)
        })
    }

    proptest! {
        /// No two connection names share a file, even on a file system that
        /// ignores case, and every name is one any file system takes.
        #[test]
        fn file_names_stay_apart(
            (a, b) in "[A-Za-z0-9 ._:/%~-]{0,12}|\\PC{0,40}".prop_flat_map(and_other_case),
            c in "[A-Za-z0-9 ._:/%~-]{0,12}|\\PC{0,40}",
        ) {
            for (x, y) in [(&a, &b), (&a, &c), (&b, &c)] {
                let same = file_name(x).eq_ignore_ascii_case(&file_name(y));
                prop_assert_eq!(same, x == y, "{:?} and {:?}", x, y);
            }
            let name = file_name(&a);
            let stem = name.strip_suffix(".history").unwrap();
            prop_assert!(!stem.is_empty() && stem.len() <= 96 + 17, "{}", name);
            let safe = |b: u8| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_%~ABCDEF".contains(&b)
            };
            prop_assert!(stem.bytes().all(safe), "{}", name);
        }

        #[test]
        fn lines_survive_encoding(line in "\\PC*|[\\\\\n\rnr]{0,12}") {
            let encoded = encode(&line);
            prop_assert!(!encoded.contains('\n') && !encoded.contains('\r'));
            prop_assert_eq!(decode(&encoded), line);
        }

        #[test]
        fn decoding_never_fails(line in "[\\\\nrx a]{0,16}") {
            // Anything decodes; what encode wrote decodes exactly.
            let _ = decode(&line);
        }

        #[test]
        fn any_command_round_trips_through_a_file(lines in proptest::collection::vec("[ -~\n\\\\ü]{1,20}", 1..20)) {
            let dir = TempDir::new("prop");
            let file = dir.file("p.history");
            let mut history = History::new();
            for line in &lines {
                if history.add(line) {
                    file.append(line.trim()).unwrap();
                }
            }
            let loaded = file.load().unwrap();
            prop_assert_eq!(entries(&loaded), entries(&history));
        }

        /// Whatever happens, the history never holds a secret, two equal
        /// commands in a row, or more than the limit; and browsing all the
        /// way back and forth returns the draft.
        #[test]
        fn invariants_hold(ops in proptest::collection::vec(prop_oneof![
            "(GET|SET|AUTH|HGET) [a-c]{1,2}".prop_map(Op::Add),
            Just(Op::Prev),
            Just(Op::Next),
        ], 0..60)) {
            let mut history = History::with_limit(5);
            for op in ops {
                match op {
                    Op::Add(line) => { history.add(&line); }
                    Op::Prev => { history.older("draft"); }
                    Op::Next => { history.newer(); }
                }
                prop_assert!(history.len() <= 5);
                prop_assert!(history.iter().all(|line| !is_sensitive_line(line)));
                let lines: Vec<&str> = history.iter().collect();
                prop_assert!(lines.windows(2).all(|w| w[0] != w[1]));
            }
            history.stop_browsing();
            let len = history.len();
            for _ in 0..len {
                prop_assert!(history.older("mine").is_some());
            }
            prop_assert_eq!(history.older("mine"), None);
            for _ in 1..len {
                prop_assert!(history.newer().is_some());
            }
            if len > 0 {
                prop_assert_eq!(history.newer(), Some("mine"));
            }
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Add(String),
        Prev,
        Next,
    }
}
