//! A command's arguments, read back from the one-line usage in the command
//! table (`SET key value [NX | XX] [GET] ...`), so that the console can tell
//! what may come after what was typed.
//!
//! The usage is written in the notation of the Redis docs, which
//! `examples/gen_commands.rs` builds from `COMMAND DOCS`:
//!
//! - `TOKEN` is typed as it is, in any case; `""` stands for an empty
//!   argument. Words with lower-case letters (`key`, `numKeys`) are values.
//! - `[x]` is optional, `[x ...]` repeats `x` any number of times.
//! - `<a | b>` is a choice, `[a | b]` an optional one.
//! - `...` on its own (commands without argument docs) takes anything.
//!
//! Matching typed arguments walks every way they can be read at once (an
//! NFA over the grammar), preferring readings that match more tokens: in
//! `ZADD key NX`, NX is the flag, not a score.
//!
//! The docs list options in one order, but servers read keyword options in
//! any order (`SET k v EX 10 NX` works), so a run of optional parts that
//! each start with a token matches in any order, each part at most once, as
//! redis-cli's hints do.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
};

/// Index of a [`Node`] in [`Grammar::nodes`].
type Id = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    /// Typed as it is, in any case.
    Token(&'static str),
    /// A value of the user's choosing.
    Arg(&'static str),
    /// `...` on its own: any arguments.
    Rest,
    Seq(Vec<Id>),
    /// `[x]`
    Optional(Id),
    /// `[x ...]`: `x` any number of times, none included.
    Repeat(Id),
    /// `<a | b>`, or `[a | b]` inside an [`Node::Optional`].
    OneOf(Vec<Id>),
    /// Optional parts, each starting with a token, that may come in any
    /// order, each at most once: `[NX | XX] [GET] [EX seconds]`. Holds the
    /// [`Node::Optional`]s (at most [`MAX_OPTIONS`]).
    Options(Vec<Id>),
}

/// The most optional parts one [`Node::Options`] holds: which ones were used
/// is a bit set.
const MAX_OPTIONS: usize = 64;

/// The arguments of one command.
#[derive(Clone, Debug)]
pub(crate) struct Grammar {
    nodes: Vec<Node>,
    root: Id,
}

/// Where a reading of the typed arguments stands: what is still to come,
/// next on top.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Stack(Vec<Frame>);

/// A node still to come; for [`Node::Options`], which of them were used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Frame {
    id: Id,
    used: u64,
}

impl Frame {
    fn new(id: Id) -> Self {
        Self { id, used: 0 }
    }
}

/// A reading: what is still to come, and how many typed arguments matched
/// a token (more is a better reading).
#[derive(Clone, Debug)]
struct Reading {
    stack: Stack,
    tokens: usize,
}

/// More readings than this at once are dropped (the least preferred ones),
/// so a pathological usage cannot make typing slow.
const MAX_READINGS: usize = 256;
/// The most expansion steps one [`Grammar::frontier`] takes.
const MAX_STEPS: usize = 20_000;

impl Grammar {
    /// Parses the usage of a command without its name (`key value [NX |
    /// XX]`). `None` when the brackets do not balance.
    pub(crate) fn parse(usage: &'static str) -> Option<Self> {
        let lexemes = lex(usage);
        let mut parser = Parser {
            lexemes: &lexemes,
            pos: 0,
            nodes: Vec::new(),
        };
        let items = parser.seq()?;
        if parser.pos != lexemes.len() {
            return None;
        }
        let root = parser.push(Node::Seq(items));
        let mut grammar = Self {
            nodes: parser.nodes,
            root,
        };
        grammar.find_options();
        Some(grammar)
    }

    /// Turns every run of two or more optional parts that start with a
    /// token into a [`Node::Options`]. Optional values (`[key [key ...]]`
    /// in EVAL) keep their order: nothing but their place tells them apart.
    fn find_options(&mut self) {
        for seq in 0..self.nodes.len() {
            let Node::Seq(items) = &self.nodes[seq] else {
                continue;
            };
            let items = items.clone();
            let mut grouped = Vec::with_capacity(items.len());
            let mut run: Vec<Id> = Vec::new();
            for item in items.into_iter().map(Some).chain([None]) {
                let option = item.is_some_and(|item| self.is_option(item));
                if let Some(item) = item.filter(|_| option && run.len() < MAX_OPTIONS) {
                    run.push(item);
                    continue;
                }
                match run.len() {
                    0 => {}
                    1 => grouped.push(run[0]),
                    _ => {
                        self.nodes.push(Node::Options(std::mem::take(&mut run)));
                        grouped.push(self.nodes.len() - 1);
                    }
                }
                run.clear();
                match item {
                    Some(item) if option => run.push(item),
                    Some(item) => grouped.push(item),
                    None => {}
                }
            }
            self.nodes[seq] = Node::Seq(grouped);
        }
    }

