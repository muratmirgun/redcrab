//! The bulk-delete flow every front end walks the user through: count the
//! keys of a folder or pattern, have the user confirm the exact pattern and
//! count, delete with progress and a way to stop, then bring the key list
//! up to date.
//!
//! No I/O happens here, as in [`crate::browser`]. The front end runs
//! [`Client::count_matching`] on [`BulkDelete::pattern`] and feeds its events
//! to [`BulkDelete::on_count`]; once the user agrees, [`BulkDelete::confirm`]
//! says to start [`Client::delete_matching`], whose events go to
//! [`BulkDelete::on_delete`].
//!
//! When the user stops, [`BulkDelete::stop`] says what to do with the walk.
//! A count is simply dropped. A delete is asked to stop with its
//! [`Stopper`] and its events are fed on: only the walk knows how many keys
//! went (its last round trip is still on its way when the user clicks), and
//! it says so in [`DeleteProgress::Stopped`]. When the flow is over,
//! [`BulkDelete::refresh`] says how to update the key list.
//!
//! [`Client::count_matching`]: crate::Client::count_matching
//! [`Client::delete_matching`]: crate::Client::delete_matching
//! [`Stopper`]: crate::progress::Stopper

use std::{borrow::Cow, time::Duration};

use crate::{
    browser::{folder_pattern, is_match_all},
    client::{DeleteProgress, DeleteStats, Key, ScanProgress},
    format,
};

/// The keys to delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Every key in a folder of the key tree: `prefix`, the separator, then
    /// anything. Not the key `prefix` itself, which is a row of its own.
    Folder { prefix: Key, separator: u8 },
    /// Every key matching a glob pattern, such as the key list's filter.
    Pattern(Vec<u8>),
}

impl Target {
    /// The MATCH pattern for the target's keys.
    pub fn pattern(&self) -> Vec<u8> {
        match self {
            Self::Folder { prefix, separator } => folder_pattern(prefix, *separator),
            Self::Pattern(pattern) => pattern.clone(),
        }
    }
}

/// Where the flow is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Counting the keys; `keys` so far.
    Counting {
        keys: u64,
    },
    /// Nothing matches, so there is nothing to confirm.
    Empty,
    /// Waiting for the user to confirm deleting `keys` keys (about that many
    /// unless `exact`).
    Confirm {
        keys: u64,
        exact: bool,
    },
    Deleting(DeleteStats),
    /// The user stopped the delete; the walk finishes the round trip it
    /// has in flight and then reports everything it did. The stats are the
    /// latest it sent before that.
    Stopping(DeleteStats),
    /// The user stopped before anything was deleted.
    Cancelled,
    /// The count failed; nothing was deleted.
    CountFailed(String),
    /// The flow would not start: the pattern matches every key (see
    /// [`is_match_all`]), and emptying a database is not a bulk delete.
    Refused(String),
    /// The delete is over: what it did, and how it ended.
    Finished {
        stats: DeleteStats,
        ending: Ending,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Every matching key was walked, in this long.
    Completed(Duration),
    /// The user stopped it.
    Stopped,
    /// An error ended it.
    Failed(String),
}

/// How the key list catches up with a finished delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Every key of the folder is gone: drop them from the tree
    /// ([`KeyBrowser::remove_folder`](crate::browser::KeyBrowser::remove_folder)),
    /// keeping its open folders and scroll position.
    RemoveFolder(Key),
    /// Some keys went: scan again to see which are left.
    Rescan,
}

pub struct BulkDelete {
    target: Target,
    pattern: Vec<u8>,
    phase: Phase,
    /// The count the user confirmed, for the progress of the delete.
    confirmed: u64,
}

