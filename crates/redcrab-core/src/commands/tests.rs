use super::*;

use CommandClass::*;

fn name_of(name: &str, sub: Option<&str>) -> Option<&'static str> {
    lookup(name, sub).map(|spec| spec.name)
}

#[test]
fn table_is_sorted_unique_and_well_formed() {
    let all = all();
    assert!(all.len() > 600, "only {} commands", all.len());
    for pair in all.windows(2) {
        assert!(
            pair[0].name.as_bytes() < pair[1].name.as_bytes(),
            "{} / {}",
            pair[0].name,
            pair[1].name
        );
    }
    for spec in all {
        let name = spec.name;
        assert!(!name.is_empty());
        assert!(!name.bytes().any(|b| b.is_ascii_lowercase()), "{name}");
        assert!(
            name.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
            "{name}"
        );
        assert!(name.matches(' ').count() <= 1, "{name}");
        assert!(spec.syntax.starts_with(name), "{name}: {}", spec.syntax);
        assert_ne!(spec.arity, 0, "{name}");
        assert!(!spec.group.is_empty(), "{name}");
        if let Some((container, _)) = name.split_once(' ') {
            let container = lookup(container, None).unwrap_or_else(|| panic!("{name}"));
            assert!(
                subcommands(container).iter().any(|s| s.name == name),
                "{name}"
            );
            assert!(spec.is_subcommand());
        }
    }
}

#[test]
fn includes_module_commands() {
    for (name, group) in [
        ("JSON.SET", "json"),
        ("JSON.GET", "json"),
        ("FT.SEARCH", "search"),
        ("FT.CREATE", "search"),
        ("TS.ADD", "timeseries"),
        ("BF.ADD", "bloom"),
        ("CF.ADD", "bloom"),
        ("TOPK.ADD", "bloom"),
        ("VADD", "vectorset"),
    ] {
        let spec = lookup(name, None).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(spec.group, group, "{name}");
        assert!(spec.flags.contains(Flags::MODULE), "{name}");
    }
}

#[test]
fn looks_up_names_ignoring_case() {
    for name in ["get", "GET", "Get", "gEt"] {
        assert_eq!(name_of(name, None), Some("GET"));
    }
    assert_eq!(name_of("json.set", None), Some("JSON.SET"));
    assert_eq!(name_of("Ft.Search", None), Some("FT.SEARCH"));
    // Mixed case on the server (`search.CLUSTERINFO`) is normalized too.
    assert_eq!(
        name_of("search.clusterinfo", None),
        Some("SEARCH.CLUSTERINFO")
    );
    assert_eq!(name_of("eval_ro", None), Some("EVAL_RO"));
}

#[test]
fn resolves_subcommands() {
    let cases: &[(&str, Option<&str>, Option<&str>)] = &[
        ("config", Some("set"), Some("CONFIG SET")),
        ("CONFIG", Some("GET"), Some("CONFIG GET")),
        ("Client", Some("Kill"), Some("CLIENT KILL")),
        ("client", Some("no-evict"), Some("CLIENT NO-EVICT")),
        ("xinfo", Some("stream"), Some("XINFO STREAM")),
        ("object", Some("encoding"), Some("OBJECT ENCODING")),
        ("ft.config", Some("set"), Some("FT.CONFIG SET")),
        (
            "_ft.debug",
            Some("ft.profile"),
            Some("_FT.DEBUG FT.PROFILE"),
        ),
        // A container alone, and with an unknown subcommand.
        ("config", None, Some("CONFIG")),
        ("config", Some("bogus"), None),
        ("config", Some(""), None),
        ("config", Some("set extra"), None),
        // Not a container: the second word is just an argument.
        ("get", Some("set"), Some("GET")),
        ("set", Some("config"), Some("SET")),
        // Not commands.
        ("nosuch", None, None),
        ("", None, None),
        ("", Some("set"), None),
        ("config set", None, None),
        ("conf", None, None),
        ("configx", None, None),
        ("get\0", None, None),
        ("géт", None, None),
    ];
    for &(name, sub, expected) in cases {
        assert_eq!(name_of(name, sub), expected, "lookup({name:?}, {sub:?})");
    }
}

