//! What Redis knows about its commands, for the read-only gate, the console
//! and autocomplete.
//!
//! `table.rs` is generated from `COMMAND INFO` and `COMMAND DOCS` of a Redis 8
//! server with its bundled modules (`scripts/gen-commands.sh`). This file holds
//! the types, the lookups and the classification built on top.

use std::{cmp::Ordering, fmt, ops::BitOr};

/// Builds a [`Flags`] set in the generated table: `flags!(WRITE | DENYOOM)`.
macro_rules! flags {
    ($($flag:ident)|*) => { Flags(0 $(| Flags::$flag.0)*) };
}

/// Builds an [`AclCategories`] set in the generated table: `acl!(READ | FAST)`.
macro_rules! acl {
    ($($category:ident)|*) => { AclCategories(0 $(| AclCategories::$category.0)*) };
}

mod table;

/// One command or subcommand, as the server describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// Upper case. A subcommand is named after its container and itself,
    /// separated by a space: `CONFIG SET`.
    pub name: &'static str,
    /// Number of arguments including the name (and the container's name for
    /// a subcommand). Negative means "at least" that many.
    pub arity: i16,
    pub flags: Flags,
    pub acl: AclCategories,
    /// The documentation group (`string`, `server`, ...), or the module for
    /// module commands (`json`, `search`, `bloom`, ...).
    pub group: &'static str,
    /// The version that introduced it (the module's version for module
    /// commands); empty when the server has no docs for it.
    pub since: &'static str,
    /// One line from the server's docs; empty when there are none.
    pub summary: &'static str,
    /// Usage built from the argument docs:
    /// `SET key value [NX | XX] [GET] [EX seconds | ...]`.
    pub syntax: &'static str,
}

/// How a command affects the server, which decides what a read-only
/// connection lets through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandClass {
    /// Reads data or server state.
    Read,
    /// Can modify data. Scripts and functions count, since they can write,
    /// and so does publishing, which makes subscribers act.
    Write,
    /// Administrative or dangerous: configuration, replication, killing
    /// clients, flushing, ... Also the read-only script commands, since
    /// those scripts can still run administrative commands.
    Admin,
    /// Changes or depends on the state of the connection it runs on: SELECT,
    /// MULTI/EXEC, WATCH, subscriptions, AUTH, ...
    ConnectionState,
    /// Not in the table: a command of an unknown module, a newer server, or a
    /// typo.
    Unknown,
}

/// Commands that change or depend on the state of their connection. They
/// are only safe on a connection of their own, like the console's.
const CONNECTION_STATE: &[&str] = &[
    "SELECT",
    "MULTI",
    "EXEC",
    "DISCARD",
    "WATCH",
    "UNWATCH",
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "SSUBSCRIBE",
    "UNSUBSCRIBE",
    "PUNSUBSCRIBE",
    "SUNSUBSCRIBE",
    "MONITOR",
    "RESET",
    "AUTH",
    "HELLO",
    "QUIT",
    "CLIENT REPLY",
    "CLIENT SETNAME",
    "CLIENT SETINFO",
    "CLIENT TRACKING",
    "CLIENT CACHING",
    "CLIENT NO-TOUCH",
    "READONLY",
    "READWRITE",
    "ASKING",
    "SYNC",
    "PSYNC",
    "REPLCONF",
    "SCRIPT DEBUG",
    // Session-local fieldsets that HIMPORT SET refers to.
    "HIMPORT PREPARE",
    "HIMPORT DISCARD",
    "HIMPORT DISCARDALL",
];