impl BulkDelete {
    /// Starts at counting: run the count for [`Self::pattern`] right away.
    /// A pattern that matches every key (`*`, `**`, `?*`) is refused
    /// instead: the flow starts over ([`Phase::Refused`]), and no count or
    /// delete is to be run for it.
    pub fn new(target: Target) -> Self {
        let pattern = target.pattern();
        let phase = if is_match_all(&pattern) {
            Phase::Refused(format!(
                "{} matches every key in the database; filter by something narrower",
                format::copyable(&pattern)
            ))
        } else {
            Phase::Counting { keys: 0 }
        };
        Self {
            target,
            pattern,
            phase,
            confirmed: 0,
        }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The MATCH pattern to count and delete.
    pub fn pattern(&self) -> &[u8] {
        &self.pattern
    }

    /// The pattern as the user should read it: as it is when it is text,
    /// else quoted the way the console reads it back.
    pub fn pattern_text(&self) -> Cow<'_, str> {
        format::copyable(&self.pattern)
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// "Delete folder user", "Delete matching keys".
    pub fn title(&self) -> String {
        match &self.target {
            Target::Folder { prefix, .. } => {
                format!("Delete folder {}", format::copyable(prefix))
            }
            Target::Pattern(_) => "Delete matching keys".into(),
        }
    }

    /// Whether a walk is running: one [`Self::stop`] would end, or one
    /// that is stopping.
    pub fn is_running(&self) -> bool {
        matches!(
            self.phase,
            Phase::Counting { .. } | Phase::Deleting(_) | Phase::Stopping(_)
        )
    }

    /// Whether the flow has ended, one way or another.
    pub fn is_over(&self) -> bool {
        matches!(
            self.phase,
            Phase::Empty
                | Phase::Cancelled
                | Phase::CountFailed(_)
                | Phase::Refused(_)
                | Phase::Finished { .. }
        )
    }

    /// Applies an event of the count. Returns whether anything changed;
    /// events after the count (a late one, after a stop) change nothing.
    pub fn on_count(&mut self, event: ScanProgress) -> bool {
        if !matches!(self.phase, Phase::Counting { .. }) {
            return false;
        }
        self.phase = match event {
            ScanProgress::Counting { keys } => Phase::Counting { keys },
            ScanProgress::Done { keys: 0, .. } => Phase::Empty,
            ScanProgress::Done { keys, exact, .. } => Phase::Confirm { keys, exact },
            ScanProgress::Failed { error, .. } => Phase::CountFailed(error),
        };
        true
    }

    /// The user confirmed. Returns whether to start the delete now: only
    /// once, and only from [`Phase::Confirm`].
    pub fn confirm(&mut self) -> bool {
        let Phase::Confirm { keys, .. } = self.phase else {
            return false;
        };
        self.confirmed = keys;
        self.phase = Phase::Deleting(DeleteStats::default());
        true
    }

    /// Applies an event of the delete, also while it is stopping. Returns
    /// whether anything changed.
    pub fn on_delete(&mut self, event: DeleteProgress) -> bool {
        let stopping = match self.phase {
            Phase::Deleting(_) => false,
            Phase::Stopping(_) => true,
            _ => return false,
        };
        self.phase = match event {
            DeleteProgress::Deleting(stats) if stopping => Phase::Stopping(stats),
            DeleteProgress::Deleting(stats) => Phase::Deleting(stats),
            // The walk may have got to the end before it saw the stop.
            DeleteProgress::Done { stats, elapsed } => Phase::Finished {
                stats,
                ending: Ending::Completed(elapsed),
            },
            DeleteProgress::Stopped { stats } => Phase::Finished {
                stats,
                ending: Ending::Stopped,
            },
            DeleteProgress::Failed { stats, error } => Phase::Finished {
                stats,
                ending: Ending::Failed(error),
            },
        };
        true
    }

    /// The user stopped. Returns what the front end does with the walk.
    pub fn stop(&mut self) -> Stop {
        match &self.phase {
            Phase::Counting { .. } | Phase::Confirm { .. } => {
                self.phase = Phase::Cancelled;
                Stop::Cancelled
            }
            Phase::Deleting(stats) => {
                self.phase = Phase::Stopping(stats.clone());
                Stop::Ask
            }
            _ => Stop::Nothing,
        }
    }

    /// How far the delete is, from 0 to 1, against the count the user
    /// confirmed (keys created since can make it reach 1 early).
    pub fn progress(&self) -> Option<f32> {
        let (Phase::Deleting(stats) | Phase::Stopping(stats)) = &self.phase else {
            return None;
        };
        let done = (stats.deleted + stats.failed) as f64;
        Some((done / self.confirmed.max(1) as f64).min(1.0) as f32)
    }

    /// What to tell the user about where the flow is, in one sentence.
    pub fn message(&self) -> String {
        let pattern = self.pattern_text();
        match &self.phase {
            Phase::Counting { keys: 0 } => format!("Counting the keys matching {pattern}…"),
            Phase::Counting { keys } => format!(
                "Counting the keys matching {pattern}… {} so far",
                format::count(*keys)
            ),
            Phase::Empty => format!("No keys match {pattern}."),
            Phase::Confirm { keys, exact } => format!(
                "Delete {}{} matching {pattern}? This cannot be undone.",
                if *exact { "" } else { "about " },
                keys_text(*keys)
            ),
            Phase::Deleting(stats) => {
                let mut text = format!(
                    "Deleting… {} of {}",
                    format::count(stats.deleted),
                    keys_text(self.confirmed)
                );
                if stats.failed > 0 {
                    text.push_str(&format!(", {} skipped", format::count(stats.failed)));
                }
                text
            }
            Phase::Stopping(stats) => {
                format!("Stopping… {} deleted so far", keys_text(stats.deleted))
            }
            Phase::Cancelled => "Nothing was deleted.".into(),
            Phase::CountFailed(error) => format!("Could not count the keys: {error}"),
            Phase::Refused(why) => format!("Nothing was deleted: {why}."),
            Phase::Finished { stats, ending } => {
                let deleted = keys_text(stats.deleted);
                let mut text = match ending {
                    Ending::Completed(elapsed) => {
                        format!("Deleted {deleted} in {}.", duration_text(*elapsed))
                    }
                    Ending::Stopped if stats.deleted == 0 => {
                        "Stopped before any key was deleted.".into()
                    }
                    Ending::Stopped => format!("Stopped after deleting {deleted}."),
                    Ending::Failed(error) => {
                        format!("Deleting stopped after {deleted}: {error}")
                    }
                };
                if stats.failed > 0 {
                    text.push_str(&format!(
                        " {} could not be deleted: {}",
                        keys_text(stats.failed),
                        stats.first_error.as_deref().unwrap_or("refused")
                    ));
                }
                text
            }
        }
    }

    /// The label of the button that confirms: "Delete 12,345 keys".
    pub fn confirm_label(&self) -> Option<String> {
        let Phase::Confirm { keys, exact } = self.phase else {
            return None;
        };
        let about = if exact { "" } else { "about " };
        Some(format!("Delete {about}{}", keys_text(keys)))
    }

    /// What the confirmation should also say: the walk deletes what
    /// matches when it runs, not a list fixed by the count.
    pub fn confirm_note(&self) -> Option<&'static str> {
        matches!(self.phase, Phase::Confirm { .. })
            .then_some("Keys that start to match before the delete ends are deleted too.")
    }

