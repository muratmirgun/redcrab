//! How a long walk (a count, a bulk delete) tells a front end how far it
//! is: the reader always gets the walk's latest state, never a backlog of
//! old ones, and the walk never waits for the reader.
//!
//! Each [`Reporter::update`] replaces the one before it if the reader has
//! not taken that yet, so a reader that looks fifteen times a second sees
//! the state of that moment, and the walk's last update (how it ended) is
//! never lost. Dropping [`Updates`] abandons the walk, which then ends
//! quietly at its next check; [`Stopper::stop`] asks it to end and still
//! say how far it got.

use std::{
    fmt,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use futures::Stream;

/// A channel for the progress of one walk.
pub(crate) fn channel<T>() -> (Reporter<T>, Updates<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            latest: None,
            waker: None,
            reporting: true,
            reading: true,
        }),
        stop: Arc::new(AtomicBool::new(false)),
    });
    (
        Reporter {
            shared: shared.clone(),
        },
        Updates { shared },
    )
}

struct Shared<T> {
    state: Mutex<State<T>>,
    /// Set by [`Stopper::stop`]; shared with the stoppers on its own, so
    /// that they need not know `T`.
    stop: Arc<AtomicBool>,
}

struct State<T> {
    /// The latest update, until the reader takes it.
    latest: Option<T>,
    /// The reader, waiting for an update.
    waker: Option<Waker>,
    /// The walk may still send updates.
    reporting: bool,
    /// Somebody still reads them.
    reading: bool,
}

impl<T> Shared<T> {
    fn state(&self) -> MutexGuard<'_, State<T>> {
        // The lock is only held to move a value in or out; a panic while
        // it was held left nothing half-done.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The walk's end of the channel.
pub(crate) struct Reporter<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Reporter<T> {
    /// Hands the reader `update`, in place of the last one if it has not
    /// taken that yet.
    pub(crate) fn update(&self, update: T) {
        let waker = {
            let mut state = self.shared.state();
            if !state.reading {
                return;
            }
            state.latest = Some(update);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Whether the reader went away, so that nobody hears the walk any
    /// more.
    pub(crate) fn is_abandoned(&self) -> bool {
        !self.shared.state().reading
    }

    /// Whether the reader asked the walk to stop ([`Stopper::stop`]).
    pub(crate) fn stop_requested(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }
}

impl<T> Drop for Reporter<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = self.shared.state();
            state.reporting = false;
            state.waker.take()
        };
        // The reader gets what is left, then the end of the stream.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The progress of a walk, as a stream of its latest states: each item is
/// the newest the walk had when it was taken, and the last is how the walk
/// ended. Dropping it abandons the walk.
#[must_use = "dropping the updates abandons the walk"]
pub struct Updates<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Updates<T> {
    /// A handle that asks the walk to stop and still report how far it got
    /// (keep reading until the stream ends), for when that matters, as it
    /// does for a delete. It can be kept after the updates move into a
    /// task that reads them.
    pub fn stopper(&self) -> Stopper {
        Stopper(self.shared.stop.clone())
    }
}

impl<T> Stream for Updates<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = self.shared.state();
        if let Some(update) = state.latest.take() {
            return Poll::Ready(Some(update));
        }
        if !state.reporting {
            return Poll::Ready(None);
        }
        match &mut state.waker {
            Some(waker) if waker.will_wake(cx.waker()) => {}
            slot => *slot = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl<T> fmt::Debug for Updates<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.shared.state();
        f.debug_struct("Updates")
            .field("pending", &state.latest.is_some())
            .field("reporting", &state.reporting)
            .field("stop_requested", &self.shared.stop.load(Ordering::Acquire))
            .finish()
    }
}

impl<T> Drop for Updates<T> {
    fn drop(&mut self) {
        let latest = {
            let mut state = self.shared.state();
            state.reading = false;
            state.waker = None;
            state.latest.take()
        };
        drop(latest);
    }
}

/// Asks a walk to stop after the round trip it has in flight, and to
/// report how far it got. See [`Updates::stopper`].
#[derive(Clone, Debug)]
pub struct Stopper(Arc<AtomicBool>);

impl Stopper {
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::AtomicUsize,
        task::{Wake, Waker},
        thread,
        time::Duration,
    };

    use futures::{StreamExt as _, executor::block_on};
    use proptest::prelude::*;

    use super::*;

