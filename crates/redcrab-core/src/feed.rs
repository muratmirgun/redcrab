//! What arrives on a connection that only listens: Pub/Sub messages and
//! MONITOR's command stream. Both can come faster than anyone reads, so a
//! [`Feed`] keeps the newest [`FEED_LEN`] and counts the rest.

use std::{collections::VecDeque, time::SystemTime};

use crate::format;

/// Entries a feed keeps.
pub const FEED_LEN: usize = 10_000;

/// The newest entries that arrived, oldest first.
#[derive(Clone, Debug)]
pub struct Feed<T> {
    entries: VecDeque<T>,
    /// Every entry that arrived, the ones no longer kept included.
    received: u64,
}

impl<T> Default for Feed<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            received: 0,
        }
    }
}

impl<T> Feed<T> {
    pub fn push(&mut self, entry: T) {
        if self.entries.len() == FEED_LEN {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.received += 1;
    }

    pub fn entries(&self) -> &VecDeque<T> {
        &self.entries
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    /// Entries that arrived but are no longer kept.
    pub fn dropped(&self) -> u64 {
        self.received - self.entries.len() as u64
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.received = 0;
    }
}

/// How a listening connection goes; what it heard is in its feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listening {
    /// Entries received so far.
    Receiving(u64),
    /// The connection ended: the server closed it, or it broke.
    Ended(String),
}

/// One Pub/Sub message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub at: SystemTime,
    pub channel: Vec<u8>,
    /// The pattern it matched, for a PSUBSCRIBE.
    pub pattern: Option<Vec<u8>>,
    pub payload: Vec<u8>,
}

impl Message {
    /// Whether `filter` (lowercase) is in its channel or its payload.
    pub fn matches(&self, filter: &str) -> bool {
        filter.is_empty()
            || String::from_utf8_lossy(&self.channel)
                .to_lowercase()
                .contains(filter)
            || String::from_utf8_lossy(&self.payload)
                .to_lowercase()
                .contains(filter)
    }
}

/// A channel name for SUBSCRIBE, or a pattern for PSUBSCRIBE: one with the
/// glob characters `*`, `?` or `[` is a pattern.
pub fn is_pattern(channel: &[u8]) -> bool {
    channel.iter().any(|b| matches!(b, b'*' | b'?' | b'['))
}

/// One command MONITOR reported.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitored {
    /// When the server ran it, in seconds since the Unix epoch.
    pub at: f64,
    pub db: u32,
    /// `ip:port`, `lua` for a script's command, or `unix:...`.
    pub client: String,
    pub args: Vec<Vec<u8>>,
}

impl Monitored {
    /// Reads one line of MONITOR:
    /// `1339518083.107412 [0 127.0.0.1:60866] "keys" "*"`.
    pub fn parse(line: &str) -> Option<Self> {
        let (at, rest) = line.split_once(' ')?;
        let at = at.parse().ok()?;
        let rest = rest.strip_prefix('[')?;
        let (source, command) = rest.split_once("] ")?;
        let (db, client) = source.split_once(' ')?;
        Some(Self {
            at,
            db: db.parse().ok()?,
            client: client.to_owned(),
            args: quoted_args(command)?,
        })
    }

    /// The command as it would be typed.
    pub fn command(&self) -> String {
        let mut words = self.args.iter();
        let mut line = words
            .next()
            .map(|name| String::from_utf8_lossy(name).to_uppercase())
            .unwrap_or_default();
        for arg in words {
            line.push(' ');
            line.push_str(&format::quote_arg(arg));
        }
        line
    }

    pub fn matches(&self, filter: &str) -> bool {
        filter.is_empty()
            || self.client.to_lowercase().contains(filter)
            || self.command().to_lowercase().contains(filter)
    }
}

/// MONITOR's arguments: double-quoted, with `\"`, `\\`, `\n`, `\r`, `\t`,
/// `\a`, `\b` and `\xHH` escapes, as `sdscatrepr` writes them.
fn quoted_args(text: &str) -> Option<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    let mut bytes = text.bytes().peekable();
    loop {
        while bytes.next_if_eq(&b' ').is_some() {}
        match bytes.next() {
            None => return Some(args),
            Some(b'"') => {}
            Some(_) => return None,
        }
        let mut arg = Vec::new();
        loop {
            match bytes.next()? {
                b'"' => break,
                b'\\' => match bytes.next()? {
                    b'n' => arg.push(b'\n'),
                    b'r' => arg.push(b'\r'),
                    b't' => arg.push(b'\t'),
                    b'a' => arg.push(0x07),
                    b'b' => arg.push(0x08),
                    b'x' => {
                        let hex = [bytes.next()?, bytes.next()?];
                        let hex = std::str::from_utf8(&hex).ok()?;
                        arg.push(u8::from_str_radix(hex, 16).ok()?);
                    }
                    other => arg.push(other),
                },
                other => arg.push(other),
            }
        }
        args.push(arg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_keeps_the_newest() {
        let mut feed = Feed::default();
        for n in 0..(FEED_LEN + 5) {
            feed.push(n);
        }
        assert_eq!(feed.entries().len(), FEED_LEN);
        assert_eq!(feed.entries().front(), Some(&5));
        assert_eq!(
            (feed.received(), feed.dropped()),
            ((FEED_LEN + 5) as u64, 5)
        );
        feed.clear();
        assert_eq!((feed.received(), feed.dropped()), (0, 0));
    }

    #[test]
    fn monitor_lines_are_read() {
        let line = Monitored::parse(
            r#"1339518083.107412 [0 127.0.0.1:60866] "set" "user:1" "a \"b\"\n\xff""#,
        )
        .unwrap();
        assert_eq!(line.db, 0);
        assert_eq!(line.client, "127.0.0.1:60866");
        assert_eq!(
            line.args,
            [
                b"set".to_vec(),
                b"user:1".to_vec(),
                b"a \"b\"\n\xff".to_vec()
            ]
        );
        assert_eq!(line.command(), r#"SET user:1 "a \"b\"\n\xff""#);
        assert!((line.at - 1_339_518_083.107_412).abs() < 1e-6);
        let script = Monitored::parse(r#"1.5 [3 lua] "get" "k""#).unwrap();
        assert_eq!((script.db, script.client.as_str()), (3, "lua"));
        assert!(script.matches("lua"));
        assert!(script.matches("get k"));
        // Not MONITOR's shape: the OK it answers first, a cut-off line.
        assert_eq!(Monitored::parse("OK"), None);
        assert_eq!(Monitored::parse(r#"1.5 [0 lua] "get"#), None);
    }

    #[test]
    fn globs_make_patterns() {
        assert!(is_pattern(b"news.*"));
        assert!(is_pattern(b"h?llo"));
        assert!(is_pattern(b"[ab]"));
        assert!(!is_pattern(b"news.sports"));
    }
}
