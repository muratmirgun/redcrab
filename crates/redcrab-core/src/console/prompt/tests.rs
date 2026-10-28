use super::*;

/// A text field the prompt drives, as a front end would.
struct Field {
    prompt: Prompt,
    line: String,
    cursor: usize,
}

impl Field {
    fn new(history: &[&str]) -> Self {
        Self {
            prompt: Prompt::new(History::from_entries(history)),
            line: String::new(),
            cursor: 0,
        }
    }

    fn type_text(&mut self, text: &str) {
        self.line.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.prompt.edited(&self.line, self.cursor);
    }

    fn set(&mut self, line: Option<String>) {
        if let Some(line) = line {
            self.cursor = line.len();
            self.line = line;
        }
    }

    fn apply(&mut self, edit: Option<Edit>) {
        if let Some(edit) = edit {
            self.line = edit.text;
            self.cursor = edit.cursor;
        }
    }

    fn up(&mut self) {
        let line = self.prompt.up(&self.line);
        self.set(line);
    }

    fn down(&mut self) {
        let line = self.prompt.down();
        self.set(line);
    }

    fn tab(&mut self) {
        let edit = self.prompt.tab(&self.line, self.cursor, true);
        self.apply(edit);
    }

    fn shift_tab(&mut self) {
        let edit = self.prompt.tab(&self.line, self.cursor, false);
        self.apply(edit);
    }

    fn enter(&mut self) -> bool {
        let line = std::mem::take(&mut self.line);
        self.cursor = 0;
        self.prompt.submit(&line)
    }

    fn hint(&self) -> Option<&str> {
        self.prompt.hint(&self.line, self.cursor)
    }

    fn suggested(&self) -> Vec<&'static str> {
        self.prompt
            .suggestions()
            .map(|s| s.candidates.iter().map(|c| c.name).collect())
            .unwrap_or_default()
    }

    fn selected(&self) -> Option<usize> {
        self.prompt.suggestions().and_then(|s| s.selected)
    }
}

#[test]
fn up_and_down_walk_the_history_and_keep_the_draft() {
    let mut field = Field::new(&["GET a", "SET b 1"]);
    field.type_text("HGET");
    field.up();
    assert_eq!(field.line, "SET b 1");
    assert_eq!(field.cursor, 7);
    assert!(
        field.suggested().is_empty(),
        "a recalled line suggests nothing"
    );
    field.up();
    assert_eq!(field.line, "GET a");
    field.up();
    assert_eq!(field.line, "GET a", "nothing older");
    field.down();
    field.down();
    assert_eq!(field.line, "HGET", "the draft is back");
    field.down();
    assert_eq!(field.line, "HGET");

    // Running the newest command again does not add it twice; an older one
    // becomes the newest.
    field.up();
    assert!(!field.enter(), "already the newest");
    field.up();
    field.up();
    assert_eq!(field.line, "GET a");
    assert!(field.enter());
    assert_eq!(
        field.prompt.history().iter().collect::<Vec<_>>(),
        ["GET a", "SET b 1", "GET a"]
    );
    // Browsing starts over from the newest.
    field.up();
    assert_eq!(field.line, "GET a");
}

#[test]
fn an_edited_recalled_line_becomes_the_draft() {
    let mut field = Field::new(&["GET a", "SET b 1"]);
    field.up();
    field.up();
    assert_eq!(field.line, "GET a");
    field.type_text("bc");
    assert_eq!(field.line, "GET abc");
    // Up starts from the newest again and keeps the edit as the draft.
    field.up();
    assert_eq!(field.line, "SET b 1");
    field.down();
    assert_eq!(field.line, "GET abc", "the edit is not lost");
    assert_eq!(
        field.prompt.history().iter().collect::<Vec<_>>(),
        ["GET a", "SET b 1"],
        "and the history is unchanged"
    );
}