/// Administrative or dangerous commands that only read. The app needs them
/// to keep working on a read-only connection.
const READ_INTROSPECTION: &[&str] = &[
    // Dangerous only because they can be slow.
    "KEYS",
    "SORT_RO",
    "FT.TAGVALS",
    "INFO",
    "CONFIG GET",
    "SLOWLOG GET",
    "SLOWLOG LEN",
    "CLIENT LIST",
    "CLIENT INFO",
    "CLIENT GETNAME",
    "CLIENT ID",
    "MEMORY USAGE",
    "MEMORY STATS",
    "MEMORY DOCTOR",
    "LATENCY LATEST",
    "LATENCY HISTORY",
    "LATENCY DOCTOR",
    "LATENCY HISTOGRAM",
    "LATENCY GRAPH",
    "DBSIZE",
    "CLUSTER INFO",
    "CLUSTER NODES",
    "CLUSTER SHARDS",
    "CLUSTER SLOTS",
    "CLUSTER MYID",
    "CLUSTER REPLICAS",
    "CLUSTER SLAVES",
    "ACL WHOAMI",
    "ACL LIST",
    "ACL USERS",
    "ACL CAT",
    "ACL GETUSER",
    "MODULE LIST",
    "TIME",
    "LASTSAVE",
    "ROLE",
    "SCRIPT EXISTS",
    "FUNCTION LIST",
    "FUNCTION STATS",
    "FUNCTION DUMP",
    "FT.CONFIG GET",
    "FT.CONFIG HELP",
];

/// Scripts the server runs in read-only mode. It refuses commands flagged
/// `write` inside them, but not administrative ones (SLOWLOG RESET, MEMORY
/// PURGE, CLUSTER FAILOVER / FORGET / SETSLOT, `_FT.DEBUG`, ...), and what a
/// script will call cannot be known before it runs. So they count as
/// [`CommandClass::Admin`], which a read-only connection refuses.
pub(crate) const READ_ONLY_SCRIPTS: &[&str] = &["EVAL_RO", "EVALSHA_RO", "FCALL_RO"];

/// Commands whose flags do not say `write` but which can change data or make
/// other clients act on it.
///
/// A command's flags are what it declares about itself, and some declare
/// `readonly` wrongly, so this list overrides them where it matters: Redis
/// marks PFCOUNT `may_replicate` (which `COMMAND INFO` does not show)
/// because it writes the cached count back into the value, which also
/// aborts other clients' WATCHes on it; RedisBloom's CF.COMPACT rewrites
/// the filter in place.
const WRITE_EXTRA: &[&str] = &[
    "EVAL",
    "EVALSHA",
    "FCALL",
    "PUBLISH",
    "SPUBLISH",
    "SCRIPT LOAD",
    "PFCOUNT",
    "CF.COMPACT",
];

/// Commands whose flags do not say `admin` but which act on the whole
/// server or on other clients.
const ADMIN_EXTRA: &[&str] = &[
    "SCRIPT FLUSH",
    "SCRIPT KILL",
    "FUNCTION KILL",
    "MEMORY PURGE",
    // Free search cursors, other clients' included; flagged `readonly`.
    "FT.CURSOR DEL",
    "FT.CURSOR GC",
    // Module-internal topology commands, flagged `readonly` by their modules.
    "SEARCH.CLUSTERSET",
    "SEARCH.CLUSTERREFRESH",
    "TIMESERIES.CLUSTERSET",
    "TIMESERIES.REFRESHCLUSTER",
];

impl CommandSpec {
    /// How this command affects the server; see [`CommandClass`].
    pub fn class(&self) -> CommandClass {
        let name = self.name;
        // `COMMAND` and all its subcommands only describe the command table.
        let command_docs = name == "COMMAND" || name.starts_with("COMMAND ");
        if CONNECTION_STATE.contains(&name) {
            CommandClass::ConnectionState
        } else if READ_INTROSPECTION.contains(&name) || command_docs {
            CommandClass::Read
        } else if READ_ONLY_SCRIPTS.contains(&name) {
            CommandClass::Admin
        } else if WRITE_EXTRA.contains(&name) {
            CommandClass::Write
        } else if ADMIN_EXTRA.contains(&name)
            || self.flags.contains(Flags::ADMIN)
            || self.acl.contains(AclCategories::ADMIN)
            // Includes FLUSHALL, SWAPDB, RESTORE, MIGRATE and SORT (which can
            // store); the dangerous commands that only read are allowed above.
            || self.acl.contains(AclCategories::DANGEROUS)
        {
            CommandClass::Admin
        } else if self.flags.contains(Flags::WRITE)
            // A module command that does not declare itself read-only may
            // write anything.
            || (self.flags.contains(Flags::MODULE) && !self.flags.contains(Flags::READONLY))
        {
            CommandClass::Write
        } else {
            CommandClass::Read
        }
    }

