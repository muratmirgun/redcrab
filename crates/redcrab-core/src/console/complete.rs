//! Tab completion for the console: command names, the subcommands of
//! container commands (`CONFIG GET`), and the tokens a command takes at the
//! cursor (`EX`, `NX`, `WITHSCORES`), from the generated command table.

use std::{borrow::Cow, ops::Range};

use super::{resolve, syntax::Grammar, usage, words};
use crate::commands::{self, CommandSpec, Flags};

/// What can go where the cursor is, best first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completion {
    /// The bytes of the line a candidate replaces: the whole word the
    /// cursor is in, or an empty range where a new word starts.
    pub range: Range<usize>,
    pub candidates: Vec<Candidate>,
}

/// One way to complete a word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What completing inserts, in the case the user types in: `hgetall`
    /// after `hge`, `HGETALL` after `HGE`.
    pub text: String,
    /// The name as the docs spell it: `HGETALL`, `CONFIG GET` for a
    /// subcommand, `EX` for a token.
    pub name: &'static str,
    /// The command's one-line summary; for a token, the values it takes
    /// right after it (`seconds` for `EX`), if any.
    pub summary: Cow<'static, str>,
    pub kind: CandidateKind,
    /// Matched by its letters in order (`hga` → `HGETALL`) rather than as a
    /// prefix.
    pub fuzzy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    Command,
    /// A subcommand of a container command (`CONFIG GET`, `CLIENT LIST`).
    Subcommand,
    /// A token the command takes at that point (`NX`, `EX`, `LIMIT`).
    Token,
}

/// A change to the console line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub text: String,
    /// Byte offset of the cursor in `text`.
    pub cursor: usize,
}

/// What can complete the word at byte offset `cursor` of `input`, best
/// first: an exact match, then names starting with what was typed before
/// the cursor (deprecated commands last), then names that contain its
/// letters in order (from two letters typed on). Commands Redis keeps for
/// itself (`syscmd`, such as REPLCONF) are left out. Nothing completes a
/// quoted word: arguments are the user's.
pub fn complete(input: &str, cursor: usize) -> Completion {
    let mut cursor = cursor.min(input.len());
    while !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let words = words::split(input);
    let (index, range) = match words.at(cursor) {
        Some((ix, word)) => (ix, word.span.clone()),
        None => (words.before(cursor), cursor..cursor),
    };
    let mut completion = Completion {
        range: range.clone(),
        candidates: Vec::new(),
    };
    let current = words.words.get(index).filter(|word| word.span == range);
    if current.is_some_and(|word| word.quoted) {
        return completion;
    }
    let typed = &input[range.start..cursor];
    let case = Case::of(typed)
        .or_else(|| words.words.first().and_then(|w| Case::of(&w.text())))
        .unwrap_or(Case::Upper);

    let first = words.words.first().filter(|_| index > 0);
    let container = first
        .and_then(|word| commands::lookup(&word.text(), None))
        .filter(|spec| !commands::subcommands(spec).is_empty());
    let mut found: Vec<(Rank, Candidate)> = Vec::new();
    match (index, container) {
        (0, _) => {
            for spec in commands::all() {
                if spec.is_subcommand() || spec.flags.contains(Flags::SYSCMD) {
                    continue;
                }
                if let Some(rank) = Rank::of(typed, spec.name, Some(spec)) {
                    let candidate = candidate(spec.name, spec, CandidateKind::Command, case, rank);
                    found.push((rank, candidate));
                }
            }
        }
        (1, Some(container)) => {
            for spec in commands::subcommands(container) {
                let sub = spec.name.split_once(' ').map_or(spec.name, |(_, sub)| sub);
                if spec.flags.contains(Flags::SYSCMD) {
                    continue;
                }
                if let Some(rank) = Rank::of(typed, sub, Some(spec)) {
                    let candidate = candidate(sub, spec, CandidateKind::Subcommand, case, rank);
                    found.push((rank, candidate));
                }
            }
        }
        _ => {
            let before = &words.words[..index];
            let tokens = resolve(before).and_then(|(spec, named)| {
                let grammar = Grammar::parse(usage(spec)?)?;
                let args: Vec<&[u8]> = before[named..].iter().map(|w| &w.value[..]).collect();
                Some(grammar.next_tokens(&args))
            });
            for (token, takes) in tokens.unwrap_or_default() {
                if let Some(rank) = Rank::of(typed, token, None) {
                    found.push((
                        rank,
                        Candidate {
                            text: case.apply(token),
                            name: token,
                            summary: takes.into(),
                            kind: CandidateKind::Token,
                            fuzzy: rank.fuzzy(),
                        },
                    ));
                }
            }
        }
    }
    // Stable: equals keep the table's (alphabetical) or the usage's order.
    found.sort_by_key(|(rank, _)| *rank);
    completion.candidates = found.into_iter().map(|(_, c)| c).collect();
    completion
}

