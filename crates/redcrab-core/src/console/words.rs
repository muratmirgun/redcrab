//! Splits a console line into words that know where they are, for
//! completion, hints and the history filter. It reads a line exactly like
//! [`crate::format::split_args`] (which decides what is sent), but never
//! gives up: a line that is still being typed often has an unclosed quote.

use std::{borrow::Cow, ops::Range};

/// [`Words::error`] for a quote that is never closed, as
/// [`crate::format::split_args`] words it.
pub const UNCLOSED_QUOTE: &str = "unbalanced quotes";

/// [`Words::error`] for text right after a closing quote.
pub const TEXT_AFTER_QUOTE: &str = "a closing quote must be followed by a space";

/// One argument of a console line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// Where the word is in the line, in bytes, quotes included.
    pub span: Range<usize>,
    /// The argument it stands for, as [`crate::format::split_args`] reads it.
    pub value: Vec<u8>,
    /// Whether any of it is in quotes.
    pub quoted: bool,
    /// Whether it ends in a quote that was never closed, so it runs to the
    /// end of the line.
    pub open: bool,
}

impl Word {
    /// The argument as text; command names and tokens are ASCII, so bytes
    /// that are not UTF-8 lose nothing that matters there.
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.value)
    }
}

/// A console line split into [`Word`]s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Words {
    pub words: Vec<Word>,
    /// Why [`crate::format::split_args`] refuses the line, if it does. The
    /// words are still the best reading of it: an unclosed quote runs to the
    /// end of the line, and text right after a closing quote starts a new
    /// word.
    pub error: Option<&'static str>,
}

impl Words {
    /// The word the byte offset `cursor` is in or right after, with its
    /// index; `None` when the cursor is in the spaces between words (or
    /// after the last one), where a new word would start.
    pub fn at(&self, cursor: usize) -> Option<(usize, &Word)> {
        self.words
            .iter()
            .enumerate()
            .find(|(_, word)| word.span.start <= cursor && cursor <= word.span.end)
    }

    /// How many words end before `cursor`.
    pub fn before(&self, cursor: usize) -> usize {
        self.words
            .iter()
            .take_while(|word| word.span.end < cursor)
            .count()
    }
}

/// Splits `line` the way `redis-cli` does (see
/// [`crate::format::split_args`]), keeping each word's position.
pub fn split(line: &str) -> Words {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let offset = |i: usize| chars.get(i).map_or(line.len(), |&(at, _)| at);
    let mut words = Words::default();
    let mut i = 0;
    loop {
        while chars.get(i).is_some_and(|&(_, c)| c.is_whitespace()) {
            i += 1;
        }
        if i == chars.len() {
            return words;
        }
        let start = offset(i);
        let mut word = Word {
            span: start..line.len(),
            value: Vec::new(),
            quoted: false,
            open: false,
        };
        while let Some(&(at, c)) = chars.get(i) {
            i += 1;
            match c {
                c if c.is_whitespace() => {
                    word.span.end = at;
                    break;
                }
                '"' | '\'' => {
                    word.quoted = true;
                    match quoted(&chars, i, c, &mut word.value) {
                        Some(end) => {
                            i = end;
                            word.span.end = offset(i);
                            if chars.get(i).is_some_and(|&(_, c)| !c.is_whitespace()) {
                                words.error.get_or_insert(TEXT_AFTER_QUOTE);
                            }
                        }
                        None => {
                            i = chars.len();
                            word.open = true;
                            words.error.get_or_insert(UNCLOSED_QUOTE);
                        }
                    }
                    break;
                }
                c => push_char(&mut word.value, c),
            }
        }
        words.words.push(word);
    }
}

/// Reads the quoted part of a word from `i`, just past the opening `quote`,
/// into `arg`, and returns where it ends: just past the closing quote.
/// `None` when the quote is never closed; `arg` then holds what was read.
fn quoted(chars: &[(usize, char)], mut i: usize, quote: char, arg: &mut Vec<u8>) -> Option<usize> {
    let hex = |i: usize| chars.get(i).and_then(|&(_, c)| c.to_digit(16));
    loop {
        let (_, c) = *chars.get(i)?;
        i += 1;
        match c {
            c if c == quote => return Some(i),
            '\\' if quote == '"' => {
                let (_, escaped) = *chars.get(i)?;
                i += 1;
                match escaped {
                    'x' => match (hex(i), hex(i + 1)) {
                        (Some(high), Some(low)) => {
                            arg.push((high << 4 | low) as u8);
                            i += 2;
                        }
                        _ => arg.push(b'x'),
                    },
                    'n' => arg.push(b'\n'),
                    'r' => arg.push(b'\r'),
                    't' => arg.push(b'\t'),
                    'b' => arg.push(0x08),
                    'a' => arg.push(0x07),
                    c => push_char(arg, c),
                }
            }
            '\\' if quote == '\'' && chars.get(i).is_some_and(|&(_, c)| c == '\'') => {
                arg.push(b'\'');
                i += 1;
            }
            c => push_char(arg, c),
        }
    }
}

