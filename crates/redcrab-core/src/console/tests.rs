use proptest::prelude::*;

use super::*;

#[test]
fn hints_what_follows_like_redis_cli() {
    assert_eq!(
        hint("GET").as_deref(),
        Some(" key"),
        "a space before it, since the line does not end in one"
    );
    assert_eq!(hint("GET ").as_deref(), Some("key"));
    assert_eq!(
        hint("get   ").as_deref(),
        Some("key"),
        "any case, any spaces"
    );
    assert_eq!(hint("GET k"), None, "complete");
    assert_eq!(hint("GET k extra"), None, "does not fit");
    assert_eq!(
        hint("HSET h").as_deref(),
        Some(" field value [field value ...]")
    );
    assert_eq!(
        hint("hset h f v f2").as_deref(),
        Some(" value [field value ...]")
    );
    assert_eq!(hint("DEL a b c").as_deref(), Some(" [key ...]"));
    assert_eq!(
        hint("ZADD k NX").as_deref(),
        Some(" [GT | LT] [CH] [INCR] score member [score member ...]")
    );
    assert_eq!(
        hint("SET k v EX").as_deref(),
        Some(
            " seconds [NX | XX | IFEQ ifeq-value | IFNE ifne-value | IFDEQ ifdeq-digest | IFDNE ifdne-digest] [GET]"
        )
    );
    assert_eq!(hint("SET k v EX 10 NX GET"), None, "every option used");
    assert_eq!(
        hint("SCAN 0 ").as_deref(),
        Some("[MATCH pattern] [COUNT count] [TYPE type]")
    );
    assert_eq!(
        hint("SCAN 0 COUNT 10 ").as_deref(),
        Some("[MATCH pattern] [TYPE type]"),
        "options in any order"
    );
    assert_eq!(hint("PING").as_deref(), Some(" [message]"));
    assert_eq!(hint("DBSIZE"), None, "takes nothing");
}

#[test]
fn hints_subcommands() {
    assert_eq!(hint("CONFIG").as_deref(), Some(" <subcommand>"));
    assert_eq!(hint("config ").as_deref(), Some("<subcommand>"));
    assert_eq!(
        hint("CONFIG GET").as_deref(),
        Some(" parameter [parameter ...]")
    );
    assert_eq!(
        hint("config set maxmemory").as_deref(),
        Some(" value [parameter value ...]")
    );
    assert_eq!(hint("CONFIG GE"), None, "not a subcommand yet");
    assert_eq!(hint("CONFIG BOGUS "), None);
    assert_eq!(
        hint("CLIENT LIST TYPE").as_deref(),
        Some(" <NORMAL | MASTER | REPLICA | PUBSUB> [ID client-id [client-id ...]]")
    );
}

#[test]
fn hints_module_commands() {
    assert_eq!(
        hint("JSON.GET k").as_deref(),
        Some(
            " [INDENT indent] [NEWLINE newline] [SPACE space] [FORMAT <STRING | EXPAND1 | EXPAND>] [path [path ...]]"
        )
    );
    assert!(
        hint("FT.SEARCH idx")
            .unwrap()
            .starts_with(" query [NOCONTENT]")
    );
    assert!(hint("TS.ADD k * ").unwrap().starts_with("value"));
}

#[test]
fn no_hint_for_what_is_not_a_command() {
    assert_eq!(hint(""), None);
    assert_eq!(hint("   "), None);
    assert_eq!(hint("HGETAL"), None, "a name being typed");
    assert_eq!(hint("clear"), None, "the console's own");
    assert_eq!(hint(r#""\xff" k"#), None);
    // A closing quote with text right after it cannot run.
    assert_eq!(hint(r#"GET "k"x"#), None);
}

#[test]
fn hints_through_quotes() {
    assert!(hint(r#"SET "my key""#).unwrap().starts_with(" value"));
    // Still typing a quoted value: what comes after it.
    assert_eq!(
        hint(r#"HSET h "f"#).as_deref(),
        Some(" value [field value ...]")
    );
    assert_eq!(
        hint(r#"HSET h "f "#).as_deref(),
        Some(" value [field value ...]")
    );
    // Binary keys.
    assert_eq!(
        hint(r#"HSET "\xff\x00" "#).as_deref(),
        Some("field value [field value ...]")
    );
    // MIGRATE's empty key.
    assert!(
        hint(r#"MIGRATE h 6379 "" "#)
            .unwrap()
            .starts_with("destination-db timeout")
    );
}

#[test]
fn tells_complete_lines_from_ones_that_do_not_fit() {
    assert_eq!(fits("GET k"), Some(true), "complete");
    assert_eq!(fits("GET"), Some(true), "not done yet");
    assert_eq!(fits("GET k extra"), Some(false));
    assert_eq!(fits("SET k v EX 10 NX"), Some(true), "options in any order");
    assert_eq!(fits("SET k v NX XX"), Some(false), "both of a choice");
    assert_eq!(fits("SET k v GET GET"), Some(false), "an option twice");
    assert_eq!(fits("config set maxmemory 1gb"), Some(true));
    assert_eq!(fits("CONFIG"), Some(true));
    assert_eq!(fits(r#"SET "k"x v"#), Some(false), "text after a quote");
    assert_eq!(fits(r#"SET "k v"#), Some(true), "a quote still open");
    assert_eq!(fits("BOGUS"), None);
    assert_eq!(fits("CONFIG BOGUS"), None);
    assert_eq!(fits(""), None);
}

proptest! {
    /// Any line: no panic; a hint starts with a space exactly when the line
    /// does not end in one (outside quotes), and is never blank.
    #[test]
    fn any_line(line in "(SET|GET|CONFIG|ZADD|XADD|CLIENT|BOGUS|json\\.set)( [a-zA-Z0-9\"' *~]{0,5}){0,5} ?") {
        if let Some(hint) = hint(&line) {
            prop_assert!(!hint.trim().is_empty());
            let open = words::split(&line).words.last().is_some_and(|w| w.open);
            prop_assert_eq!(
                hint.starts_with(' '),
                !line.ends_with(' ') || open,
                "{:?} {:?}", line, hint
            );
        }
    }
}