#[test]
fn a_completed_recalled_line_becomes_the_draft() {
    // Tab and a pick change the line without an edit event, as a front end
    // setting the line sends none; the completed line is kept all the same.
    let mut field = Field::new(&["GET a", "CONFIG RESETST"]);
    field.type_text("typed draft");
    field.up();
    assert_eq!(field.line, "CONFIG RESETST");
    field.tab();
    assert_eq!(field.line, "CONFIG RESETSTAT ");
    field.down();
    assert_eq!(field.line, "CONFIG RESETSTAT ", "no longer browsing");
    field.up();
    assert_eq!(field.line, "CONFIG RESETST");
    field.up();
    assert_eq!(field.line, "GET a");
    field.down();
    field.down();
    assert_eq!(
        field.line, "CONFIG RESETSTAT ",
        "the completion is the draft"
    );

    // The same for a suggestion picked from a Tab cycle.
    let mut field = Field::new(&["GET a", "config re"]);
    field.up();
    field.tab();
    assert_eq!(field.line, "config resetstat");
    let edit = field.prompt.pick(&field.line, 1);
    field.apply(edit);
    assert_eq!(field.line, "config rewrite ");
    field.down();
    assert_eq!(field.line, "config rewrite ", "no longer browsing");
    field.up();
    assert_eq!(field.line, "config re");
    field.down();
    assert_eq!(field.line, "config rewrite ", "the pick is the draft");
    assert_eq!(
        field.prompt.history().iter().collect::<Vec<_>>(),
        ["GET a", "config re"],
        "and the history is unchanged"
    );
}

#[test]
fn submitting_records_history_without_secrets() {
    let mut field = Field::new(&[]);
    field.type_text("SET a 1");
    assert!(field.enter());
    field.type_text("SET a 1");
    assert!(!field.enter(), "the same as the newest");
    field.type_text("AUTH default s3cret");
    assert!(!field.enter(), "never a password");
    field.type_text("  ");
    assert!(!field.enter());
    assert_eq!(
        field.prompt.history().iter().collect::<Vec<_>>(),
        ["SET a 1"]
    );
    assert_eq!(field.hint(), None);
    assert!(field.suggested().is_empty());
}

#[test]
fn suggests_command_names_while_typing() {
    let mut field = Field::new(&[]);
    assert!(field.suggested().is_empty());
    field.type_text("hgeta");
    assert_eq!(field.suggested(), ["HGETALL"]);
    assert_eq!(field.selected(), None);
    // The whole name typed: nothing left to suggest.
    field.type_text("ll");
    assert!(field.suggested().is_empty());
    // Arguments are not suggested unasked.
    field.type_text(" k");
    assert!(field.suggested().is_empty());

    let mut field = Field::new(&[]);
    field.type_text("config g");
    assert_eq!(field.suggested()[0], "CONFIG GET");
    // Escape hides them; typing brings them back.
    assert!(field.prompt.dismiss());
    assert!(field.suggested().is_empty());
    assert!(!field.prompt.dismiss(), "nothing left to hide");
    field.type_text("e");
    assert_eq!(field.suggested()[0], "CONFIG GET");
}

#[test]
fn tab_completes_cycles_and_picks() {
    let mut field = Field::new(&[]);
    field.type_text("config re");
    field.tab();
    assert_eq!(field.line, "config resetstat");
    assert_eq!(field.selected(), Some(0));
    assert_eq!(field.suggested(), ["CONFIG RESETSTAT", "CONFIG REWRITE"]);
    field.tab();
    assert_eq!(field.line, "config rewrite");
    assert_eq!(field.selected(), Some(1));
    field.tab();
    assert_eq!(field.line, "config re", "back to what was typed");
    field.shift_tab();
    assert_eq!(field.line, "config rewrite");

    // Typing ends the cycle; Tab starts over from the new line.
    field.type_text(" ");
    assert_eq!(field.suggested(), Vec::<&str>::new());
    assert_eq!(field.hint(), None, "CONFIG REWRITE takes nothing");

    // A click on a suggestion accepts it.
    let mut field = Field::new(&[]);
    field.type_text("hgeta");
    let edit = field.prompt.pick(&field.line, 0);
    field.apply(edit);
    assert_eq!((field.line.as_str(), field.cursor), ("hgetall ", 8));
    assert!(field.suggested().is_empty());
    assert_eq!(field.hint(), Some("key"));
    // A stale list (the line changed without an edit event) picks nothing.
    field.type_text("x");
    assert_eq!(field.prompt.pick("something else", 0), None);

    // Picking from a Tab cycle accepts that candidate in the original line.
    let mut field = Field::new(&[]);
    field.type_text("config re");
    field.tab();
    let edit = field.prompt.pick(&field.line, 1);
    field.apply(edit);
    assert_eq!(field.line, "config rewrite ");

    // Nothing to complete: the line stays.
    let mut field = Field::new(&[]);
    field.type_text("GET ");
    field.tab();
    assert_eq!(field.line, "GET ");
}

