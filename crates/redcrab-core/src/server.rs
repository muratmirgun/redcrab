//! What the server tools show about a server, read from INFO, SLOWLOG and
//! CLIENT LIST, with nothing about how a front end draws it. The requests
//! are [`ServerNode`](crate::ServerNode)'s.

use std::{collections::VecDeque, time::Duration};

use crate::format;

/// A server's `INFO`: its sections, in the order it wrote them, each with its
/// `field:value` lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    sections: Vec<Section>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    /// "Server", "Clients", "Memory", ...
    pub name: String,
    pub fields: Vec<(String, String)>,
}

impl Info {
    /// Reads INFO's text: `# Section` headings, `field:value` lines.
    pub fn parse(text: &str) -> Self {
        let mut sections: Vec<Section> = Vec::new();
        for line in text.lines() {
            let line = line.trim_end();
            if let Some(name) = line.strip_prefix('#') {
                sections.push(Section {
                    name: name.trim().to_owned(),
                    fields: Vec::new(),
                });
            } else if let Some((field, value)) = line.split_once(':') {
                if sections.is_empty() {
                    sections.push(Section::default());
                }
                let section = sections.last_mut().expect("pushed above");
                section.fields.push((field.to_owned(), value.to_owned()));
            }
        }
        Self { sections }
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// The value of `field`, in whichever section.
    pub fn get(&self, field: &str) -> Option<&str> {
        self.sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.as_str())
    }

    pub fn number(&self, field: &str) -> Option<f64> {
        self.get(field)?.trim().parse().ok()
    }

    fn count(&self, field: &str) -> Option<u64> {
        self.get(field)?.trim().parse().ok()
    }

    /// The `Keyspace` section: `db0:keys=12,expires=3,avg_ttl=0`.
    pub fn keyspace(&self) -> Vec<KeyspaceDb> {
        let Some(section) = self.sections.iter().find(|s| s.name == "Keyspace") else {
            return Vec::new();
        };
        section
            .fields
            .iter()
            .filter_map(|(name, value)| {
                let db = name.strip_prefix("db")?.parse().ok()?;
                let mut keys = 0;
                let mut expires = 0;
                for pair in value.split(',') {
                    match pair.split_once('=') {
                        Some(("keys", n)) => keys = n.parse().ok()?,
                        Some(("expires", n)) => expires = n.parse().ok()?,
                        _ => {}
                    }
                }
                Some(KeyspaceDb { db, keys, expires })
            })
            .collect()
    }
}

/// One database of the `Keyspace` section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyspaceDb {
    pub db: u32,
    pub keys: u64,
    /// Keys with a TTL.
    pub expires: u64,
}

/// The figures the overview shows, from one INFO and, for what INFO only
/// counts since the start (hits and misses), the one before it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overview {
    pub version: Option<String>,
    pub mode: Option<String>,
    /// "master", "slave" (INFO's own words).
    pub role: Option<String>,
    pub uptime: Option<Duration>,
    pub clients: Option<u64>,
    pub blocked_clients: Option<u64>,
    pub used_memory: Option<u64>,
    pub peak_memory: Option<u64>,
    pub rss_memory: Option<u64>,
    /// `None` when there is no limit (`maxmemory 0`).
    pub max_memory: Option<u64>,
    pub eviction_policy: Option<String>,
    pub fragmentation: Option<f64>,
    pub ops_per_sec: Option<f64>,
    pub input_kbps: Option<f64>,
    pub output_kbps: Option<f64>,
    /// Hits among lookups since the INFO before; `None` without lookups in
    /// between, or without an INFO before.
    pub hit_rate: Option<f64>,
    /// Hits among lookups since the server started.
    pub hit_rate_total: Option<f64>,
    pub evicted_keys: Option<u64>,
    pub expired_keys: Option<u64>,
    pub keys: u64,
    pub keys_with_ttl: u64,
    /// When the last RDB save ended, in seconds since the Unix epoch.
    pub last_save: Option<u64>,
    pub changes_since_save: Option<u64>,
    pub aof: Option<bool>,
    pub replicas: Option<u64>,
    /// A replica's link to its primary: "up" or "down".
    pub primary_link: Option<String>,
}