impl Completion {
    /// `input` with candidate `ix` in place of the word, the cursor right
    /// after it: for showing candidates in turn.
    pub fn preview(&self, input: &str, ix: usize) -> Option<Edit> {
        let candidate = self.candidates.get(ix)?;
        let (before, after) = (input.get(..self.range.start)?, input.get(self.range.end..)?);
        let text = format!("{before}{}{after}", candidate.text);
        Some(Edit {
            cursor: before.len() + candidate.text.len(),
            text,
        })
    }

    /// Like [`Completion::preview`], but done with the word: the cursor moves
    /// past the space after it, adding one if none follows, ready for the
    /// next argument.
    pub fn accept(&self, input: &str, ix: usize) -> Option<Edit> {
        let mut edit = self.preview(input, ix)?;
        match edit.text[edit.cursor..].chars().next() {
            Some(c) if c.is_whitespace() => edit.cursor += c.len_utf8(),
            _ => {
                edit.text.insert(edit.cursor, ' ');
                edit.cursor += 1;
            }
        }
        Some(edit)
    }
}

/// Tab and Shift-Tab: the first Tab completes the word, more Tabs cycle
/// through the candidates and back to what was typed. A lone candidate is
/// accepted at once (see [`Completion::accept`]), which ends the cycle: the
/// next Tab completes at the new cursor.
#[derive(Clone, Debug)]
pub struct TabCompletion {
    input: String,
    cursor: usize,
    completion: Completion,
    selected: Option<usize>,
    /// The line as the last step left it, and the cursor in it.
    shown: Edit,
}

impl TabCompletion {
    /// Starts completing at `cursor`; `None` when nothing completes there.
    pub fn start(input: &str, cursor: usize) -> Option<Self> {
        let completion = complete(input, cursor);
        if completion.candidates.is_empty() {
            return None;
        }
        Some(Self {
            input: input.to_string(),
            cursor,
            completion,
            selected: None,
            shown: Edit {
                text: input.to_string(),
                cursor,
            },
        })
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.completion.candidates
    }

    /// The candidate in the line now; `None` while it shows what was typed.
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Whether `line` with the cursor at `cursor` is still what the last
    /// step left, so another Tab goes on cycling instead of starting over
    /// there. A front end may change neither without telling the prompt
    /// (a cursor moved to another word), so both must match. Never once
    /// [`TabCompletion::finished`].
    pub fn continues(&self, line: &str, cursor: usize) -> bool {
        !self.finished() && self.shown.text == line && self.shown.cursor == cursor
    }

    /// Whether the last step accepted the only candidate: the word is done
    /// and nothing is left to cycle through.
    pub fn finished(&self) -> bool {
        self.completion.candidates.len() == 1 && self.selected.is_some()
    }

    /// Candidate `ix` accepted (see [`Completion::accept`]) in the line the
    /// cycle started from: a click on the list.
    pub fn accept(&self, ix: usize) -> Option<Edit> {
        self.completion.accept(&self.input, ix)
    }

    /// Tab: the next candidate.
    pub fn forward(&mut self) -> Edit {
        self.step(true)
    }

    /// Shift-Tab: the previous candidate.
    pub fn backward(&mut self) -> Edit {
        self.step(false)
    }