#[test]
fn lists_subcommands() {
    let config = lookup("config", None).unwrap();
    let names: Vec<_> = subcommands(config).iter().map(|s| s.name).collect();
    for expected in [
        "CONFIG GET",
        "CONFIG SET",
        "CONFIG REWRITE",
        "CONFIG RESETSTAT",
    ] {
        assert!(names.contains(&expected), "{names:?}");
    }
    assert!(names.iter().all(|n| n.starts_with("CONFIG ")));
    assert!(subcommands(lookup("get", None).unwrap()).is_empty());
    assert!(subcommands(lookup("config", Some("set")).unwrap()).is_empty());
    // CLIENT and CLUSTER share a prefix; neither picks up the other's.
    let client = subcommands(lookup("client", None).unwrap());
    assert!(client.iter().all(|s| s.name.starts_with("CLIENT ")));
    assert!(client.len() > 15);
}

#[test]
fn classifies_command_lines() {
    let cases: &[(&str, CommandClass)] = &[
        // Reads, in any case.
        ("GET k", Read),
        ("get k", Read),
        ("HGETALL h", Read),
        ("XRANGE s - +", Read),
        ("SCAN 0", Read),
        ("TYPE k", Read),
        ("PING", Read),
        ("ECHO hi", Read),
        ("XREAD BLOCK 0 STREAMS s $", Read),
        ("BLPOP q 0", Write),
        ("WAIT 1 0", Read),
        // Rewrites the cached count in the value.
        ("PFCOUNT h", Write),
        ("OBJECT ENCODING k", Read),
        ("MEMORY MALLOC-STATS", Read),
        ("CLUSTER KEYSLOT k", Read),
        ("PUBSUB CHANNELS", Read),
        // Admin-flagged or dangerous introspection on the allow-list.
        ("INFO", Read),
        ("info memory", Read),
        ("CONFIG GET maxmemory", Read),
        ("config get *", Read),
        ("SLOWLOG GET 10", Read),
        ("SLOWLOG LEN", Read),
        ("CLIENT LIST", Read),
        ("CLIENT INFO", Read),
        ("CLIENT GETNAME", Read),
        ("CLIENT ID", Read),
        ("MEMORY USAGE k", Read),
        ("MEMORY STATS", Read),
        ("MEMORY DOCTOR", Read),
        ("LATENCY LATEST", Read),
        ("LATENCY HISTORY command", Read),
        ("LATENCY DOCTOR", Read),
        ("COMMAND", Read),
        ("COMMAND DOCS get", Read),
        ("COMMAND INFO", Read),
        ("COMMAND COUNT", Read),
        ("DBSIZE", Read),
        ("KEYS *", Read),
        ("SORT_RO list", Read),
        ("CLUSTER INFO", Read),
        ("CLUSTER NODES", Read),
        ("CLUSTER SHARDS", Read),
        ("CLUSTER SLOTS", Read),
        ("CLUSTER MYID", Read),
        ("ACL WHOAMI", Read),
        ("ACL LIST", Read),
        ("ACL USERS", Read),
        ("ACL CAT", Read),
        ("MODULE LIST", Read),
        ("TIME", Read),
        ("LASTSAVE", Read),
        ("ROLE", Read),
        ("SCRIPT EXISTS abc", Read),
        ("FUNCTION LIST", Read),
        ("FUNCTION STATS", Read),
        ("FUNCTION DUMP", Read),
        // Read-only scripts may still run administrative commands.
        ("EVAL_RO return 1 0", Admin),
        ("evalsha_ro abc 0", Admin),
        ("FCALL_RO f 0", Admin),
        // Writes.
        ("SET k v", Write),
        ("set k v", Write),
        ("DEL k", Write),
        ("UNLINK k", Write),
        ("EXPIRE k 1", Write),
        ("PERSIST k", Write),
        ("RENAME a b", Write),
        ("HSET h f v", Write),
        ("LPUSH l v", Write),
        ("XADD s * f v", Write),
        ("XREADGROUP GROUP g c STREAMS s >", Write),
        ("FUNCTION LOAD code", Write),
        ("FUNCTION DELETE lib", Write),
        ("HIMPORT SET k fs v", Write),
        // Scripts can write, and publishing makes subscribers act.
        ("EVAL return 1 0", Write),
        ("EVALSHA abc 0", Write),
        ("FCALL f 0", Write),
        ("PUBLISH ch msg", Write),
        ("SPUBLISH ch msg", Write),
        ("SCRIPT LOAD return", Write),
        // Administrative or dangerous.
        ("FLUSHALL", Admin),
        ("FLUSHDB ASYNC", Admin),
        ("SWAPDB 0 1", Admin),
        ("RESTORE k 0 x", Admin),
        ("MIGRATE h 1 k 0 1", Admin),
        ("SORT list STORE dst", Admin),
        ("CONFIG SET maxmemory 1", Admin),
        ("config rewrite", Admin),
        ("CONFIG RESETSTAT", Admin),
        ("CLIENT KILL ID 1", Admin),
        ("client pause 10", Admin),
        ("CLIENT NO-EVICT on", Admin),
        ("DEBUG SLEEP 0", Admin),
        ("SHUTDOWN", Admin),
        ("SAVE", Admin),
        ("BGSAVE", Admin),
        ("REPLICAOF NO ONE", Admin),
        ("FAILOVER", Admin),
        ("ACL SETUSER u", Admin),
        ("ACL LOG", Admin),
        ("SLOWLOG RESET", Admin),
        ("LATENCY RESET", Admin),
        ("MODULE LOAD x", Admin),
        ("CLUSTER RESET", Admin),
        ("SCRIPT FLUSH", Admin),
        ("SCRIPT KILL", Admin),
        ("FUNCTION KILL", Admin),
        ("MEMORY PURGE", Admin),
        ("PFDEBUG GETREG h", Admin),
        ("SEARCH.CLUSTERSET x", Admin),
        ("timeseries.clusterset x", Admin),
        // Per-connection state.
        ("SELECT 1", ConnectionState),
        ("MULTI", ConnectionState),
        ("EXEC", ConnectionState),
        ("DISCARD", ConnectionState),
        ("WATCH k", ConnectionState),
        ("UNWATCH", ConnectionState),
        ("SUBSCRIBE ch", ConnectionState),
        ("PSUBSCRIBE *", ConnectionState),
        ("SSUBSCRIBE ch", ConnectionState),
        ("UNSUBSCRIBE", ConnectionState),
        ("PUNSUBSCRIBE", ConnectionState),
        ("SUNSUBSCRIBE", ConnectionState),
        ("MONITOR", ConnectionState),
        ("RESET", ConnectionState),
        ("AUTH pw", ConnectionState),
        ("HELLO 3", ConnectionState),
        ("QUIT", ConnectionState),
        ("CLIENT REPLY OFF", ConnectionState),
        ("CLIENT SETNAME x", ConnectionState),
        ("CLIENT TRACKING on", ConnectionState),
        ("READONLY", ConnectionState),
        ("READWRITE", ConnectionState),
        ("ASKING", ConnectionState),
        ("SYNC", ConnectionState),
        ("PSYNC ? -1", ConnectionState),
        ("REPLCONF ack 0", ConnectionState),
        ("HIMPORT PREPARE fs f", ConnectionState),
        // Module commands.
        ("JSON.GET doc", Read),
        ("JSON.SET doc $ 1", Write),
        ("json.del doc", Write),
        ("FT.SEARCH idx *", Read),
        ("FT.AGGREGATE idx *", Read),
        ("FT.INFO idx", Read),
        ("FT.TAGVALS idx f", Read),
        ("FT.CONFIG GET *", Read),
        ("FT.CREATE idx SCHEMA f TEXT", Write),
        ("FT.DROPINDEX idx", Admin),
        ("FT.CONFIG SET x 1", Admin),
        ("_FT.DEBUG GC_FORCEINVOKE idx", Admin),
        ("TS.GET ts", Read),
        ("TS.ADD ts * 1", Write),
        ("BF.EXISTS bf x", Read),
        ("BF.ADD bf x", Write),
        // Declared `readonly` by their modules, but they are not.
        ("CF.EXISTS cf x", Read),
        ("CF.COMPACT cf", Write),
        ("FT.CURSOR READ idx 1", Read),
        ("FT.CURSOR DEL idx 1", Admin),
        ("ft.cursor gc idx", Admin),
        ("VSIM v ELE x", Read),
        ("VADD v VALUES 1 0 x", Write),
        // Unknown.
        ("NOSUCHCOMMAND", Unknown),
        ("CONFIG BOGUS", Unknown),
        ("client bogus", Unknown),
        ("FT.CONFIG BOGUS", Unknown),
        ("MYMODULE.DO x", Unknown),
    ];
    for &(line, expected) in cases {
        let argv: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(classify(&argv), expected, "{line}");
        // Owned strings work the same.
        let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        assert_eq!(classify(&owned), expected, "{line}");
    }
    let empty: &[&str] = &[];
    assert_eq!(classify(empty), Unknown);
    assert_eq!(classify(&[""]), Unknown);
}

