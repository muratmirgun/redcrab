//! The console's input line as a view-model: history on Up / Down, Tab
//! completion, the suggestion list and the inline syntax hint. A front end
//! forwards keys and edits and draws what it says; it never reasons about
//! commands itself.

use super::{
    Candidate, CandidateKind, Completion, Edit, History, TabCompletion, complete::complete, hint,
};

/// The state of a console's input line besides its text, which the front
/// end's text field owns.
#[derive(Clone, Debug, Default)]
pub struct Prompt {
    history: History,
    tab: Option<TabCompletion>,
    /// Suggestions for the command name being typed, with the line they
    /// were made for.
    live: Option<(String, Completion)>,
    /// What may follow the line, with the line it was made for.
    hint: Option<(String, String)>,
}

/// The suggestion list under the input line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Suggestions<'a> {
    pub candidates: &'a [Candidate],
    /// The candidate Tab put in the line, if any.
    pub selected: Option<usize>,
}

impl Prompt {
    pub fn new(history: History) -> Self {
        Self {
            history,
            ..Self::default()
        }
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// For merging in a history file read after the console opened (see
    /// [`History::merge_older`]).
    pub fn history_mut(&mut self) -> &mut History {
        &mut self.history
    }

    /// The user changed the line (typed, deleted, pasted). Ends a Tab
    /// cycle; suggests command names while one is being typed. Ends
    /// browsing the history too, so the edited line becomes the draft the
    /// next Up keeps: what the user typed is never lost.
    pub fn edited(&mut self, line: &str, cursor: usize) {
        self.tab = None;
        self.history.stop_browsing();
        let completion = complete(line, cursor);
        self.live = worth_suggesting(line, &completion).then(|| (line.to_string(), completion));
        self.update_hint(line);
    }

    /// Up: the previous command from the history, to put in the line;
    /// `None` leaves the line as it is. `line` is the line now (kept as the
    /// draft when browsing starts).
    pub fn up(&mut self, line: &str) -> Option<String> {
        let line = self.history.older(line)?.to_string();
        self.showing(&line);
        Some(line)
    }

    /// Down: the next command, or past the newest the line typed before
    /// browsing. `None` when not browsing.
    pub fn down(&mut self) -> Option<String> {
        let line = self.history.newer()?.to_string();
        self.showing(&line);
        Some(line)
    }

    /// Tab (`forward`) or Shift-Tab: completes the word at `cursor`, or goes
    /// on cycling through the candidates when `line` and `cursor` are what
    /// the last Tab left. A lone candidate is accepted and ends the cycle,
    /// so the next Tab completes the next argument. `None` when nothing
    /// completes there.
    ///
    /// A completed line counts as edited: it ends browsing the history, so
    /// the next Up keeps it as the draft (see [`Prompt::edited`]).
    pub fn tab(&mut self, line: &str, cursor: usize, forward: bool) -> Option<Edit> {
        if !self
            .tab
            .as_ref()
            .is_some_and(|tab| tab.continues(line, cursor))
        {
            self.tab = TabCompletion::start(line, cursor);
        }
        let tab = self.tab.as_mut()?;
        let edit = if forward {
            tab.forward()
        } else {
            tab.backward()
        };
        if tab.finished() {
            // Nothing left to choose from: no list to show (or for Escape
            // to close first).
            self.tab = None;
        }
        self.live = None;
        self.history.stop_browsing();
        self.update_hint(&edit.text);
        Some(edit)
    }

    /// The user picked suggestion `ix` (a click): it replaces the word, and
    /// the cursor moves on to the next argument. Like Tab, ends browsing the
    /// history.
    pub fn pick(&mut self, line: &str, ix: usize) -> Option<Edit> {
        let edit = match (&self.tab, &self.live) {
            (Some(tab), _) => tab.accept(ix),
            (None, Some((made_for, completion))) if made_for == line => completion.accept(line, ix),
            _ => None,
        }?;
        self.history.stop_browsing();
        self.showing(&edit.text);
        Some(edit)
    }

    /// Escape: hides the suggestion list and ends a Tab cycle. Returns
    /// whether there was anything to hide, so the key can mean something
    /// else otherwise.
    pub fn dismiss(&mut self) -> bool {
        let shown = self.tab.is_some() || self.live.is_some();
        self.tab = None;
        self.live = None;
        shown
    }

    /// Enter: `line` is being run. Returns whether it went into the history
    /// (see [`History::add`]), so the caller saves it.
    pub fn submit(&mut self, line: &str) -> bool {
        self.tab = None;
        self.live = None;
        self.hint = None;
        self.history.add(line)
    }

    /// The suggestion list to show under the line: the Tab candidates while
    /// cycling, else command names matching the one being typed.
    pub fn suggestions(&self) -> Option<Suggestions<'_>> {
        if let Some(tab) = &self.tab {
            return Some(Suggestions {
                candidates: tab.candidates(),
                selected: tab.selected(),
            });
        }
        self.live.as_ref().map(|(_, completion)| Suggestions {
            candidates: &completion.candidates,
            selected: None,
        })
    }

    /// The syntax that may follow `line` (see [`hint`]), to show greyed out
    /// after it; only while the cursor is at its end, as in redis-cli.
    pub fn hint(&self, line: &str, cursor: usize) -> Option<&str> {
        let (made_for, hint) = self.hint.as_ref()?;
        (made_for == line && cursor >= line.len()).then_some(hint.as_str())
    }

    /// The line now shows `line`, put there by the prompt (history or a
    /// completion), not typed.
    fn showing(&mut self, line: &str) {
        self.tab = None;
        self.live = None;
        self.update_hint(line);
    }

    fn update_hint(&mut self, line: &str) {
        self.hint = hint(line).map(|hint| (line.to_string(), hint));
    }
}

/// Whether to show suggestions for `completion` unasked: only for a command
/// or subcommand name being typed, and not when the only one is the word as
/// typed.
fn worth_suggesting(line: &str, completion: &Completion) -> bool {
    let typed = line.get(completion.range.clone()).unwrap_or_default();
    let names = completion
        .candidates
        .iter()
        .all(|c| matches!(c.kind, CandidateKind::Command | CandidateKind::Subcommand));
    let done =
        matches!(completion.candidates.as_slice(), [only] if only.text.eq_ignore_ascii_case(typed));
    !typed.is_empty() && !completion.candidates.is_empty() && names && !done
}

#[cfg(test)]
mod tests;