    /// How the key list should catch up once the flow is over; `None` when
    /// nothing changed.
    pub fn refresh(&self) -> Option<Refresh> {
        let Phase::Finished { stats, ending } = &self.phase else {
            return None;
        };
        match (&self.target, ending) {
            // Every key of the folder is gone, whoever deleted it.
            (Target::Folder { prefix, .. }, Ending::Completed(_)) if stats.failed == 0 => {
                Some(Refresh::RemoveFolder(prefix.clone()))
            }
            (_, Ending::Completed(_)) if stats.deleted == 0 && stats.failed == 0 => None,
            // A walk that ended early always scans again, whatever its
            // stats say: after a lost connection the keys of its last round
            // trip may be gone or not, and it is only one scan.
            _ => Some(Refresh::Rescan),
        }
    }
}

/// What the front end does with a walk the user stopped
/// ([`BulkDelete::stop`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Nothing changed: the flow is over, or already stopping.
    Nothing,
    /// The flow is over with nothing deleted: drop the count, if it still
    /// runs (a count has nothing to undo).
    Cancelled,
    /// The delete: ask it to stop with its
    /// [`Stopper`](crate::progress::Stopper), and keep feeding its events
    /// to [`BulkDelete::on_delete`] until it says how far it got.
    Ask,
}

/// "1 key", "12,345 keys".
fn keys_text(n: u64) -> String {
    match n {
        1 => "1 key".into(),
        n => format!("{} keys", format::count(n)),
    }
}