#[test]
fn containers_alone_are_harmless() {
    // The server answers a bare container with an arity error.
    assert_eq!(classify(&["CONFIG"]), Read);
    assert_eq!(classify(&["client"]), Read);
}

/// The hand-written lists name real commands; a typo would silently
/// classify a command wrong.
#[test]
fn every_listed_name_is_in_the_table() {
    for list in [
        CONNECTION_STATE,
        READ_INTROSPECTION,
        READ_ONLY_SCRIPTS,
        WRITE_EXTRA,
        ADMIN_EXTRA,
    ] {
        for &name in list {
            let (command, sub) = match name.split_once(' ') {
                Some((command, sub)) => (command, Some(sub)),
                None => (name, None),
            };
            assert_eq!(
                name_of(command, sub),
                Some(name),
                "{name} is not in the table"
            );
        }
    }
}

#[test]
fn nothing_that_writes_or_administers_classifies_as_read() {
    for spec in all() {
        let class = spec.class();
        if spec.flags.contains(Flags::WRITE) {
            assert!(
                matches!(class, Write | Admin | ConnectionState),
                "{}",
                spec.name
            );
        }
        if spec.flags.contains(Flags::ADMIN) || spec.acl.contains(AclCategories::DANGEROUS) {
            assert!(
                class != Read || READ_INTROSPECTION.contains(&spec.name),
                "{} is admin but classifies as read",
                spec.name
            );
        }
        if class == Read
            && !spec.name.starts_with("COMMAND")
            && !READ_INTROSPECTION.contains(&spec.name)
        {
            assert!(!spec.flags.contains(Flags::WRITE), "{}", spec.name);
            assert!(!spec.flags.contains(Flags::ADMIN), "{}", spec.name);
        }
    }
}

