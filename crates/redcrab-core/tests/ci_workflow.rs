//! The CI workflow's `concurrency` settings, checked against the rules GitHub
//! applies to them: a newer push to a pull request or branch cancels the run
//! it supersedes, and every push to main gets a complete run.
//!
//! `cancel-in-progress: false` alone does not give main the latter. It only
//! spares the run in progress: a group holds at most one pending run (the
//! default `queue: single`), and queueing a new one cancels the pending one.
//! With one group for all of main, a push that lands while an earlier run is
//! still going is dropped as soon as the next push arrives, so it never gets
//! a run of its own.
//!
//! The workspace has no root package, so this lives in core, the crate every
//! CI job tests. It reads the workflow as text and evaluates its expressions
//! with a small evaluator for the part of GitHub's expression syntax they use
//! (anything else fails the test, rather than being guessed at), so it needs
//! no YAML or expression crate. No Redis needed.

use std::{collections::HashMap, fs, path::Path};

/// The raw values of the workflow's top-level `concurrency:` block.
struct Concurrency {
    group: String,
    cancel_in_progress: Option<String>,
    queue: Option<String>,
}

fn workflow_concurrency() -> Concurrency {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading the CI workflow {}: {e}", path.display()));
    // A job-level block would add a second group per job, which the model
    // below does not cover.
    let blocks = text
        .lines()
        .filter(|line| line.trim_start().starts_with("concurrency:"))
        .count();
    assert_eq!(blocks, 1, "the workflow has one `concurrency:` block");

    // `lines` drops the `\r` of a Windows checkout too.
    let mut lines = text.lines();
    lines.position(|line| line == "concurrency:").expect(
        "`concurrency:` is a top-level block (not the one-line `concurrency: <group>` form)",
    );
    let mut group = None;
    let mut cancel_in_progress = None;
    let mut queue = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            break;
        }
        let (key, value) = trimmed
            .split_once(':')
            .unwrap_or_else(|| panic!("a `key: value` line in `concurrency:`, got {trimmed:?}"));
        let value = yaml_scalar(value);
        match key {
            "group" => group = Some(value),
            "cancel-in-progress" => cancel_in_progress = Some(value),
            "queue" => queue = Some(value),
            other => panic!("`concurrency.{other}` is not modelled by this test"),
        }
    }
    Concurrency {
        group: group.expect("`concurrency:` sets a group"),
        cancel_in_progress,
        queue,
    }
}

/// A plain or quoted one-line YAML scalar, without a trailing comment.
fn yaml_scalar(raw: &str) -> String {
    let value = raw.split(" #").next().unwrap_or_default().trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            assert!(
                !inner.contains(quote) && !inner.contains('\\'),
                "escapes in {value} are not modelled by this test"
            );
            return inner.to_owned();
        }
    }
    value.to_owned()
}

/// An expression value, for the types the workflow's expressions produce.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Str(String),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Str(s) => !s.is_empty(),
        }
    }

    /// How GitHub writes a value into a string around it.
    fn into_string(self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => s,
        }
    }
}

/// `==` as GitHub has it: strings compare ignoring case.
fn equals(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Str(a), Value::Str(b)) => a.eq_ignore_ascii_case(b),
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        _ => panic!(
            "comparing {left:?} with {right:?} needs GitHub's number coercion, which this test does not model"
        ),
    }
}

/// The `github` context of one workflow run, by dotted path.
type Context = HashMap<&'static str, String>;

/// A workflow value: one expression keeps its type, anything else is a
/// string with each expression's value written into it.
fn evaluate(value: &str, ctx: &Context) -> Value {
    if let Some(inner) = value
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        && !inner.contains("${{")
    {
        return expression(inner, ctx);
    }
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let end = after
            .find("}}")
            .unwrap_or_else(|| panic!("an unterminated expression in {value:?}"));
        out.push_str(&expression(&after[..end], ctx).into_string());
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Value::Str(out)
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Str(String),
    /// A context path such as `github.ref`, or `true`, `false`, `null`.
    Word(String),
    Op(&'static str),
}

fn tokens(source: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = source.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '\'' {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    // `''` is an escaped quote.
                    Some('\'') if chars.peek() == Some(&'\'') => {
                        chars.next();
                        s.push('\'');
                    }
                    Some('\'') => break,
                    Some(c) => s.push(c),
                    None => panic!("an unterminated string in {source:?}"),
                }
            }
            out.push(Token::Str(s));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
                    break;
                }
                word.push(c);
                chars.next();
            }
            out.push(Token::Word(word));
        } else {
            chars.next();
            let op = match (c, chars.peek()) {
                ('=', Some('=')) => "==",
                ('!', Some('=')) => "!=",
                ('&', Some('&')) => "&&",
                ('|', Some('|')) => "||",
                ('!', _) => "!",
                ('(', _) => "(",
                (')', _) => ")",
                _ => panic!("{c:?} in {source:?} is not modelled by this test"),
            };
            if op.len() == 2 {
                chars.next();
            }
            out.push(Token::Op(op));
        }
    }
    out
}