    /// An optional part that starts with a token and takes at least one.
    fn is_option(&self, id: Id) -> bool {
        matches!(self.nodes[id], Node::Optional(inner)
            if self.starts_with_token(inner) && !self.nullable(inner))
    }

    /// Whether `id` can match no words at all.
    fn nullable(&self, id: Id) -> bool {
        match &self.nodes[id] {
            Node::Token(_) | Node::Arg(_) => false,
            Node::Rest | Node::Optional(_) | Node::Repeat(_) | Node::Options(_) => true,
            Node::Seq(items) => items.iter().all(|&item| self.nullable(item)),
            Node::OneOf(alternatives) => alternatives.iter().any(|&alt| self.nullable(alt)),
        }
    }

    /// Whether every word that can start `id` is a token.
    fn starts_with_token(&self, id: Id) -> bool {
        match &self.nodes[id] {
            Node::Token(_) => true,
            Node::Arg(_) | Node::Rest => false,
            Node::Seq(items) => {
                for &item in items {
                    if !self.starts_with_token(item) {
                        return false;
                    }
                    if !self.nullable(item) {
                        return true;
                    }
                }
                true
            }
            &Node::Optional(inner) | &Node::Repeat(inner) => self.starts_with_token(inner),
            Node::OneOf(ids) | Node::Options(ids) => {
                ids.iter().all(|&id| self.starts_with_token(id))
            }
        }
    }