#[test]
fn flag_names_match_their_constants() {
    for (ident, text) in Flags::IDENTS.iter().chain(AclCategories::IDENTS) {
        assert_eq!(*ident, text.to_ascii_uppercase());
    }
    assert_eq!(Flags::from_name("WRITE"), Some(Flags::WRITE));
    assert_eq!(Flags::from_name("no_auth"), Some(Flags::NO_AUTH));
    assert_eq!(Flags::from_name("nope"), None);
    assert_eq!(
        AclCategories::from_name("dangerous"),
        Some(AclCategories::DANGEROUS)
    );
    let set = lookup("set", None).unwrap();
    assert!(set.flags.contains(Flags::WRITE | Flags::DENYOOM));
    assert!(!set.flags.contains(Flags::READONLY));
    assert_eq!(
        set.acl.names().collect::<Vec<_>>(),
        ["write", "string", "slow"]
    );
    assert_eq!(format!("{:?}", set.flags), r#"{"write", "denyoom"}"#);
    assert!(Flags::empty().is_empty());
}

#[test]
fn syntax_follows_the_docs_notation() {
    let syntax = |name: &str| lookup(name, None).unwrap().syntax;
    assert!(
        syntax("SET").starts_with("SET key value [NX | XX"),
        "{}",
        syntax("SET")
    );
    assert!(syntax("SET").contains("[GET] [EX seconds | PX milliseconds"));
    assert_eq!(
        syntax("LMOVE"),
        "LMOVE source destination <LEFT | RIGHT> <LEFT | RIGHT>"
    );
    assert_eq!(syntax("DEL"), "DEL key [key ...]");
    assert_eq!(syntax("DBSIZE"), "DBSIZE");
    assert_eq!(syntax("PING"), "PING [message]");
    assert_eq!(syntax("CONFIG"), "CONFIG <subcommand>");
    assert!(syntax("SORT").contains("[GET pattern [GET pattern ...]]"));
    assert!(syntax("ZADD").ends_with("score member [score member ...]"));
    assert!(
        syntax("MIGRATE").contains(r#"<key | "">"#),
        "{}",
        syntax("MIGRATE")
    );
    assert_eq!(
        lookup("config", Some("set")).unwrap().syntax,
        "CONFIG SET parameter value [parameter value ...]"
    );
}

#[test]
fn deprecated_commands_are_marked() {
    for name in ["GETSET", "SETNX", "RPOPLPUSH", "GEORADIUS"] {
        assert!(
            lookup(name, None)
                .unwrap()
                .flags
                .contains(Flags::DEPRECATED),
            "{name}"
        );
    }
    assert!(
        !lookup("SET", None)
            .unwrap()
            .flags
            .contains(Flags::DEPRECATED)
    );
}