impl Overview {
    pub fn from_info(info: &Info, before: Option<&Info>) -> Self {
        let text = |field: &str| info.get(field).map(str::to_owned);
        let hits =
            |info: &Info| Some((info.count("keyspace_hits")?, info.count("keyspace_misses")?));
        let rate = |hits: u64, misses: u64| {
            let lookups = hits + misses;
            (lookups > 0).then(|| hits as f64 / lookups as f64)
        };
        let hit_rate = match (hits(info), before.and_then(hits)) {
            (Some((hits, misses)), Some((hits_before, misses_before)))
                if hits >= hits_before && misses >= misses_before =>
            {
                rate(hits - hits_before, misses - misses_before)
            }
            _ => None,
        };
        let keyspace = info.keyspace();
        Self {
            version: text("redis_version"),
            mode: text("redis_mode"),
            role: text("role"),
            uptime: info.count("uptime_in_seconds").map(Duration::from_secs),
            clients: info.count("connected_clients"),
            blocked_clients: info.count("blocked_clients"),
            used_memory: info.count("used_memory"),
            peak_memory: info.count("used_memory_peak"),
            rss_memory: info.count("used_memory_rss"),
            max_memory: info.count("maxmemory").filter(|&n| n > 0),
            eviction_policy: text("maxmemory_policy"),
            fragmentation: info.number("mem_fragmentation_ratio"),
            ops_per_sec: info.number("instantaneous_ops_per_sec"),
            input_kbps: info.number("instantaneous_input_kbps"),
            output_kbps: info.number("instantaneous_output_kbps"),
            hit_rate,
            hit_rate_total: hits(info).and_then(|(hits, misses)| rate(hits, misses)),
            evicted_keys: info.count("evicted_keys"),
            expired_keys: info.count("expired_keys"),
            keys: keyspace.iter().map(|db| db.keys).sum(),
            keys_with_ttl: keyspace.iter().map(|db| db.expires).sum(),
            last_save: info.count("rdb_last_save_time"),
            changes_since_save: info.count("rdb_changes_since_last_save"),
            aof: info.count("aof_enabled").map(|n| n == 1),
            replicas: info.count("connected_slaves"),
            primary_link: text("master_link_status"),
        }
    }
}

/// "3d 4h", "5h 12m", "42s": an uptime, to the two largest units.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (days, hours, minutes) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m {}s", secs % 60),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// How many samples [`Trend`] keeps: two minutes at one every two seconds.
pub const TREND_LEN: usize = 60;

/// The last [`TREND_LEN`] values of a figure, oldest first, for a small
/// chart. A sample the server did not give is left out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Trend(VecDeque<f64>);

impl Trend {
    pub fn push(&mut self, value: Option<f64>) {
        let Some(value) = value.filter(|v| v.is_finite()) else {
            return;
        };
        if self.0.len() == TREND_LEN {
            self.0.pop_front();
        }
        self.0.push_back(value);
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = f64> + '_ {
        self.0.iter().copied()
    }

    pub fn max(&self) -> f64 {
        self.0.iter().copied().fold(0.0, f64::max)
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// The overview of one server as it changes: its latest INFO, the figures
/// from it, and the trends the overview charts.
#[derive(Clone, Debug, Default)]
pub struct Monitor {
    info: Option<Info>,
    overview: Overview,
    pub ops: Trend,
    pub memory: Trend,
    pub clients: Trend,
    pub hit_rate: Trend,
}

impl Monitor {
    /// Takes a new INFO of the same server.
    pub fn record(&mut self, info: Info) {
        self.overview = Overview::from_info(&info, self.info.as_ref());
        self.ops.push(self.overview.ops_per_sec);
        self.memory
            .push(self.overview.used_memory.map(|n| n as f64));
        self.clients.push(self.overview.clients.map(|n| n as f64));
        self.hit_rate.push(self.overview.hit_rate);
        self.info = Some(info);
    }

    /// Forgets everything, for another server.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn info(&self) -> Option<&Info> {
        self.info.as_ref()
    }

    pub fn overview(&self) -> &Overview {
        &self.overview
    }
}

/// The slow log of one server, newest first, and how it is set up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlowLog {
    pub entries: Vec<SlowEntry>,
    /// How many entries the server keeps now (SLOWLOG LEN).
    pub len: Option<u64>,
    /// Commands slower than this many microseconds are logged; negative
    /// turns the log off (`slowlog-log-slower-than`). `None` when CONFIG
    /// GET is not allowed, as on many managed services.
    pub slower_than_micros: Option<i64>,
    /// How many entries the server keeps at most (`slowlog-max-len`).
    pub max_len: Option<u64>,
}