/// "850 ms", "12.3 s".
fn duration_text(elapsed: Duration) -> String {
    let ms = elapsed.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.1} s", ms / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(prefix: &str) -> BulkDelete {
        BulkDelete::new(Target::Folder {
            prefix: Key::from(prefix.as_bytes()),
            separator: b':',
        })
    }

    fn done(keys: u64) -> ScanProgress {
        ScanProgress::Done {
            keys,
            elapsed: Duration::from_millis(3),
            exact: true,
        }
    }

    fn stats(deleted: u64, failed: u64) -> DeleteStats {
        DeleteStats {
            scanned: deleted + failed,
            deleted,
            failed,
            first_error: (failed > 0).then(|| "NOPERM No permissions to access a key".into()),
        }
    }

    #[test]
    fn walks_a_folder_delete() {
        let mut flow = folder("user");
        assert_eq!(flow.pattern(), b"user:*");
        assert_eq!(flow.title(), "Delete folder user");
        assert!(flow.is_running() && !flow.is_over());
        assert_eq!(flow.message(), "Counting the keys matching user:*…");
        assert!(!flow.confirm(), "nothing to confirm while counting");

        assert!(flow.on_count(ScanProgress::Counting { keys: 1200 }));
        assert_eq!(
            flow.message(),
            "Counting the keys matching user:*… 1,200 so far"
        );
        assert!(flow.on_count(done(12_345)));
        assert_eq!(
            flow.phase(),
            &Phase::Confirm {
                keys: 12_345,
                exact: true
            }
        );
        assert_eq!(
            flow.message(),
            "Delete 12,345 keys matching user:*? This cannot be undone."
        );
        assert!(flow.confirm_note().is_some());
        assert_eq!(flow.confirm_label().as_deref(), Some("Delete 12,345 keys"));
        assert!(!flow.is_running());
        assert!(!flow.on_count(done(1)), "the count is over");

        assert!(flow.confirm());
        assert!(!flow.confirm(), "only once");
        assert!(flow.confirm_note().is_none());
        assert_eq!(flow.confirm_label(), None);
        assert_eq!(flow.progress(), Some(0.0));
        assert!(flow.on_delete(DeleteProgress::Deleting(stats(6000, 0))));
        assert_eq!(flow.message(), "Deleting… 6,000 of 12,345 keys");
        let half = flow.progress().unwrap();
        assert!((0.48..0.49).contains(&half), "{half}");
        assert_eq!(flow.refresh(), None, "not over yet");

        assert!(flow.on_delete(DeleteProgress::Done {
            stats: stats(12_345, 0),
            elapsed: Duration::from_millis(1234),
        }));
        assert!(flow.is_over() && !flow.is_running());
        assert_eq!(flow.message(), "Deleted 12,345 keys in 1.2 s.");
        assert_eq!(flow.progress(), None);
        assert_eq!(
            flow.refresh(),
            Some(Refresh::RemoveFolder(Key::from(&b"user"[..])))
        );
        assert!(!flow.on_delete(DeleteProgress::Deleting(stats(1, 0))));
        assert_eq!(flow.stop(), Stop::Nothing, "nothing to stop");
    }

    #[test]
    fn a_pattern_rescans_after_deleting() {
        let mut flow = BulkDelete::new(Target::Pattern(b"session:*".to_vec()));
        assert_eq!(flow.title(), "Delete matching keys");
        flow.on_count(done(1));
        assert_eq!(
            flow.message(),
            "Delete 1 key matching session:*? This cannot be undone."
        );
        flow.confirm();
        flow.on_delete(DeleteProgress::Done {
            stats: stats(1, 0),
            elapsed: Duration::from_millis(2),
        });
        assert_eq!(flow.message(), "Deleted 1 key in 2 ms.");
        assert_eq!(flow.refresh(), Some(Refresh::Rescan));
    }

    #[test]
    fn patterns_matching_every_key_are_refused() {
        for pattern in ["*", "**", "?*", "*?", "*??*"] {
            let mut flow = BulkDelete::new(Target::Pattern(pattern.into()));
            assert!(
                matches!(flow.phase(), Phase::Refused(_)),
                "{pattern}: {:?}",
                flow.phase()
            );
            assert!(flow.is_over() && !flow.is_running());
            assert_eq!(
                flow.message(),
                format!(
                    "Nothing was deleted: {pattern} matches every key in the database; filter by something narrower."
                )
            );
            // Nothing can move it on to a delete.
            assert!(!flow.on_count(done(5)));
            assert!(!flow.confirm());
            assert_eq!(flow.stop(), Stop::Nothing);
            assert_eq!((flow.confirm_label(), flow.refresh()), (None, None));
        }
        // Narrower patterns, and folders (whose prefix is literal), count.
        for pattern in ["a*", "*a*", "?", "[ab]*", "\\**", ""] {
            let flow = BulkDelete::new(Target::Pattern(pattern.into()));
            assert_eq!(flow.phase(), &Phase::Counting { keys: 0 }, "{pattern}");
        }
        let flow = BulkDelete::new(Target::Folder {
            prefix: Key::from(&b"*"[..]),
            separator: b':',
        });
        assert_eq!(flow.phase(), &Phase::Counting { keys: 0 });
    }

    #[test]
    fn nothing_to_delete() {
        let mut flow = folder("user");
        flow.on_count(done(0));
        assert_eq!(flow.phase(), &Phase::Empty);
        assert_eq!(flow.message(), "No keys match user:*.");
        assert!(flow.is_over());
        assert!(!flow.confirm());
        assert_eq!(flow.refresh(), None);
    }

    #[test]
    fn stopping() {
        // While counting, or at the confirmation: nothing was deleted.
        for confirm in [false, true] {
            let mut flow = folder("user");
            if confirm {
                flow.on_count(done(5));
            }
            assert_eq!(flow.stop(), Stop::Cancelled);
            assert_eq!(flow.phase(), &Phase::Cancelled);
            assert_eq!(flow.message(), "Nothing was deleted.");
            assert_eq!(flow.refresh(), None);
            assert_eq!(flow.stop(), Stop::Nothing);
            // A late count event changes nothing.
            assert!(!flow.on_count(done(5)));
        }

        // While deleting: the walk is asked to stop, and the flow waits
        // for it to say how far it got. What the front end saw last is
        // behind: the walk had more round trips answered by then, and one
        // more in flight.
        let mut flow = folder("user");
        flow.on_count(done(10_000));
        flow.confirm();
        flow.on_delete(DeleteProgress::Deleting(stats(1000, 0)));
        assert_eq!(flow.stop(), Stop::Ask);
        assert_eq!(flow.phase(), &Phase::Stopping(stats(1000, 0)));
        assert!(flow.is_running() && !flow.is_over());
        assert_eq!(flow.message(), "Stopping… 1,000 keys deleted so far");
        assert_eq!(flow.progress(), Some(0.1));
        assert_eq!(flow.refresh(), None, "not over yet");
        assert_eq!(flow.stop(), Stop::Nothing, "already stopping");
        assert!(flow.confirm_label().is_none() && !flow.confirm());

        assert!(flow.on_delete(DeleteProgress::Deleting(stats(4000, 0))));
        assert_eq!(flow.phase(), &Phase::Stopping(stats(4000, 0)));
        assert!(flow.on_delete(DeleteProgress::Stopped {
            stats: stats(5000, 0)
        }));
        assert!(flow.is_over() && !flow.is_running());
        assert_eq!(
            flow.phase(),
            &Phase::Finished {
                stats: stats(5000, 0),
                ending: Ending::Stopped
            }
        );
        assert_eq!(flow.message(), "Stopped after deleting 5,000 keys.");
        // The folder is not all gone: scan to see what is left.
        assert_eq!(flow.refresh(), Some(Refresh::Rescan));
        assert!(!flow.on_delete(DeleteProgress::Deleting(stats(8000, 0))));

        // Stopped before the first key went, as the walk says: the list is
        // scanned again all the same.
        let mut flow = folder("user");
        flow.on_count(done(10));
        flow.confirm();
        assert_eq!(flow.stop(), Stop::Ask);
        assert_eq!(flow.message(), "Stopping… 0 keys deleted so far");
        flow.on_delete(DeleteProgress::Stopped { stats: stats(0, 0) });
        assert_eq!(flow.message(), "Stopped before any key was deleted.");
        assert_eq!(flow.refresh(), Some(Refresh::Rescan));

        // The walk got to the end before it saw the stop: it completed.
        let mut flow = folder("user");
        flow.on_count(done(10));
        flow.confirm();
        flow.stop();
        flow.on_delete(DeleteProgress::Done {
            stats: stats(10, 0),
            elapsed: Duration::from_millis(5),
        });
        assert_eq!(flow.message(), "Deleted 10 keys in 5 ms.");
        assert_eq!(
            flow.refresh(),
            Some(Refresh::RemoveFolder(Key::from(&b"user"[..])))
        );

        // Or failed.
        let mut flow = folder("user");
        flow.on_count(done(10));
        flow.confirm();
        flow.stop();
        flow.on_delete(DeleteProgress::Failed {
            stats: stats(3, 0),
            error: "broken pipe; up to 7 more keys may have been deleted".into(),
        });
        assert_eq!(
            flow.message(),
            "Deleting stopped after 3 keys: broken pipe; up to 7 more keys may have been deleted"
        );
        assert_eq!(flow.refresh(), Some(Refresh::Rescan));
    }

    #[test]
    fn failures() {
        let mut flow = folder("user");
        flow.on_count(ScanProgress::Failed {
            keys: 3,
            error: "NOPERM User app has no permissions to run the 'scan' command".into(),
        });
        assert!(flow.is_over());
        assert_eq!(
            flow.message(),
            "Could not count the keys: NOPERM User app has no permissions to run the 'scan' command"
        );

        // Keys refused one by one: the folder is not all gone.
        let mut flow = folder("user");
        flow.on_count(done(10));
        flow.confirm();
        flow.on_delete(DeleteProgress::Deleting(stats(2, 1)));
        assert_eq!(flow.message(), "Deleting… 2 of 10 keys, 1 skipped");
        flow.on_delete(DeleteProgress::Done {
            stats: stats(8, 2),
            elapsed: Duration::from_millis(20),
        });
        assert_eq!(
            flow.message(),
            "Deleted 8 keys in 20 ms. 2 keys could not be deleted: NOPERM No permissions to access a key"
        );
        assert_eq!(flow.refresh(), Some(Refresh::Rescan));

        // An error ends the walk.
        let mut flow = folder("user");
        flow.on_count(done(10));
        flow.confirm();
        flow.on_delete(DeleteProgress::Failed {
            stats: stats(0, 0),
            error: "READONLY You can't write against a read only replica.".into(),
        });
        assert_eq!(
            flow.message(),
            "Deleting stopped after 0 keys: READONLY You can't write against a read only replica."
        );
        // A walk that failed may have deleted keys of its last round trip
        // that its stats do not count.
        assert_eq!(flow.refresh(), Some(Refresh::Rescan));

        // A walk that completed without deleting anything changed nothing.
        let mut flow = BulkDelete::new(Target::Pattern(b"gone:*".to_vec()));
        flow.on_count(done(3));
        flow.confirm();
        flow.on_delete(DeleteProgress::Done {
            stats: DeleteStats {
                scanned: 3,
                ..DeleteStats::default()
            },
            elapsed: Duration::from_millis(1),
        });
        assert_eq!(flow.refresh(), None);
    }

    #[test]
    fn counts_that_are_not_exact_say_so() {
        let mut flow = folder("user");
        flow.on_count(ScanProgress::Done {
            keys: 2_000_001,
            elapsed: Duration::from_secs(1),
            exact: false,
        });
        assert_eq!(
            flow.message(),
            "Delete about 2,000,001 keys matching user:*? This cannot be undone."
        );
        assert_eq!(
            flow.confirm_label().as_deref(),
            Some("Delete about 2,000,001 keys")
        );
        // More keys than counted: progress stops at the end.
        flow.confirm();
        flow.on_delete(DeleteProgress::Deleting(stats(2_100_000, 0)));
        assert_eq!(flow.progress(), Some(1.0));
    }

    #[test]
    fn binary_prefixes_show_quoted() {
        let mut flow = BulkDelete::new(Target::Folder {
            prefix: Key::from(&b"bin\xff*"[..]),
            separator: b':',
        });
        assert_eq!(flow.pattern(), b"bin\xff\\*:*");
        assert_eq!(flow.title(), r#"Delete folder "bin\xff*""#);
        flow.on_count(done(2));
        assert_eq!(
            flow.message(),
            r#"Delete 2 keys matching "bin\xff\\*:*"? This cannot be undone."#
        );
    }
}