    pub fn is_subcommand(&self) -> bool {
        self.name.contains(' ')
    }
}

/// Every command and subcommand, sorted by name; a container is directly
/// followed by its subcommands.
pub fn all() -> &'static [CommandSpec] {
    table::COMMANDS
}

/// Finds a command by name, ignoring case. For a container such as `CONFIG`,
/// `sub` picks the subcommand (`lookup("config", Some("set"))` is `CONFIG
/// SET`); an unknown subcommand finds nothing, and no `sub` finds the
/// container. For other commands `sub` is just their first argument and is
/// ignored, so `lookup(argv[0], argv.get(1))` resolves any command line.
pub fn lookup(name: &str, sub: Option<&str>) -> Option<&'static CommandSpec> {
    // Spaces only separate a container from its subcommand in table names;
    // `"config set"` typed as one argument is not a command.
    if name.is_empty() || name.contains(' ') {
        return None;
    }
    let ix = position(&[name])?;
    let spec = &table::COMMANDS[ix];
    if subcommands_at(ix).is_empty() {
        return Some(spec);
    }
    match sub {
        None => Some(spec),
        Some(sub) if sub.is_empty() || sub.contains(' ') => None,
        Some(sub) => position(&[name, sub]).map(|ix| &table::COMMANDS[ix]),
    }
}

/// The subcommands of a container (`CONFIG GET`, `CONFIG SET`, ...); empty
/// for any other command.
pub fn subcommands(spec: &CommandSpec) -> &'static [CommandSpec] {
    if spec.is_subcommand() {
        return &[];
    }
    position(&[spec.name]).map_or(&[], subcommands_at)
}

/// Classifies a command line (name first, as typed). See [`CommandClass`].
pub fn classify(argv: &[impl AsRef<str>]) -> CommandClass {
    let Some(name) = argv.first() else {
        return CommandClass::Unknown;
    };
    lookup(name.as_ref(), argv.get(1).map(AsRef::as_ref))
        .map_or(CommandClass::Unknown, |spec| spec.class())
}

/// Index of the entry named `parts` joined by spaces, ignoring case.
fn position(parts: &[&str]) -> Option<usize> {
    table::COMMANDS
        .binary_search_by(|spec| compare(spec.name, parts))
        .ok()
}

/// Compares a table name with `parts` joined by spaces and upper-cased,
/// without building that string.
fn compare(name: &str, parts: &[&str]) -> Ordering {
    let query = parts.iter().enumerate().flat_map(|(i, part)| {
        (i > 0)
            .then_some(b' ')
            .into_iter()
            .chain(part.bytes().map(|b| b.to_ascii_uppercase()))
    });
    name.bytes().cmp(query)
}

/// The subcommands right after the container at `ix`. Names sort bytewise
/// and a space sorts before every character a name can contain, so they
/// always follow their container directly.
fn subcommands_at(ix: usize) -> &'static [CommandSpec] {
    let container = table::COMMANDS[ix].name;
    let rest = &table::COMMANDS[ix + 1..];
    let len = rest
        .iter()
        .take_while(|spec| {
            spec.name
                .strip_prefix(container)
                .is_some_and(|sub| sub.starts_with(' '))
        })
        .count();
    &rest[..len]
}

