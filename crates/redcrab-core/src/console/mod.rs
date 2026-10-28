//! Everything a console prompt needs besides running commands (which
//! [`crate::ConsoleSession`] does): a per-connection [`History`] that never
//! keeps secrets, Tab [`complete`]ion from the command table, and inline
//! syntax [`hint`]s like `redis-cli`'s. [`Prompt`] ties them together for a
//! front end.
//!
//! No I/O here but [`HistoryFile`], whose blocking calls a front end makes
//! off its UI thread.

mod complete;
mod history;
mod prompt;
mod secrets;
mod syntax;
pub mod words;

pub use complete::{Candidate, CandidateKind, Completion, Edit, TabCompletion, complete};
pub use history::{HISTORY_LIMIT, History, HistoryFile};
pub use prompt::{Prompt, Suggestions};
pub use secrets::{is_sensitive, is_sensitive_line};

use crate::commands::{self, CommandSpec};
use syntax::Grammar;
use words::Word;

/// The syntax that may follow `input`, like `redis-cli`'s hints: `SET k`
/// gives ` value [NX | XX | ...] [GET] [EX seconds | ...]`, `CONFIG GET`
/// gives ` parameter [parameter ...]`. The arguments typed so far are
/// matched against the command's usage, so what they fill is left out
/// (`ZADD k NX` continues with `[GT | LT] [CH] [INCR] score member ...`).
///
/// The hint is meant to be shown right after the input: it starts with a
/// space unless the input ends in one. `None` for an unknown command, a
/// complete one, or arguments that do not fit its usage.
pub fn hint(input: &str) -> Option<String> {
    let line = Line::read(input)?;
    if line.cannot_run() {
        return None;
    }
    let rest = line.grammar.remaining(&line.args())?;
    if rest.is_empty() {
        return None;
    }
    let open = line.words.words.last().is_some_and(|w| w.open);
    let spaced = input.ends_with(char::is_whitespace) && !open;
    Some(if spaced { rest } else { format!(" {rest}") })
}

/// Whether the arguments of `input` fit its command's usage so far, the
/// last one included: `Some(false)` for a misspelled token, an option given
/// twice, one argument too many, ... `None` when the table does not know
/// the command (or its subcommand is not typed in full yet).
///
/// [`hint`] cannot tell a complete line from one that does not fit; this
/// can, for a front end that wants to flag the line.
pub fn fits(input: &str) -> Option<bool> {
    let line = Line::read(input)?;
    Some(!line.cannot_run() && line.grammar.remaining(&line.args()).is_some())
}

/// A console line read against its command's usage.
struct Line {
    words: words::Words,
    grammar: Grammar,
    /// How many words name the command: 2 for a subcommand.
    named: usize,
}

impl Line {
    /// `None` for a command the table does not know.
    fn read(input: &str) -> Option<Self> {
        let words = words::split(input);
        let (spec, named) = resolve(&words.words)?;
        let grammar = Grammar::parse(usage(spec)?)?;
        Some(Self {
            words,
            grammar,
            named,
        })
    }

    /// Text right after a closing quote: the console refuses the line. An
    /// unclosed quote is fine, it is just still being typed.
    fn cannot_run(&self) -> bool {
        self.words
            .error
            .is_some_and(|err| err != words::UNCLOSED_QUOTE)
    }

    fn args(&self) -> Vec<&[u8]> {
        self.words.words[self.named..]
            .iter()
            .map(|w| &w.value[..])
            .collect()
    }
}

/// The command `words` start with, and how many words name it: two for a
/// subcommand (`CONFIG GET`), else one. A container without a subcommand yet
/// is itself; an unknown subcommand is nothing.
fn resolve(words: &[Word]) -> Option<(&'static CommandSpec, usize)> {
    let name = words.first()?.text();
    let spec = commands::lookup(&name, None)?;
    if commands::subcommands(spec).is_empty() {
        return Some((spec, 1));
    }
    match words.get(1) {
        None => Some((spec, 1)),
        Some(sub) => Some((commands::lookup(&name, Some(&sub.text()))?, 2)),
    }
}

/// A command's usage without its name: `key value [NX | XX] ...` for SET.
fn usage(spec: &'static CommandSpec) -> Option<&'static str> {
    Some(spec.syntax.strip_prefix(spec.name)?.trim_start())
}

#[cfg(test)]
mod tests;