#[test]
fn tab_after_a_lone_candidate_completes_the_next_argument() {
    // Between Tabs the field sends no edit: a front end that sets the line
    // gets no change event for it.
    let mut field = Field::new(&[]);
    field.type_text("CLIENT KIL");
    field.tab();
    assert_eq!((field.line.as_str(), field.cursor), ("CLIENT KILL ", 12));
    // The word is done: no list stays open, and Escape has nothing to close
    // (so it can stop a running command).
    assert!(field.suggested().is_empty());
    assert_eq!(field.selected(), None);
    assert!(!field.prompt.dismiss());
    // The next Tab completes the next argument.
    field.tab();
    assert_eq!(field.line, "CLIENT KILL ID");
    assert_eq!(field.selected(), Some(0));
    assert_eq!(
        field.suggested(),
        ["ID", "TYPE", "USER", "ADDR", "LADDR", "SKIPME", "MAXAGE"]
    );
    field.tab();
    assert_eq!(field.line, "CLIENT KILL TYPE", "and cycles through them");

    // The same with Shift-Tab, and with a token.
    let mut field = Field::new(&[]);
    field.type_text("ZRANGE k 0 -1 WITHSC");
    field.shift_tab();
    assert_eq!(field.line, "ZRANGE k 0 -1 WITHSCORES ");
    assert!(field.suggested().is_empty());
    field.tab();
    assert!(field.line.len() > "ZRANGE k 0 -1 WITHSCORES ".len());
    let offered = field.suggested();
    for token in ["BYSCORE", "REV", "LIMIT"] {
        assert!(offered.contains(&token), "{token} in {offered:?}");
    }
}

#[test]
fn tab_completes_where_the_cursor_is_now() {
    let mut field = Field::new(&[]);
    field.type_text("config re");
    field.tab();
    assert_eq!(field.line, "config resetstat");
    // The cursor moves into the command name (Home, arrows) without an
    // edit event: Tab completes that word, not the old one again.
    field.cursor = 3;
    field.tab();
    assert_eq!((field.line.as_str(), field.cursor), ("config resetstat", 6));
    assert_eq!(field.suggested()[0], "CONFIG");
    assert_eq!(field.selected(), Some(0));
}

#[test]
fn hints_follow_the_line() {
    let mut field = Field::new(&["HSET h f"]);
    field.type_text("SET");
    assert_eq!(
        field.hint(),
        Some(
            " key value [NX | XX | IFEQ ifeq-value | IFNE ifne-value | IFDEQ ifdeq-digest | IFDNE ifdne-digest] [GET] [EX seconds | PX milliseconds | EXAT unix-time-seconds | PXAT unix-time-milliseconds | KEEPTTL]"
        )
    );
    field.type_text(" k v XX GET EX");
    assert_eq!(field.hint(), Some(" seconds"));
    // Only with the cursor at the end.
    field.cursor = 3;
    assert_eq!(field.hint(), None);
    field.cursor = field.line.len();
    field.type_text(" 10");
    assert_eq!(field.hint(), None, "complete");

    // A recalled line gets its hint too.
    field.up();
    assert_eq!(field.line, "HSET h f");
    assert_eq!(field.hint(), Some(" value [field value ...]"));
    field.down();
    assert_eq!(field.line, "SET k v XX GET EX 10");

    // A hint made for another line is not shown.
    assert_eq!(field.prompt.hint("SET", 3), None);
}