fn push_char(arg: &mut Vec<u8>, c: char) {
    arg.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::format::{quote_arg, split_args};

    fn values(line: &str) -> Vec<Vec<u8>> {
        split(line).words.into_iter().map(|w| w.value).collect()
    }

    fn spans(line: &str) -> Vec<&str> {
        split(line)
            .words
            .iter()
            .map(|w| &line[w.span.clone()])
            .collect()
    }

    #[test]
    fn keeps_positions() {
        assert_eq!(
            spans("  SET  k\t\"a b\" 'c'  "),
            ["SET", "k", "\"a b\"", "'c'"]
        );
        assert_eq!(values(r#"SET "a b" 'c d'"#), [&b"SET"[..], b"a b", b"c d"]);
        let words = split("GET ü🦀 x");
        assert_eq!(words.words[1].span, 4..10);
        assert_eq!(words.words[2].span, 11..12);
        assert!(split("   ").words.is_empty());
        assert!(split("").words.is_empty());
    }

    #[test]
    fn reads_unfinished_lines() {
        let words = split(r#"SET "hello wor"#);
        assert_eq!(words.error, Some("unbalanced quotes"));
        let last = words.words.last().unwrap();
        assert!(last.open && last.quoted);
        assert_eq!(last.value, b"hello wor");
        assert_eq!(last.span, 4..14);

        // A trailing backslash inside double quotes escapes nothing yet.
        let words = split(r#"GET "a\"#);
        assert!(words.words[1].open);
        assert_eq!(words.words[1].value, b"a");

        // Text right after a closing quote starts a new word.
        let words = split(r#"GET "a"b c"#);
        assert_eq!(
            words.error,
            Some("a closing quote must be followed by a space")
        );
        assert_eq!(spans(r#"GET "a"b c"#), ["GET", "\"a\"", "b", "c"]);
    }

    #[test]
    fn finds_the_word_at_the_cursor() {
        let words = split("HGET  key");
        assert_eq!(words.at(0).map(|(ix, _)| ix), Some(0));
        assert_eq!(words.at(4).map(|(ix, _)| ix), Some(0), "right after it");
        assert_eq!(words.at(5), None, "between words");
        assert_eq!(words.before(5), 1);
        assert_eq!(words.at(6).map(|(ix, _)| ix), Some(1));
        assert_eq!(words.at(9).map(|(ix, _)| ix), Some(1));
        let words = split("GET ");
        assert_eq!(words.at(4), None);
        assert_eq!(words.before(4), 1);
    }

    proptest! {
        /// Whatever `split_args` accepts, `split` reads the same way, and it
        /// flags exactly the lines `split_args` refuses.
        #[test]
        fn agrees_with_split_args(line in r#"[a-z "'\\ \tx0-9fü]{0,24}"#) {
            let words = split(&line);
            match split_args(&line) {
                Ok(args) => {
                    prop_assert_eq!(words.error, None);
                    prop_assert_eq!(values(&line), args);
                }
                Err(err) => prop_assert_eq!(words.error, Some(err)),
            }
        }

        #[test]
        fn spans_are_ordered_and_on_boundaries(line in "\\PC{0,30}") {
            let words = split(&line);
            let mut end = 0;
            for word in &words.words {
                prop_assert!(end <= word.span.start && word.span.start < word.span.end);
                prop_assert!(line.is_char_boundary(word.span.start));
                prop_assert!(line.is_char_boundary(word.span.end));
                let text = &line[word.span.clone()];
                prop_assert!(!text.starts_with(char::is_whitespace));
                end = word.span.end;
            }
            prop_assert!(end <= line.len());
        }

        /// Quoted arguments read back as the same bytes, with spans that
        /// cover exactly the quoted text.
        #[test]
        fn quoted_arguments_read_back(args in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..12), 1..5)) {
            let quoted: Vec<String> = args.iter().map(|a| quote_arg(a)).collect();
            let line = quoted.join(" ");
            let words = split(&line);
            prop_assert_eq!(words.error, None);
            prop_assert_eq!(values(&line), args);
            let shown: Vec<&str> = words.words.iter().map(|w| &line[w.span.clone()]).collect();
            prop_assert_eq!(shown, quoted.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
}