    /// The usage written back in the docs' notation, spaces normalized.
    #[cfg(test)]
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        self.write(self.root, &mut out);
        out
    }

    /// What may follow `args` (the typed arguments, without the command's
    /// name), in the docs' notation; empty when the command is complete.
    /// `None` when the arguments cannot be read against the usage.
    pub(crate) fn remaining<A: AsRef<[u8]>>(&self, args: &[A]) -> Option<String> {
        let best = self.read(args).into_iter().next()?;
        let mut out = String::new();
        for &frame in best.stack.0.iter().rev() {
            self.write_frame(frame, &mut out);
        }
        Some(out)
    }

    /// The tokens that may come right after `args`, best readings first, each
    /// with the values it takes right after it (`EX` → `seconds`). Empty when
    /// only values can come next, or `args` cannot be read.
    pub(crate) fn next_tokens<A: AsRef<[u8]>>(&self, args: &[A]) -> Vec<(&'static str, String)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for reading in self.read(args) {
            for stack in self.frontier(reading.stack) {
                let Some((&top, below)) = stack.0.split_last() else {
                    continue;
                };
                let Node::Token(token) = self.nodes[top.id] else {
                    continue;
                };
                if !seen.insert(token.to_ascii_uppercase()) {
                    continue;
                }
                let takes: Vec<&str> = below
                    .iter()
                    .rev()
                    .map_while(|frame| match self.nodes[frame.id] {
                        Node::Arg(arg) => Some(arg),
                        _ => None,
                    })
                    .collect();
                out.push((token, takes.join(" ")));
            }
        }
        out
    }

    /// Every reading of `args`, best first: most tokens matched, then the
    /// order the usage lists its alternatives in (taking an optional part
    /// before skipping it).
    fn read<A: AsRef<[u8]>>(&self, args: &[A]) -> Vec<Reading> {
        let mut readings = vec![Reading {
            stack: Stack(vec![Frame::new(self.root)]),
            tokens: 0,
        }];
        for arg in args {
            let arg = arg.as_ref();
            let mut next: Vec<Reading> = Vec::new();
            // Where each stack is in `next`: two readings that end up the
            // same keep the better count.
            let mut seen: HashMap<Stack, usize> = HashMap::new();
            for reading in &readings {
                for mut stack in self.frontier(reading.stack.clone()) {
                    let Some(&top) = stack.0.last() else {
                        continue;
                    };
                    let tokens = match self.nodes[top.id] {
                        Node::Token(token) if token_matches(token, arg) => reading.tokens + 1,
                        Node::Token(_) => continue,
                        Node::Arg(_) => reading.tokens,
                        // Stays on top: it takes the next ones too.
                        Node::Rest => reading.tokens,
                        _ => unreachable!("the frontier stops at atoms"),
                    };
                    if !matches!(self.nodes[top.id], Node::Rest) {
                        stack.0.pop();
                    }
                    match seen.get(&stack) {
                        Some(&ix) => next[ix].tokens = next[ix].tokens.max(tokens),
                        None => {
                            seen.insert(stack.clone(), next.len());
                            next.push(Reading { stack, tokens });
                        }
                    }
                }
            }
            if next.is_empty() {
                return next;
            }
            // Stable: equally good readings keep the usage's order.
            next.sort_by_key(|reading| std::cmp::Reverse(reading.tokens));
            next.truncate(MAX_READINGS);
            readings = next;
        }
        readings
    }

    /// Expands `stack` until an atom (token, value or `...`) is on top, in
    /// every way the usage allows, preferred ways first. An empty stack (the
    /// command may end here) is included.
    fn frontier(&self, stack: Stack) -> Vec<Stack> {
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        let mut steps = 0;
        self.expand(stack, &mut out, &mut visited, &mut steps);
        out
    }

    fn expand(
        &self,
        mut stack: Stack,
        out: &mut Vec<Stack>,
        visited: &mut HashSet<Stack>,
        steps: &mut usize,
    ) {
        loop {
            *steps += 1;
            // A repeat of something that can be empty comes back to the same
            // stack; so can two ways through optional parts.
            if *steps > MAX_STEPS || out.len() >= MAX_READINGS || !visited.insert(stack.clone()) {
                return;
            }
            let Some(top) = stack.0.pop() else {
                out.push(stack);
                return;
            };
            match &self.nodes[top.id] {
                Node::Token(_) | Node::Arg(_) => {
                    stack.0.push(top);
                    out.push(stack);
                    return;
                }
                Node::Rest => {
                    let mut rest = stack.clone();
                    rest.0.push(top);
                    out.push(rest);
                    // Or nothing more.
                }
                Node::Seq(items) => stack.0.extend(items.iter().rev().map(|&id| Frame::new(id))),
                &Node::Optional(inner) => {
                    let mut taken = stack.clone();
                    taken.0.push(Frame::new(inner));
                    self.expand(taken, out, visited, steps);
                }
                &Node::Repeat(inner) => {
                    let mut again = stack.clone();
                    again.0.extend([top, Frame::new(inner)]);
                    self.expand(again, out, visited, steps);
                }
                Node::OneOf(alternatives) => {
                    for &alternative in alternatives {
                        let mut taken = stack.clone();
                        taken.0.push(Frame::new(alternative));
                        self.expand(taken, out, visited, steps);
                    }
                    return;
                }
                Node::Options(options) => {
                    // Any option not used yet, with the rest still open...
                    for (bit, &option) in options.iter().enumerate() {
                        let bit = 1 << bit;
                        let Node::Optional(inner) = self.nodes[option] else {
                            unreachable!("options are optional parts");
                        };
                        if top.used & bit != 0 {
                            continue;
                        }
                        let mut taken = stack.clone();
                        taken.0.push(Frame {
                            id: top.id,
                            used: top.used | bit,
                        });
                        taken.0.push(Frame::new(inner));
                        self.expand(taken, out, visited, steps);
                    }
                    // ...or no more of them.
                }
            }
        }
    }

    fn write(&self, id: Id, out: &mut String) {
        match &self.nodes[id] {
            Node::Token(text) | Node::Arg(text) => out.push_str(text),
            Node::Rest => out.push_str("..."),
            Node::Seq(items) => self.write_all(items, " ", out),
            &Node::Optional(inner) => {
                out.push('[');
                match &self.nodes[inner] {
                    Node::OneOf(alternatives) => self.write_all(alternatives, " | ", out),
                    _ => self.write(inner, out),
                }
                out.push(']');
            }
            &Node::Repeat(inner) => {
                out.push('[');
                match &self.nodes[inner] {
                    Node::OneOf(alternatives) => self.write_all(alternatives, " | ", out),
                    _ => self.write(inner, out),
                }
                out.push_str(" ...]");
            }
            Node::OneOf(alternatives) => {
                out.push('<');
                self.write_all(alternatives, " | ", out);
                out.push('>');
            }
            Node::Options(options) => self.write_all(options, " ", out),
        }
    }

    /// A node still to come, after what is in `out` already; options leave
    /// out the ones used.
    fn write_frame(&self, frame: Frame, out: &mut String) {
        let ids: Vec<Id> = match &self.nodes[frame.id] {
            Node::Options(options) => options
                .iter()
                .enumerate()
                .filter(|&(bit, _)| frame.used & (1 << bit) == 0)
                .map(|(_, &id)| id)
                .collect(),
            _ => vec![frame.id],
        };
        for id in ids {
            if !out.is_empty() {
                out.push(' ');
            }
            self.write(id, out);
        }
    }

    fn write_all(&self, ids: &[Id], separator: &str, out: &mut String) {
        for (i, &id) in ids.iter().enumerate() {
            if i > 0 {
                let _ = write!(out, "{separator}");
            }
            self.write(id, out);
        }
    }
}