/// Defines a set of named bits: the flag sets of [`CommandSpec`].
macro_rules! bit_set {
    (
        $(#[$meta:meta])*
        $set:ident($repr:ty) {
            $($(#[$doc:meta])* $bit:ident = $shift:literal, $text:literal;)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct $set($repr);

        impl $set {
            $($(#[$doc])* pub const $bit: Self = Self(1 << $shift);)*

            /// Every member, with its name as Redis spells it.
            const NAMES: &'static [(Self, &'static str)] = &[$((Self::$bit, $text)),*];

            #[cfg(test)]
            const IDENTS: &'static [(&'static str, &'static str)] = &[$((stringify!($bit), $text)),*];

            pub const fn empty() -> Self {
                Self(0)
            }

            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }

            /// Whether every member of `other` is in `self`.
            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }

            /// The member Redis calls `name` (any case).
            pub fn from_name(name: &str) -> Option<Self> {
                Self::NAMES
                    .iter()
                    .find(|(_, text)| text.eq_ignore_ascii_case(name))
                    .map(|(bit, _)| *bit)
            }

            /// The names of the members, as Redis spells them.
            pub fn names(self) -> impl Iterator<Item = &'static str> {
                Self::NAMES
                    .iter()
                    .filter(move |(bit, _)| self.contains(*bit))
                    .map(|(_, text)| *text)
            }
        }

        impl BitOr for $set {
            type Output = Self;

            fn bitor(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }
        }

        impl fmt::Debug for $set {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_set().entries(self.names()).finish()
            }
        }
    };
}

bit_set! {
    /// Command flags from `COMMAND INFO`, plus the doc flags of `COMMAND DOCS`
    /// (`deprecated`, `syscmd`).
    Flags(u32) {
        WRITE = 0, "write";
        READONLY = 1, "readonly";
        DENYOOM = 2, "denyoom";
        MODULE = 3, "module";
        ADMIN = 4, "admin";
        PUBSUB = 5, "pubsub";
        NOSCRIPT = 6, "noscript";
        BLOCKING = 7, "blocking";
        LOADING = 8, "loading";
        STALE = 9, "stale";
        SKIP_MONITOR = 10, "skip_monitor";
        SKIP_SLOWLOG = 11, "skip_slowlog";
        ASKING = 12, "asking";
        FAST = 13, "fast";
        NO_AUTH = 14, "no_auth";
        MAY_REPLICATE = 15, "may_replicate";
        NO_MANDATORY_KEYS = 16, "no_mandatory_keys";
        NO_MULTI = 17, "no_multi";
        MOVABLEKEYS = 18, "movablekeys";
        ALLOW_BUSY = 19, "allow_busy";
        NO_ASYNC_LOADING = 20, "no_async_loading";
        SCRIPT_RUNNER = 21, "script_runner";
        /// Doc flag: superseded by another command.
        DEPRECATED = 22, "deprecated";
        /// Doc flag: used by Redis itself (replication, cluster bus), not by clients.
        SYSCMD = 23, "syscmd";
    }
}

bit_set! {
    /// ACL categories (`@read`, `@dangerous`, ...), named without the `@`.
    AclCategories(u64) {
        KEYSPACE = 0, "keyspace";
        READ = 1, "read";
        WRITE = 2, "write";
        SET = 3, "set";
        SORTEDSET = 4, "sortedset";
        LIST = 5, "list";
        HASH = 6, "hash";
        STRING = 7, "string";
        BITMAP = 8, "bitmap";
        HYPERLOGLOG = 9, "hyperloglog";
        GEO = 10, "geo";
        STREAM = 11, "stream";
        PUBSUB = 12, "pubsub";
        ADMIN = 13, "admin";
        FAST = 14, "fast";
        SLOW = 15, "slow";
        BLOCKING = 16, "blocking";
        DANGEROUS = 17, "dangerous";
        CONNECTION = 18, "connection";
        TRANSACTION = 19, "transaction";
        SCRIPTING = 20, "scripting";
        ARRAY = 21, "array";
        JSON = 22, "json";
        SEARCH = 23, "search";
        TIMESERIES = 24, "timeseries";
        BLOOM = 25, "bloom";
        CUCKOO = 26, "cuckoo";
        CMS = 27, "cms";
        TOPK = 28, "topk";
        TDIGEST = 29, "tdigest";
    }
}

#[cfg(test)]
mod tests;