    fn step(&mut self, forward: bool) -> Edit {
        let count = self.completion.candidates.len();
        let edit = if count == 1 {
            self.selected = Some(0);
            self.completion.accept(&self.input, 0)
        } else {
            self.selected = match (self.selected, forward) {
                (None, true) => Some(0),
                (None, false) => Some(count - 1),
                (Some(ix), true) => Some(ix + 1).filter(|&ix| ix < count),
                (Some(ix), false) => ix.checked_sub(1),
            };
            self.selected
                .and_then(|ix| self.completion.preview(&self.input, ix))
        };
        let edit = edit.unwrap_or_else(|| Edit {
            text: self.input.clone(),
            cursor: self.cursor,
        });
        self.shown.clone_from(&edit);
        edit
    }
}

fn candidate(
    name: &'static str,
    spec: &'static CommandSpec,
    kind: CandidateKind,
    case: Case,
    rank: Rank,
) -> Candidate {
    Candidate {
        text: case.apply(name),
        name: if kind == CandidateKind::Subcommand {
            spec.name
        } else {
            name
        },
        summary: Cow::Borrowed(spec.summary),
        kind,
        fuzzy: rank.fuzzy(),
    }
}

/// The case completions are inserted in, following what the user typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Upper,
    Lower,
}

impl Case {
    /// Lower when `typed` has lower-case letters and no upper-case ones,
    /// upper when it has upper-case ones; `None` without letters.
    fn of(typed: &str) -> Option<Self> {
        let lower = typed.chars().any(char::is_lowercase);
        let upper = typed.chars().any(char::is_uppercase);
        match (lower, upper) {
            (_, true) => Some(Self::Upper),
            (true, false) => Some(Self::Lower),
            (false, false) => None,
        }
    }

    fn apply(self, name: &str) -> String {
        match self {
            Self::Upper => name.to_ascii_uppercase(),
            Self::Lower => name.to_ascii_lowercase(),
        }
    }
}

/// How well a name matches what was typed; smaller sorts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Exact,
    /// Starts with what was typed; deprecated commands after the others.
    Prefix {
        deprecated: bool,
    },
    /// Has the typed letters in order; better scores first.
    Fuzzy {
        score: std::cmp::Reverse<i32>,
        deprecated: bool,
    },
}

impl Rank {
    fn of(typed: &str, name: &str, spec: Option<&CommandSpec>) -> Option<Self> {
        let deprecated = spec.is_some_and(|spec| spec.flags.contains(Flags::DEPRECATED));
        let (typed, name) = (typed.as_bytes(), name.as_bytes());
        if typed.eq_ignore_ascii_case(name) {
            Some(Self::Exact)
        } else if name.len() > typed.len() && name[..typed.len()].eq_ignore_ascii_case(typed) {
            Some(Self::Prefix { deprecated })
        } else if typed.len() >= 2 {
            fuzzy_score(typed, name).map(|score| Self::Fuzzy {
                deprecated,
                score: std::cmp::Reverse(score),
            })
        } else {
            None
        }
    }

    fn fuzzy(self) -> bool {
        matches!(self, Self::Fuzzy { .. })
    }
}

/// How well `query` matches `name` as a subsequence, ignoring case; higher
/// is better. Letters in a row and letters starting a word (`HGETALL` for
/// `hga`, `FT.SEARCH` for `fts`) score more, gaps less. `None` when the
/// letters are not all there in order.
fn fuzzy_score(query: &[u8], name: &[u8]) -> Option<i32> {
    let mut score = 0;
    let mut matched = 0;
    let mut last: Option<usize> = None;
    for (ix, &byte) in name.iter().enumerate() {
        let Some(&want) = query.get(matched) else {
            break;
        };
        if byte.eq_ignore_ascii_case(&want) {
            score += 1;
            if last.is_some_and(|last| last + 1 == ix) {
                score += 4;
            }
            if ix == 0 || matches!(name[ix - 1], b'.' | b'_' | b'-' | b' ') {
                score += 3;
            }
            last = Some(ix);
            matched += 1;
        } else if last.is_some() {
            score -= 1;
        }
    }
    (matched == query.len()).then_some(score)
}

#[cfg(test)]
mod tests;
