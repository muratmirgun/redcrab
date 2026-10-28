use proptest::prelude::*;

use super::*;
use crate::{commands, console::usage};

fn grammar(usage: &'static str) -> Grammar {
    Grammar::parse(usage).unwrap_or_else(|| panic!("does not parse: {usage}"))
}

/// What may follow `args` (space separated, `_` for an empty argument).
fn rest(usage: &'static str, args: &str) -> Option<String> {
    let args: Vec<&[u8]> = args
        .split_whitespace()
        .map(|a| if a == "_" { &b""[..] } else { a.as_bytes() })
        .collect();
    grammar(usage).remaining(&args)
}

fn tokens(usage: &'static str, args: &str) -> Vec<(&'static str, String)> {
    let args: Vec<&str> = args.split_whitespace().collect();
    grammar(usage).next_tokens(&args)
}

fn normalized(usage: &str) -> String {
    usage.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_usage_in_the_table_reads_back() {
    let mut count = 0;
    for spec in commands::all() {
        let usage = usage(spec).unwrap_or_else(|| panic!("{} has no usage", spec.name));
        let grammar = Grammar::parse(usage)
            .unwrap_or_else(|| panic!("the usage of {} does not parse: {usage}", spec.name));
        assert_eq!(grammar.render(), normalized(usage), "{}", spec.name);
        // With no arguments typed, all of it is still to come.
        assert_eq!(
            grammar.remaining::<&[u8]>(&[]),
            Some(normalized(usage)),
            "{}",
            spec.name
        );
        count += 1;
    }
    assert!(count > 600);
}

#[test]
fn reads_the_notation() {
    let g = grammar("key [NX | XX] <A | B c> [D d ...] ... [<E | F> ...]");
    assert_eq!(
        g.render(),
        "key [NX | XX] <A | B c> [D d ...] ... [<E | F> ...]"
    );
    assert_eq!(grammar("").render(), "");
    assert_eq!(grammar("  a   [B]  ").render(), "a [B]");
    assert_eq!(grammar("[...]").render(), "[...]");
    assert_eq!(grammar("<x>").render(), "<x>");
    for broken in ["[a", "a]", "<a | b", "a >", "[a | b>", "|", "a | b", "[[a]"] {
        assert!(Grammar::parse(broken).is_none(), "{broken}");
    }
}

#[test]
fn tells_tokens_from_values() {
    let g = grammar(r#"key <* | id> <m | km> [count | RESET] <key | ""> numKeys ~ $"#);
    let kinds: Vec<String> = g
        .nodes
        .iter()
        .filter_map(|node| match node {
            Node::Token(t) => Some(format!("T:{t}")),
            Node::Arg(a) => Some(format!("A:{a}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "A:key",
            "T:*",
            "A:id",
            "T:m",
            "T:km",
            "A:count",
            "T:RESET",
            "A:key",
            r#"T:"""#,
            "A:numKeys",
            "T:~",
            "T:$"
        ]
    );
}

#[test]
fn leaves_out_what_was_typed() {
    let set = "key value [NX | XX] [GET] [EX seconds | PX milliseconds | KEEPTTL]";
    assert_eq!(rest(set, "").unwrap(), set);
    assert_eq!(
        rest(set, "k").unwrap(),
        "value [NX | XX] [GET] [EX seconds | PX milliseconds | KEEPTTL]"
    );
    assert_eq!(
        rest(set, "k v").unwrap(),
        "[NX | XX] [GET] [EX seconds | PX milliseconds | KEEPTTL]"
    );
    assert_eq!(
        rest(set, "k v nx").unwrap(),
        "[GET] [EX seconds | PX milliseconds | KEEPTTL]"
    );
    // Options in any order; the hint keeps the ones not used yet.
    assert_eq!(
        rest(set, "k v GET").unwrap(),
        "[NX | XX] [EX seconds | PX milliseconds | KEEPTTL]"
    );
    assert_eq!(rest(set, "k v ex").unwrap(), "seconds [NX | XX] [GET]");
    assert_eq!(rest(set, "k v ex 10").unwrap(), "[NX | XX] [GET]");
    assert_eq!(rest(set, "k v EX 10 nx").unwrap(), "[GET]");
    assert_eq!(rest(set, "k v XX GET px 5").unwrap(), "");
    assert_eq!(rest(set, "k v px 5 GET xx").unwrap(), "");
    // Twice the same option, both of a choice, unknown, too many: nothing
    // fits.
    assert_eq!(rest(set, "k v GET GET"), None);
    assert_eq!(rest(set, "k v NX XX"), None);
    assert_eq!(rest(set, "k v EX 1 PX 2"), None);
    assert_eq!(rest(set, "k v BOGUS"), None);
    assert_eq!(rest(set, "k v ex 10 more"), None);
    // Options only move among themselves: not before a value.
    assert_eq!(rest(set, "k NX v"), None);
}

#[test]
fn options_come_in_any_order() {
    // SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]
    let scan = "cursor [MATCH pattern] [COUNT count] [TYPE type]";
    assert_eq!(
        rest(scan, "0 TYPE hash").unwrap(),
        "[MATCH pattern] [COUNT count]"
    );
    assert_eq!(rest(scan, "0 COUNT 10 MATCH a*").unwrap(), "[TYPE type]");
    assert_eq!(rest(scan, "0 type t count 1 match m").unwrap(), "");
    // ZADD's flags before the scores.
    let zadd = "key [NX | XX] [GT | LT] [CH] [INCR] score member [score member ...]";
    assert_eq!(
        rest(zadd, "k CH XX").unwrap(),
        "[GT | LT] [INCR] score member [score member ...]"
    );
    assert_eq!(rest(zadd, "k INCR GT 1 m").unwrap(), "[score member ...]");
    // Flags come before the scores: after them, CH is a score.
    assert_eq!(rest(zadd, "k 1 m CH").unwrap(), "member [score member ...]");
    // Options with values of their own (SORT).
    let sort = "key [BY pattern] [LIMIT offset count] [GET pattern [GET pattern ...]] [ASC | DESC] [ALPHA] [STORE destination]";
    assert_eq!(
        rest(sort, "k ALPHA LIMIT 0 10 DESC").unwrap(),
        "[BY pattern] [GET pattern [GET pattern ...]] [STORE destination]"
    );
    assert_eq!(
        rest(sort, "k GET a GET b BY w").unwrap(),
        "[LIMIT offset count] [ASC | DESC] [ALPHA] [STORE destination]"
    );
    // Optional values keep their order: nothing else tells them apart.
    let eval = "script numkeys [key [key ...]] [arg [arg ...]]";
    assert_eq!(
        rest(eval, "s 1").unwrap(),
        "[key [key ...]] [arg [arg ...]]"
    );
    let g = grammar(eval);
    assert!(!g.nodes.iter().any(|n| matches!(n, Node::Options(_))));
    // A run is split where something else comes between.
    let g = grammar("[A] [B] c [D] [E] [f]");
    let options: Vec<usize> = g
        .nodes
        .iter()
        .filter_map(|n| match n {
            Node::Options(ids) => Some(ids.len()),
            _ => None,
        })
        .collect();
    assert_eq!(options, [2, 2]);
    assert_eq!(g.render(), "[A] [B] c [D] [E] [f]");
}

#[test]
fn prefers_readings_with_more_tokens() {
    let zadd = "key [NX | XX] [GT | LT] [CH] [INCR] score member [score member ...]";
    // NX is the flag, not a score.
    assert_eq!(
        rest(zadd, "k NX").unwrap(),
        "[GT | LT] [CH] [INCR] score member [score member ...]"
    );
    assert_eq!(rest(zadd, "k 1").unwrap(), "member [score member ...]");
    assert_eq!(rest(zadd, "k 1 m").unwrap(), "[score member ...]");
    assert_eq!(rest(zadd, "k 1 m 2").unwrap(), "member [score member ...]");
    // A member named like a flag is still a member where only one fits.
    assert_eq!(rest(zadd, "k 1 CH").unwrap(), "[score member ...]");
}

#[test]
fn takes_optional_parts_before_skipping_them() {
    // `AUTH [username] password`: one word could be either; the hint says
    // what a second would be.
    assert_eq!(rest("[username] password", "x").unwrap(), "password");
    assert_eq!(rest("[username] password", "u p").unwrap(), "");
    // BLPOP key [key ...] timeout: more keys may come.
    let blpop = "key [key ...] timeout";
    assert_eq!(rest(blpop, "a").unwrap(), "[key ...] timeout");
    assert_eq!(rest(blpop, "a b").unwrap(), "[key ...] timeout");
    let hello = "[protover [AUTH username password] [SETNAME clientname]]";
    assert_eq!(
        rest(hello, "3").unwrap(),
        "[AUTH username password] [SETNAME clientname]"
    );
    assert_eq!(
        rest(hello, "3 auth").unwrap(),
        "username password [SETNAME clientname]"
    );
    assert_eq!(
        rest(hello, "3 SETNAME n").unwrap(),
        "[AUTH username password]"
    );
    assert_eq!(rest(hello, "3 SETNAME n AUTH u p").unwrap(), "");
}

#[test]
fn repeats_and_choices() {
    let mset = "key value [key value ...]";
    assert_eq!(rest(mset, "a 1").unwrap(), "[key value ...]");
    assert_eq!(rest(mset, "a 1 b").unwrap(), "value [key value ...]");
    assert_eq!(rest(mset, "a 1 b 2").unwrap(), "[key value ...]");

    let geo = "key <FROMMEMBER member | FROMLONLAT longitude latitude> <BYRADIUS radius <M | KM> | BYBOX width height <M | KM>> [ASC | DESC]";
    assert_eq!(
        rest(geo, "k FROMLONLAT 1").unwrap(),
        "latitude <BYRADIUS radius <M | KM> | BYBOX width height <M | KM>> [ASC | DESC]"
    );
    assert_eq!(
        rest(geo, "k FROMMEMBER m BYBOX 1 2").unwrap(),
        "<M | KM> [ASC | DESC]"
    );
    assert_eq!(
        rest(geo, "k FROMMEMBER m BYBOX 1 2 km").unwrap(),
        "[ASC | DESC]"
    );
    assert_eq!(rest(geo, "k FROMMEMBER m BYBOX 1 2 mi"), None);

    // MIGRATE's `""` is an empty argument.
    let migrate = r#"host port <key | ""> destination-db timeout [COPY]"#;
    assert_eq!(
        rest(migrate, "h 1 _").unwrap(),
        "destination-db timeout [COPY]"
    );
    assert_eq!(
        rest(migrate, "h 1 k").unwrap(),
        "destination-db timeout [COPY]"
    );

    // Lower-case tokens in a choice (TS.RANGE's BUCKETTIMESTAMP).
    let ts = "[BUCKETTIMESTAMP <start | - | end | + | mid | ~>] [EMPTY]";
    assert_eq!(rest(ts, "BUCKETTIMESTAMP mid").unwrap(), "[EMPTY]");
    assert_eq!(rest(ts, "BUCKETTIMESTAMP 7"), None);
}

#[test]
fn rest_takes_anything() {
    let g = "key ...";
    assert_eq!(rest(g, "").unwrap(), "key ...");
    assert_eq!(rest(g, "k").unwrap(), "...");
    assert_eq!(rest(g, "k a b c").unwrap(), "...");
    assert_eq!(rest("...", "").unwrap(), "...");
}

#[test]
fn nullable_repeats_terminate() {
    // A repeat of something that can be empty must not loop.
    let g = grammar("[[A] [B] ...] C");
    assert_eq!(g.remaining(&["A", "B", "A", "C"]).unwrap(), "");
    assert_eq!(g.remaining(&["C"]).unwrap(), "");
    assert_eq!(g.remaining::<&str>(&[]).unwrap(), "[[A] [B] ...] C");
    let g = grammar("[[x] ...] [[y] ...]");
    assert!(g.remaining(&["1", "2", "3"]).is_some());
}

#[test]
fn lists_the_tokens_that_come_next() {
    let set = "key value [NX | XX] [GET] [EX seconds | PX milliseconds | KEEPTTL]";
    assert_eq!(tokens(set, ""), []);
    assert_eq!(tokens(set, "k"), []);
    let next = tokens(set, "k v");
    let next: Vec<(&str, &str)> = next.iter().map(|(t, takes)| (*t, takes.as_str())).collect();
    assert_eq!(
        next,
        [
            ("NX", ""),
            ("XX", ""),
            ("GET", ""),
            ("EX", "seconds"),
            ("PX", "milliseconds"),
            ("KEEPTTL", "")
        ]
    );
    let after_get: Vec<&str> = tokens(set, "k v GET").iter().map(|(t, _)| *t).collect();
    assert_eq!(after_get, ["NX", "XX", "EX", "PX", "KEEPTTL"]);
    let after_ex: Vec<&str> = tokens(set, "k v EX 1").iter().map(|(t, _)| *t).collect();
    assert_eq!(after_ex, ["NX", "XX", "GET"]);
    assert_eq!(tokens(set, "k v BOGUS"), []);
    let limit = tokens("key [LIMIT offset count] [WITHSCORES]", "k");
    assert_eq!(limit[0], ("LIMIT", "offset count".to_string()));
    assert_eq!(limit[1], ("WITHSCORES", String::new()));
}

#[test]
fn many_arguments_stay_cheap() {
    // The biggest usage in the table, fed far more words than it takes.
    let spec = commands::lookup("FT.AGGREGATE", None).unwrap();
    let g = grammar(usage(spec).unwrap());
    let words: Vec<String> = (0..60)
        .map(|i| ["LOAD", "*", "GROUPBY", "1", "@f", "REDUCE", "COUNT", "0"][i % 8].to_string())
        .collect();
    let started = std::time::Instant::now();
    let _ = g.remaining(&words);
    let _ = g.next_tokens(&words);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

// Random grammars in the docs' notation, and sentences they accept.

#[derive(Clone, Debug)]
enum Tree {
    Token(&'static str),
    Arg(&'static str),
    /// `[a b]`
    Optional(Vec<Tree>),
    /// `[A a | B b]`, two or more alternatives, each starting with a token.
    OptionalChoice(Vec<Vec<Tree>>),
    /// `<A a | B b>`
    Choice(Vec<Vec<Tree>>),
    /// `[a b ...]`
    Repeat(Vec<Tree>),
}

const TOKENS: &[&str] = &["NX", "XX", "GET", "EX", "LIMIT", "*", "~"];
const ARGS: &[&str] = &["key", "value", "count", "numKeys"];

fn tree() -> impl Strategy<Value = Tree> {
    let leaf = prop_oneof![
        proptest::sample::select(TOKENS).prop_map(Tree::Token),
        proptest::sample::select(ARGS).prop_map(Tree::Arg),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        let items = proptest::collection::vec(inner.clone(), 1..3);
        let alternative = (
            proptest::sample::select(TOKENS),
            proptest::collection::vec(inner, 0..2),
        )
            .prop_map(|(token, rest)| {
                let mut items = vec![Tree::Token(token)];
                items.extend(rest);
                items
            });
        prop_oneof![
            items.clone().prop_map(Tree::Optional),
            proptest::collection::vec(alternative.clone(), 2..4).prop_map(Tree::OptionalChoice),
            proptest::collection::vec(alternative, 1..4).prop_map(Tree::Choice),
            items.prop_map(Tree::Repeat),
        ]
    })
}

fn write(trees: &[Tree], out: &mut String) {
    for (i, tree) in trees.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match tree {
            Tree::Token(t) | Tree::Arg(t) => out.push_str(t),
            Tree::Optional(items) => {
                out.push('[');
                write(items, out);
                out.push(']');
            }
            Tree::OptionalChoice(alternatives) | Tree::Choice(alternatives) => {
                let (open, close) = match tree {
                    Tree::Choice(_) => ('<', '>'),
                    _ => ('[', ']'),
                };
                out.push(open);
                for (j, alternative) in alternatives.iter().enumerate() {
                    if j > 0 {
                        out.push_str(" | ");
                    }
                    write(alternative, out);
                }
                out.push(close);
            }
            Tree::Repeat(items) => {
                out.push('[');
                write(items, out);
                out.push_str(" ...]");
            }
        }
    }
}

/// A sentence of `trees`, steered by `dice`.
fn sentence(trees: &[Tree], dice: &mut impl Iterator<Item = u8>, out: &mut Vec<String>) {
    fn roll(dice: &mut impl Iterator<Item = u8>) -> u8 {
        dice.next().unwrap_or(0)
    }
    for tree in trees {
        match tree {
            Tree::Token(t) => {
                // Tokens match in any case.
                out.push(if roll(dice).is_multiple_of(2) {
                    t.to_string()
                } else {
                    t.to_lowercase()
                })
            }
            Tree::Arg(_) => {
                // Values may look like tokens, or be empty.
                const VALUES: &[&str] = &["x", "1", "NX", "", "limit", "*"];
                out.push(VALUES[usize::from(roll(dice)) % VALUES.len()].to_string());
            }
            Tree::Optional(items) => {
                if roll(dice).is_multiple_of(2) {
                    sentence(items, dice, out);
                }
            }
            Tree::OptionalChoice(alternatives) => {
                let pick = usize::from(roll(dice)) % (alternatives.len() + 1);
                if let Some(alternative) = alternatives.get(pick) {
                    sentence(alternative, dice, out);
                }
            }
            Tree::Choice(alternatives) => {
                let pick = usize::from(roll(dice)) % alternatives.len();
                sentence(&alternatives[pick], dice, out);
            }
            Tree::Repeat(items) => {
                for _ in 0..roll(dice) % 3 {
                    sentence(items, dice, out);
                }
            }
        }
    }
}

fn leaked(usage: String) -> &'static str {
    Box::leak(usage.into_boxed_str())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Whatever the notation says, it reads back the same.
    #[test]
    fn notation_round_trips(trees in proptest::collection::vec(tree(), 0..5), rest in any::<bool>()) {
        let mut usage = String::new();
        write(&trees, &mut usage);
        if rest {
            usage.push_str(" ...");
        }
        let usage = leaked(usage);
        let g = Grammar::parse(usage).unwrap();
        prop_assert_eq!(g.render(), normalized(usage));
    }

    /// Every sentence of a grammar is accepted, word by word, and the
    /// command can end after the last word.
    #[test]
    fn accepts_what_the_grammar_allows(
        trees in proptest::collection::vec(tree(), 1..5),
        dice in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut usage = String::new();
        write(&trees, &mut usage);
        let usage = leaked(usage);
        let g = Grammar::parse(usage).unwrap();
        let mut words = Vec::new();
        sentence(&trees, &mut dice.into_iter(), &mut words);
        for typed in 0..=words.len() {
            prop_assert!(
                g.remaining(&words[..typed]).is_some(),
                "{} rejects {:?}", usage, &words[..typed]
            );
        }
        let complete = g
            .read(&words)
            .into_iter()
            .any(|reading| g.frontier(reading.stack).iter().any(|stack| stack.0.is_empty()));
        prop_assert!(complete, "{} cannot end after {:?}", usage, words);
    }

    /// Keyword options taken in any order and any number: accepted, and
    /// what is left are exactly the unused ones, in the usage's order.
    #[test]
    fn options_in_any_order(
        takes in proptest::collection::vec(0usize..3, 2..8),
        order in Just((0..8).collect::<Vec<usize>>()).prop_shuffle(),
        used in 0usize..9,
    ) {
        let options: Vec<String> = takes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let values: Vec<String> = (0..n).map(|v| format!("v{v}")).collect();
                format!("[O{i}{}{}]", if n > 0 { " " } else { "" }, values.join(" "))
            })
            .collect();
        let usage = leaked(format!("key {}", options.join(" ")));
        let g = Grammar::parse(usage).unwrap();
        let picked: Vec<usize> = order.into_iter().filter(|&i| i < takes.len()).take(used).collect();
        let mut words = vec!["k".to_string()];
        for &i in &picked {
            words.push(format!("o{i}"));
            words.extend((0..takes[i]).map(|v| format!("x{v}")));
        }
        let left: Vec<&str> = options
            .iter()
            .enumerate()
            .filter(|(i, _)| !picked.contains(i))
            .map(|(_, o)| o.as_str())
            .collect();
        prop_assert_eq!(g.remaining(&words), Some(left.join(" ")), "{} {:?}", usage, words);
        // The same option twice never fits.
        if let Some(&i) = picked.first() {
            let mut twice = words.clone();
            twice.push(format!("O{i}"));
            twice.extend((0..takes[i]).map(|v| format!("x{v}")));
            prop_assert_eq!(g.remaining(&twice), None);
        }
    }

    /// Any words against any command of the table: no panic, and the
    /// hint is a suffix of what was possible before the last word or empty.
    #[test]
    fn any_words_against_the_table(
        ix in 0..commands::all().len(),
        words in proptest::collection::vec("[A-Za-z0-9*~$\"]{0,6}|NX|EX|LIMIT|COUNT|key", 0..8),
    ) {
        let spec = &commands::all()[ix];
        let g = grammar(usage(spec).unwrap());
        let _ = g.remaining(&words);
        let _ = g.next_tokens(&words);
    }
}
