use proptest::prelude::*;

use super::*;

/// The names completing `input` with the cursor at `|` (the end without
/// one).
fn names(input: &str) -> Vec<&'static str> {
    complete_at(input)
        .candidates
        .iter()
        .map(|c| c.name)
        .collect()
}

fn texts(input: &str) -> Vec<String> {
    complete_at(input)
        .candidates
        .into_iter()
        .map(|c| c.text)
        .collect()
}

fn complete_at(input: &str) -> Completion {
    match input.split_once('|') {
        Some((before, after)) => complete(&format!("{before}{after}"), before.len()),
        None => complete(input, input.len()),
    }
}

#[test]
fn completes_command_names_by_prefix() {
    assert_eq!(names("HGETA"), ["HGETALL"], "a prefix of one command");
    let get = names("GET");
    assert_eq!(get[0], "GET", "the exact match first");
    assert_eq!(
        &get[1..6],
        ["GETBIT", "GETDEL", "GETEX", "GETRANGE", "GETSET"]
    );
    // Fuzzy matches come after every prefix match.
    let fuzzy = complete_at("GET").candidates;
    let first_fuzzy = fuzzy.iter().position(|c| c.fuzzy).unwrap_or(fuzzy.len());
    assert!(
        fuzzy[..first_fuzzy]
            .iter()
            .all(|c| c.name.starts_with("GET"))
    );
    assert!(fuzzy[first_fuzzy..].iter().all(|c| c.fuzzy));

    let completion = complete_at("hge");
    assert_eq!(completion.range, 0..3);
    let first = &completion.candidates[0];
    assert_eq!(
        (first.name, first.text.as_str(), first.kind),
        ("HGET", "hget", CandidateKind::Command)
    );
    assert!(first.summary.contains("hash"), "{}", first.summary);
    assert!(!first.fuzzy);
}

#[test]
fn follows_the_case_typed() {
    assert_eq!(texts("hgeta"), ["hgetall"]);
    assert_eq!(texts("HGETA"), ["HGETALL"]);
    assert_eq!(texts("hGeTa"), ["HGETALL"]);
    assert_eq!(texts("config ge"), ["get"]);
    assert_eq!(texts("CONFIG ge"), ["get"]);
    assert_eq!(texts("config GE"), ["GET"]);
    // Nothing typed in the word: the command's case.
    assert!(texts("set k v ").contains(&"nx".to_string()));
    assert!(texts("SET k v ").contains(&"NX".to_string()));
    // Commands with dots and digits keep them.
    assert_eq!(texts("json.arra")[0], "json.arrappend");
}

#[test]
fn deprecated_commands_come_last() {
    // SLAVEOF is deprecated (REPLICAOF replaced it).
    let s = names("S");
    let slaveof = s.iter().position(|&n| n == "SLAVEOF").unwrap();
    let set = s.iter().position(|&n| n == "SET").unwrap();
    assert!(set < slaveof);
    let last_fresh = s
        .iter()
        .rposition(|&n| {
            !commands::lookup(n, None)
                .unwrap()
                .flags
                .contains(Flags::DEPRECATED)
        })
        .unwrap();
    assert!(slaveof > last_fresh, "{s:?}");
}

#[test]
fn matches_letters_in_order() {
    let hga = complete_at("hga").candidates;
    let hgetall = hga.iter().find(|c| c.name == "HGETALL").unwrap();
    assert!(hgetall.fuzzy);
    assert_eq!(hgetall.text, "hgetall");
    // Words starting a part of the name score more.
    let fts = names("ftsea");
    assert_eq!(fts[0], "FT.SEARCH", "{fts:?}");
    // One letter is only a prefix; nothing fuzzy.
    assert!(complete_at("z").candidates.iter().all(|c| !c.fuzzy));
    assert!(names("qqqq").is_empty());
    assert_eq!(fuzzy_score(b"abc", b"xaxbxc"), Some(3 - 2));
    assert!(fuzzy_score(b"ab", b"ab").unwrap() > fuzzy_score(b"ab", b"a_b").unwrap());
    assert_eq!(fuzzy_score(b"ba", b"ab"), None);
}

#[test]
fn leaves_out_what_redis_keeps_for_itself() {
    // REPLCONF and RESTORE-ASKING are flagged `syscmd`.
    for name in ["REPLCONF", "RESTORE-ASKING"] {
        assert!(!names(&name[..name.len() - 1]).contains(&name), "{name}");
    }
    // Subcommands are not offered as commands.
    assert!(names("CONFIG").iter().all(|n| !n.contains(' ')));
}