    /// Counts its wake-ups.
    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll<T>(updates: &mut Updates<T>, waker: &Waker) -> Poll<Option<T>> {
        Pin::new(updates).poll_next(&mut Context::from_waker(waker))
    }

    #[test]
    fn the_reader_gets_the_latest_update_and_then_the_end() {
        let (reporter, mut updates) = channel();
        for n in 1..=5 {
            reporter.update(n);
        }
        assert_eq!(block_on(updates.next()), Some(5));
        reporter.update(6);
        // How the walk ended is the last update, and is never replaced.
        reporter.update(7);
        drop(reporter);
        assert_eq!(block_on(updates.next()), Some(7));
        assert_eq!(block_on(updates.next()), None);
        assert_eq!(block_on(updates.next()), None);
    }

    #[test]
    fn a_waiting_reader_is_woken() {
        let (reporter, mut updates) = channel();
        let count = Arc::new(Count::default());
        let waker = Waker::from(count.clone());
        assert_eq!(poll(&mut updates, &waker), Poll::Pending);
        assert_eq!(poll(&mut updates, &waker), Poll::Pending);
        reporter.update("a");
        reporter.update("b");
        assert_eq!(count.0.load(Ordering::SeqCst), 1, "woken once");
        assert_eq!(poll(&mut updates, &waker), Poll::Ready(Some("b")));
        assert_eq!(poll(&mut updates, &waker), Poll::Pending);
        drop(reporter);
        assert_eq!(count.0.load(Ordering::SeqCst), 2, "woken by the end");
        assert_eq!(poll(&mut updates, &waker), Poll::Ready(None));
    }

    #[test]
    fn a_reader_on_another_thread_sees_every_walk_end() {
        let (reporter, updates) = channel();
        let walk = thread::spawn(move || {
            for n in 0..10_000u32 {
                reporter.update(n);
                if n % 1000 == 0 {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        });
        let seen: Vec<u32> = block_on(updates.collect());
        walk.join().unwrap();
        assert_eq!(seen.last(), Some(&9_999));
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "in order, no repeats");
    }

    #[test]
    fn dropping_the_reader_abandons_the_walk() {
        let (reporter, updates) = channel();
        let stopper = updates.stopper();
        assert!(!reporter.is_abandoned());
        reporter.update(Arc::new(1));
        let held = Arc::new(2);
        reporter.update(held.clone());
        drop(updates);
        assert!(reporter.is_abandoned());
        assert_eq!(Arc::strong_count(&held), 1, "the update was dropped");
        // Updates nobody reads are not kept.
        reporter.update(held.clone());
        assert_eq!(Arc::strong_count(&held), 1);
        // Neither the walk nor a stopper kept around minds.
        stopper.stop();
        assert!(reporter.stop_requested());
    }

    #[test]
    fn a_stopper_asks_the_walk_to_stop() {
        let (reporter, mut updates) = channel();
        let stopper = updates.stopper();
        assert!(!reporter.stop_requested());
        stopper.clone().stop();
        assert!(reporter.stop_requested());
        // The walk says how far it got, and the reader still hears it.
        reporter.update("stopped after 3");
        drop(reporter);
        assert_eq!(block_on(updates.next()), Some("stopped after 3"));
        assert_eq!(block_on(updates.next()), None);
    }

    proptest! {
        /// However updates and reads interleave, the reader sees updates in
        /// the order they were sent, none twice, and always the last.
        #[test]
        fn readers_see_an_ordered_tail(steps in proptest::collection::vec(any::<bool>(), 0..200)) {
            let (reporter, mut updates) = channel();
            let waker = Waker::from(Arc::new(Count::default()));
            let mut sent = 0u32;
            let mut seen = Vec::new();
            for update in steps {
                if update {
                    sent += 1;
                    reporter.update(sent);
                } else if let Poll::Ready(Some(n)) = poll(&mut updates, &waker) {
                    seen.push(n);
                }
            }
            drop(reporter);
            while let Poll::Ready(Some(n)) = poll(&mut updates, &waker) {
                seen.push(n);
            }
            prop_assert!(seen.windows(2).all(|w| w[0] < w[1]), "{:?}", seen);
            prop_assert_eq!(seen.last().copied(), (sent > 0).then_some(sent));
            prop_assert_eq!(poll(&mut updates, &waker), Poll::Ready(None));
        }
    }
}