impl SlowLog {
    /// "Commands slower than 10 ms are logged; the last 128 are kept."
    pub fn settings(&self) -> Option<String> {
        let threshold = self.slower_than_micros?;
        let logged = match threshold {
            t if t < 0 => "The slow log is off".to_owned(),
            0 => "Every command is logged".to_owned(),
            t => format!(
                "Commands slower than {} are logged",
                micros(Duration::from_micros(t as u64))
            ),
        };
        Some(match self.max_len {
            Some(max) => format!("{logged}; the last {} are kept.", format::count(max)),
            None => format!("{logged}."),
        })
    }
}

/// One slow command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlowEntry {
    pub id: u64,
    /// When it ran, in seconds since the Unix epoch.
    pub at: u64,
    pub duration: Duration,
    /// The command as logged: long ones are cut, the last argument then
    /// says how many more there were.
    pub args: Vec<Vec<u8>>,
    /// `ip:port` of the client (Redis 4 and later).
    pub client: Option<String>,
    pub client_name: Option<String>,
}

impl SlowEntry {
    /// The command as it would be typed: `HGETALL "user:1"`.
    pub fn command(&self) -> String {
        let mut words = self.args.iter();
        let mut line = words
            .next()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .unwrap_or_default();
        for arg in words {
            line.push(' ');
            line.push_str(&format::quote_arg(arg));
        }
        line
    }
}

/// "850 µs", "12.4 ms", "2.31 s".
pub fn micros(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1000 {
        format!("{us} µs")
    } else if us < 1_000_000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else {
        format!("{:.2} s", us as f64 / 1_000_000.0)
    }
}

/// The clients connected to one server (CLIENT LIST).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientList {
    pub clients: Vec<ClientEntry>,
    /// The id of the connection that asked: redcrab's own.
    pub own_id: Option<u64>,
}

impl ClientList {
    /// Reads CLIENT LIST: one client a line, `field=value` pairs.
    pub fn parse(text: &str, own_id: Option<u64>) -> Self {
        Self {
            clients: text.lines().filter_map(ClientEntry::parse).collect(),
            own_id,
        }
    }
}

/// One connected client. The fields every Redis since 2.8 writes have
/// their own; [`ClientEntry::fields`] has them all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientEntry {
    pub id: u64,
    pub addr: String,
    pub name: String,
    pub age: Duration,
    pub idle: Duration,
    pub db: u32,
    /// The last command; `client|list` for a subcommand.
    pub cmd: String,
    pub flags: String,
    /// The ACL user (Redis 6 and later).
    pub user: Option<String>,
    /// Memory the client takes, buffers included (`tot-mem`, Redis 6+).
    pub memory: Option<u64>,
    pub fields: Vec<(String, String)>,
}

impl ClientEntry {
    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<(String, String)> = line
            .split(' ')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let get = |name: &str| {
            fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let number = |name: &str| get(name).and_then(|v| v.parse::<u64>().ok());
        Some(Self {
            id: number("id")?,
            addr: get("addr").unwrap_or_default().to_owned(),
            name: get("name").unwrap_or_default().to_owned(),
            age: Duration::from_secs(number("age").unwrap_or(0)),
            idle: Duration::from_secs(number("idle").unwrap_or(0)),
            db: number("db")
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            cmd: get("cmd").unwrap_or_default().to_owned(),
            flags: get("flags").unwrap_or_default().to_owned(),
            user: get("user").map(str::to_owned),
            memory: number("tot-mem"),
            fields,
        })
    }

    /// Whether `filter` (lowercase) is in its address, name, user or last
    /// command.
    pub fn matches(&self, filter: &str) -> bool {
        filter.is_empty()
            || [
                self.addr.as_str(),
                self.name.as_str(),
                self.cmd.as_str(),
                self.user.as_deref().unwrap_or(""),
            ]
            .iter()
            .any(|field| field.to_lowercase().contains(filter))
    }

    /// What its flags say, in words: "replica", "pub/sub", "blocked", ...
    pub fn kind(&self) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        for (flag, kind) in [
            ('M', "primary"),
            ('S', "replica"),
            ('P', "pub/sub"),
            ('b', "blocked"),
            ('x', "in MULTI"),
            ('O', "MONITOR"),
        ] {
            if self.flags.contains(flag) {
                kinds.push(kind);
            }
        }
        kinds
    }
}

#[cfg(test)]
mod tests;