#[test]
fn completes_subcommands_of_containers() {
    let completion = complete_at("CONFIG G");
    assert_eq!(completion.range, 7..8);
    let get = &completion.candidates[0];
    assert_eq!(
        (get.name, get.text.as_str(), get.kind),
        ("CONFIG GET", "GET", CandidateKind::Subcommand)
    );
    assert!(!get.summary.is_empty());
    let all = names("CLIENT ");
    assert!(
        all.contains(&"CLIENT LIST") && all.contains(&"CLIENT KILL"),
        "{all:?}"
    );
    assert_eq!(complete_at("CLIENT ").range, 7..7);
    // Not a container: its first argument is a value.
    assert!(names("GET ").is_empty());
}

#[test]
fn completes_tokens_where_the_usage_allows_them() {
    let completion = complete_at("SET k v ");
    let tokens: Vec<&str> = completion.candidates.iter().map(|c| c.name).collect();
    assert_eq!(&tokens[..2], ["NX", "XX"]);
    assert!(tokens.contains(&"KEEPTTL") && tokens.contains(&"GET"));
    let ex = completion
        .candidates
        .iter()
        .find(|c| c.name == "EX")
        .unwrap();
    assert_eq!(
        (ex.kind, ex.summary.as_ref()),
        (CandidateKind::Token, "seconds")
    );
    assert_eq!(names("SET k v ke"), ["KEEPTTL"]);
    assert_eq!(names("SET k v GET E")[..2], ["EX", "EXAT"]);
    // Subcommand usages too.
    assert_eq!(names("CLIENT LIST T"), ["TYPE"]);
    assert_eq!(
        names("CLIENT LIST TYPE "),
        ["NORMAL", "MASTER", "REPLICA", "PUBSUB"]
    );
    // Where a value goes, nothing.
    assert!(names("SET ").is_empty());
    assert!(names("SET k ").is_empty());
    // Unknown commands and arguments that do not fit: nothing.
    assert!(names("BOGUS ").is_empty());
    assert!(names("SET k v BOGUS ").is_empty());
}