/// Whether a typed argument is `token`: the same letters in any case, as
/// the server reads tokens. `""` stands for the empty argument.
fn token_matches(token: &str, arg: &[u8]) -> bool {
    if token == r#""""# {
        arg.is_empty()
    } else {
        arg.eq_ignore_ascii_case(token.as_bytes())
    }
}

/// Whether a word of the usage is a value rather than a token: values are
/// named in lower case (`key`, `numKeys`), tokens in upper case or symbols
/// (`NX`, `*`, `~`, `""`).
fn is_value(word: &str) -> bool {
    word.bytes().any(|b| b.is_ascii_lowercase())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lexeme {
    Open(u8),
    Close(u8),
    Bar,
    Word(&'static str),
}

fn lex(usage: &'static str) -> Vec<Lexeme> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, b) in usage.bytes().enumerate() {
        let structural = matches!(b, b'[' | b']' | b'<' | b'>' | b'|');
        if structural || b.is_ascii_whitespace() {
            if let Some(start) = start.take() {
                out.push(Lexeme::Word(&usage[start..i]));
            }
            match b {
                b'[' | b'<' => out.push(Lexeme::Open(b)),
                b']' | b'>' => out.push(Lexeme::Close(b)),
                b'|' => out.push(Lexeme::Bar),
                _ => {}
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(start) = start {
        out.push(Lexeme::Word(&usage[start..]));
    }
    out
}

struct Parser<'a> {
    lexemes: &'a [Lexeme],
    pos: usize,
    nodes: Vec<Node>,
}

impl Parser<'_> {
    fn push(&mut self, node: Node) -> Id {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// Items up to a `|` or a closing bracket (not consumed).
    fn seq(&mut self) -> Option<Vec<Id>> {
        let mut items = Vec::new();
        while let Some(&lexeme) = self.lexemes.get(self.pos) {
            let item = match lexeme {
                Lexeme::Bar | Lexeme::Close(_) => break,
                Lexeme::Word("...") => {
                    self.pos += 1;
                    self.push(Node::Rest)
                }
                Lexeme::Word(word) => {
                    self.pos += 1;
                    self.push(if is_value(word) {
                        Node::Arg(word)
                    } else {
                        Node::Token(word)
                    })
                }
                Lexeme::Open(open) => {
                    self.pos += 1;
                    self.group(open)?
                }
            };
            items.push(item);
        }
        Some(items)
    }

    /// A bracketed group, its opening bracket consumed.
    fn group(&mut self, open: u8) -> Option<Id> {
        let close = if open == b'[' { b']' } else { b'>' };
        let mut alternatives = vec![self.seq()?];
        while self.lexemes.get(self.pos) == Some(&Lexeme::Bar) {
            self.pos += 1;
            alternatives.push(self.seq()?);
        }
        if self.lexemes.get(self.pos) != Some(&Lexeme::Close(close)) {
            return None;
        }
        self.pos += 1;

        // `[x ...]`: a trailing `...` repeats the group.
        let repeat = open == b'['
            && alternatives.last().is_some_and(|items| {
                items.len() > 1 && items.last().is_some_and(|&id| self.nodes[id] == Node::Rest)
            });
        if repeat && let Some(last) = alternatives.last_mut() {
            last.pop();
        }
        self.tokens_by_choice(&alternatives);
        let inner = if alternatives.len() == 1 && open == b'[' {
            let items = alternatives.pop()?;
            self.push(Node::Seq(items))
        } else {
            let alternatives = alternatives
                .into_iter()
                .map(|items| self.push(Node::Seq(items)))
                .collect();
            self.push(Node::OneOf(alternatives))
        };
        Some(match open {
            b'[' if repeat => self.push(Node::Repeat(inner)),
            b'[' => self.push(Node::Optional(inner)),
            _ => inner,
        })
    }

    /// A choice between single words where more than one is named like a
    /// value (`<m | km | mi | ft>`, `<start | - | end | + | mid | ~>`) is a
    /// choice between tokens that happen to be lower case: values could not
    /// be told apart. One value among tokens (`<* | id>`, `<key | "">`) is a
    /// real value.
    fn tokens_by_choice(&mut self, alternatives: &[Vec<Id>]) {
        let single: Vec<Id> = alternatives
            .iter()
            .filter_map(|items| match items.as_slice() {
                [id] => Some(*id),
                _ => None,
            })
            .collect();
        if alternatives.len() < 2 || single.len() != alternatives.len() {
            return;
        }
        let values = single
            .iter()
            .filter(|&&id| matches!(self.nodes[id], Node::Arg(_)))
            .count();
        if values >= 2 {
            for id in single {
                if let Node::Arg(word) = self.nodes[id] {
                    self.nodes[id] = Node::Token(word);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