/// Evaluates one expression: literals, `github` context paths, `!`, `==`,
/// `!=`, `&&`, `||` and parentheses, with GitHub's precedence and its
/// value-returning `&&` and `||`.
fn expression(source: &str, ctx: &Context) -> Value {
    let mut parser = Parser {
        tokens: tokens(source),
        pos: 0,
        ctx,
    };
    let value = parser.or();
    assert_eq!(
        parser.pos,
        parser.tokens.len(),
        "trailing tokens in {source:?}"
    );
    value
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    ctx: &'a Context,
}

impl Parser<'_> {
    fn eat(&mut self, op: &str) -> bool {
        let found = matches!(self.tokens.get(self.pos), Some(Token::Op(o)) if *o == op);
        if found {
            self.pos += 1;
        }
        found
    }

    fn or(&mut self) -> Value {
        let mut left = self.and();
        while self.eat("||") {
            let right = self.and();
            left = if left.truthy() { left } else { right };
        }
        left
    }

    fn and(&mut self) -> Value {
        let mut left = self.comparison();
        while self.eat("&&") {
            let right = self.comparison();
            left = if left.truthy() { right } else { left };
        }
        left
    }

    fn comparison(&mut self) -> Value {
        let mut left = self.unary();
        loop {
            if self.eat("==") {
                let right = self.unary();
                left = Value::Bool(equals(&left, &right));
            } else if self.eat("!=") {
                let right = self.unary();
                left = Value::Bool(!equals(&left, &right));
            } else {
                return left;
            }
        }
    }

    fn unary(&mut self) -> Value {
        if self.eat("!") {
            Value::Bool(!self.unary().truthy())
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> Value {
        if self.eat("(") {
            let value = self.or();
            assert!(self.eat(")"), "an unclosed parenthesis");
            return value;
        }
        let token = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        match token {
            Some(Token::Str(s)) => Value::Str(s),
            Some(Token::Word(word)) => {
                assert!(
                    !matches!(self.tokens.get(self.pos), Some(Token::Op("("))),
                    "the function {word}() is not modelled by this test"
                );
                match word.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    path => Value::Str(
                        self.ctx
                            .get(path)
                            .unwrap_or_else(|| {
                                panic!("{path} is not in the modelled context; add it to `Event::context`")
                            })
                            .clone(),
                    ),
                }
            }
            other => panic!("expected a value, got {other:?}"),
        }
    }
}

/// The parts of a workflow run's trigger that its `github` context exposes.
struct Event {
    name: &'static str,
    git_ref: String,
    sha: &'static str,
    /// Set for pull requests only.
    head_ref: &'static str,
    run_id: u64,
}

fn push_to_main(sha: &'static str, run_id: u64) -> Event {
    Event {
        name: "push",
        git_ref: "refs/heads/main".into(),
        sha,
        head_ref: "",
        run_id,
    }
}

fn pull_request(number: u32, sha: &'static str, run_id: u64) -> Event {
    Event {
        name: "pull_request",
        git_ref: format!("refs/pull/{number}/merge"),
        sha,
        head_ref: "topic",
        run_id,
    }
}

fn dispatch_on_branch(sha: &'static str, run_id: u64) -> Event {
    Event {
        name: "workflow_dispatch",
        git_ref: "refs/heads/topic".into(),
        sha,
        head_ref: "",
        run_id,
    }
}

impl Event {
    fn context(&self) -> Context {
        let ref_name = self
            .git_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&self.git_ref);
        HashMap::from([
            ("github.workflow", "CI".to_owned()),
            ("github.event_name", self.name.to_owned()),
            ("github.ref", self.git_ref.clone()),
            ("github.ref_name", ref_name.to_owned()),
            ("github.sha", self.sha.to_owned()),
            ("github.head_ref", self.head_ref.to_owned()),
            ("github.run_id", self.run_id.to_string()),
        ])
    }
}

/// A run's concurrency settings, evaluated for its event.
#[derive(Debug)]
struct Run {
    group: String,
    cancel_in_progress: bool,
    /// `queue: max`: every run waits its turn instead of one pending run.
    queue_max: bool,
}

fn run(c: &Concurrency, event: &Event) -> Run {
    let ctx = event.context();
    let group = evaluate(&c.group, &ctx).into_string();
    assert!(!group.is_empty(), "an empty concurrency group");
    let cancel_in_progress = match c.cancel_in_progress.as_deref() {
        None => false,
        Some(value) => match evaluate(value, &ctx) {
            Value::Bool(b) => b,
            // A plain YAML `true` or `false`.
            Value::Str(s) if s == "true" || s == "false" => s == "true",
            other => panic!("cancel-in-progress is {other:?}, not a boolean"),
        },
    };
    let queue_max = match c.queue.as_deref().map(|q| evaluate(q, &ctx).into_string()) {
        None => false,
        Some(q) if q == "single" => false,
        Some(q) if q == "max" => true,
        Some(other) => panic!("`queue: {other}` is not a queue GitHub knows"),
    };
    assert!(
        !(queue_max && cancel_in_progress),
        "GitHub rejects `queue: max` together with `cancel-in-progress: true` (group {group})"
    );
    Run {
        group,
        cancel_in_progress,
        queue_max,
    }
}