#[test]
fn completes_the_word_under_the_cursor() {
    // Only what is before the cursor counts; the whole word is replaced.
    let completion = complete_at("HGE|TALL k");
    assert_eq!(completion.range, 0..7);
    assert_eq!(completion.candidates[0].name, "HGET");
    // In the spaces between words a new one starts.
    let completion = complete_at("SET k v | GET");
    assert_eq!(completion.range, 8..8);
    assert!(completion.candidates.iter().any(|c| c.name == "NX"));
    // Quoted words are values: nothing completes them.
    assert!(names(r#""GE"#).is_empty());
    assert!(names(r#"SET k v "N"#).is_empty());
    // A cursor past the end or inside a character is clamped.
    assert_eq!(complete("hgeta", 99).candidates[0].name, "HGETALL");
    let line = "ü hge";
    assert_eq!(complete(line, 1).range, 0..2);
    // An empty line: every command.
    assert!(complete("", 0).candidates.len() > 300);
}

#[test]
fn previews_and_accepts_candidates() {
    let line = "hge k";
    let completion = complete(line, 3);
    let hgetall = completion
        .candidates
        .iter()
        .position(|c| c.name == "HGETALL")
        .unwrap();
    assert_eq!(
        completion.preview(line, hgetall),
        Some(Edit {
            text: "hgetall k".into(),
            cursor: 7
        })
    );
    // Accepting moves past the space that is already there.
    assert_eq!(
        completion.accept(line, hgetall),
        Some(Edit {
            text: "hgetall k".into(),
            cursor: 8
        })
    );
    // At the end of the line it adds one.
    let completion = complete("hgeta", 5);
    assert_eq!(
        completion.accept("hgeta", 0),
        Some(Edit {
            text: "hgetall ".into(),
            cursor: 8
        })
    );
    assert_eq!(completion.preview("hgeta", 9), None);
    // A line changed since (shorter than the range): nothing.
    assert_eq!(completion.preview("h", 0), None);
}

#[test]
fn tab_cycles_and_comes_back() {
    let mut tab = TabCompletion::start("CONFIG RE", 9).unwrap();
    let names: Vec<&str> = tab.candidates().iter().map(|c| c.name).collect();
    assert_eq!(names, ["CONFIG RESETSTAT", "CONFIG REWRITE"]);
    assert_eq!(tab.selected(), None);
    assert!(tab.continues("CONFIG RE", 9));
    let first = tab.forward();
    assert_eq!(first.text, "CONFIG RESETSTAT");
    assert_eq!(first.cursor, first.text.len());
    assert_eq!(tab.selected(), Some(0));
    assert!(!tab.finished(), "more to cycle through");
    assert!(tab.continues("CONFIG RESETSTAT", 16));
    assert!(!tab.continues("CONFIG RESETSTATX", 16));
    // The same line with the cursor moved (to another word) starts over.
    assert!(!tab.continues("CONFIG RESETSTAT", 0));
    assert!(!tab.continues("CONFIG RESETSTAT", 3));
    assert_eq!(tab.forward().text, "CONFIG REWRITE");
    // Past the last: back to what was typed, cursor where it was.
    assert_eq!(
        tab.forward(),
        Edit {
            text: "CONFIG RE".into(),
            cursor: 9
        }
    );
    assert_eq!(tab.selected(), None);
    assert_eq!(tab.forward().text, "CONFIG RESETSTAT");
    // Shift-Tab goes the other way, through what was typed.
    assert_eq!(tab.backward().text, "CONFIG RE");
    assert_eq!(tab.backward().text, "CONFIG REWRITE");
    assert_eq!(
        tab.accept(0),
        Some(Edit {
            text: "CONFIG RESETSTAT ".into(),
            cursor: 17
        })
    );

    // A lone candidate is accepted at once, every time, and that ends the
    // cycle: the next Tab completes where the cursor went.
    let mut tab = TabCompletion::start("hgeta", 5).unwrap();
    assert!(!tab.finished());
    assert!(tab.continues("hgeta", 5));
    for _ in 0..2 {
        assert_eq!(
            tab.forward(),
            Edit {
                text: "hgetall ".into(),
                cursor: 8
            }
        );
        assert_eq!(tab.selected(), Some(0));
        assert!(tab.finished());
        assert!(!tab.continues("hgetall ", 8));
    }
    let mut tab = TabCompletion::start("hgeta", 5).unwrap();
    assert_eq!(tab.backward().text, "hgetall ");
    assert!(tab.finished());
    assert!(TabCompletion::start("SET ", 4).is_none());
    assert!(TabCompletion::start("qqqq", 4).is_none());
}

proptest! {
    /// Any line, any cursor: no panic, a range on character boundaries that
    /// covers the cursor, and edits that only change that range.
    #[test]
    fn any_line_any_cursor(line in "[A-Za-z .\"'ü]{0,16}", cursor in 0usize..20) {
        let completion = complete(&line, cursor);
        let range = &completion.range;
        prop_assert!(range.start <= range.end && range.end <= line.len());
        prop_assert!(line.is_char_boundary(range.start) && line.is_char_boundary(range.end));
        let cursor = cursor.min(line.len());
        prop_assert!(range.start <= cursor || !line.is_char_boundary(cursor));
        for ix in 0..completion.candidates.len().min(5) {
            let edit = completion.preview(&line, ix).unwrap();
            prop_assert!(edit.text.starts_with(&line[..range.start]));
            prop_assert!(edit.text.ends_with(&line[range.end..]));
            prop_assert!(edit.text.is_char_boundary(edit.cursor));
            let accepted = completion.accept(&line, ix).unwrap();
            prop_assert!(accepted.cursor > edit.cursor);
        }
    }

    /// Every candidate for a prefix starts with it or, from two letters on,
    /// holds its letters in order; prefix matches come first.
    #[test]
    fn candidates_match_what_was_typed(typed in "[a-zA-Z.]{1,4}") {
        let completion = complete(&typed, typed.len());
        let mut seen_fuzzy = false;
        for candidate in &completion.candidates {
            let name = candidate.text.to_ascii_lowercase();
            let lower = typed.to_ascii_lowercase();
            if candidate.fuzzy {
                seen_fuzzy = true;
                prop_assert!(typed.len() >= 2);
                prop_assert!(fuzzy_score(lower.as_bytes(), name.as_bytes()).is_some());
            } else {
                prop_assert!(!seen_fuzzy, "a prefix match after a fuzzy one");
                prop_assert!(name.starts_with(&lower));
            }
        }
    }
}