#[derive(Clone, Copy, Debug)]
enum State {
    InProgress,
    Pending,
}

/// Whether queueing `later` cancels `earlier`, which is in `state`.
fn cancels(earlier: &Run, state: State, later: &Run) -> bool {
    // Group names are case-insensitive.
    if !earlier.group.eq_ignore_ascii_case(&later.group) {
        return false;
    }
    match state {
        State::InProgress => later.cancel_in_progress,
        // Unless the group queues every run, it holds one pending run, and the
        // newest one replaces it whatever `cancel-in-progress` says.
        State::Pending => later.cancel_in_progress || !later.queue_max,
    }
}

const STATES: [State; 2] = [State::InProgress, State::Pending];

#[test]
fn every_push_to_main_gets_a_complete_run() {
    let c = workflow_concurrency();
    let pushes = [
        push_to_main("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1", 1),
        push_to_main("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2", 2),
        push_to_main("ccccccccccccccccccccccccccccccccccccccc3", 3),
    ];
    let runs: Vec<Run> = pushes.iter().map(|push| run(&c, push)).collect();
    for (i, earlier) in runs.iter().enumerate() {
        for later in &runs[i + 1..] {
            for state in STATES {
                assert!(
                    !cancels(earlier, state, later),
                    "a push to main cancels the {state:?} run of an earlier one: {earlier:?} then {later:?}"
                );
            }
        }
    }
}

#[test]
fn a_newer_push_cancels_the_run_it_supersedes_off_main() {
    let c = workflow_concurrency();
    let superseded = [
        (
            pull_request(7, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1", 1),
            pull_request(7, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2", 2),
        ),
        (
            dispatch_on_branch("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1", 3),
            dispatch_on_branch("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2", 4),
        ),
    ];
    for (earlier, later) in &superseded {
        let (earlier, later) = (run(&c, earlier), run(&c, later));
        for state in STATES {
            assert!(
                cancels(&earlier, state, &later),
                "a newer push leaves the {state:?} run it supersedes going: {earlier:?} then {later:?}"
            );
        }
    }
}

#[test]
fn runs_for_different_refs_never_cancel_each_other() {
    let c = workflow_concurrency();
    let events = [
        push_to_main("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1", 1),
        pull_request(7, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2", 2),
        pull_request(8, "ccccccccccccccccccccccccccccccccccccccc3", 3),
        dispatch_on_branch("ddddddddddddddddddddddddddddddddddddddd4", 4),
    ];
    let runs: Vec<Run> = events.iter().map(|event| run(&c, event)).collect();
    for (i, earlier) in runs.iter().enumerate() {
        for (j, later) in runs.iter().enumerate() {
            for state in STATES {
                assert!(
                    i == j || !cancels(earlier, state, later),
                    "a run for one ref cancels the {state:?} run for another: {earlier:?} then {later:?}"
                );
            }
        }
    }
}

/// The evaluator itself, on the GitHub behaviours the checks above rely on.
#[test]
fn the_expression_evaluator_follows_github() {
    let ctx = push_to_main("abc", 1).context();
    let eval = |source: &str| expression(source, &ctx);
    let s = |s: &str| Value::Str(s.to_owned());

    // `&&` and `||` return one of their operands, not a boolean.
    assert_eq!(eval("'x' && 'y'"), s("y"));
    assert_eq!(eval("'' && 'y'"), s(""));
    assert_eq!(eval("'' || 'z'"), s("z"));
    assert_eq!(eval("'x' || 'z'"), s("x"));
    assert_eq!(eval("false && 'y'"), Value::Bool(false));
    // `==` binds tighter than `&&`, which binds tighter than `||`.
    assert_eq!(
        eval("github.ref == 'refs/heads/main' && github.sha || github.ref"),
        s("abc")
    );
    assert_eq!(
        eval("github.ref != 'refs/heads/main' && github.sha || github.ref"),
        s("refs/heads/main")
    );
    assert_eq!(eval("true || false && false"), Value::Bool(true));
    assert_eq!(eval("!(true || false) && false"), Value::Bool(false));
    // Strings compare ignoring case; `''` escapes a quote.
    assert_eq!(eval("github.ref == 'REFS/HEADS/MAIN'"), Value::Bool(true));
    assert_eq!(eval("'it''s'"), s("it's"));
    assert_eq!(eval("!''"), Value::Bool(true));

    // One expression keeps its type; with text around it, it is a string.
    assert_eq!(
        evaluate("${{ github.ref != 'refs/heads/main' }}", &ctx),
        Value::Bool(false)
    );
    assert_eq!(
        evaluate("${{ github.workflow }}-${{ github.ref == 'x' }}", &ctx),
        s("CI-false")
    );
    assert_eq!(
        yaml_scalar(" 'CI-${{ github.ref }}' # per ref"),
        "CI-${{ github.ref }}"
    );
}
