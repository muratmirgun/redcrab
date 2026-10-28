//! The memory analysis's walk: SCAN every node, and for each key measured
//! ask TYPE, PTTL and MEMORY USAGE in the round trip after the page that
//! named it. Only reads, so a read-only connection can run it.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use redis::{Value, cmd, pipe};

use super::{
    Client,
    bulk::Distinct,
    cluster::unowned_error,
    conn::{CursorStep, NodeCursor},
};
use crate::{
    KeyType,
    analysis::Analysis,
    progress::{self, Updates},
    runtime::runtime,
    value::into_string,
};

/// SCAN's COUNT for the walk.
const SCAN_COUNT: u32 = 1000;
/// Elements MEMORY USAGE samples in a collection: its default, cheap on
/// big keys, close enough for ranking them.
const MEMORY_SAMPLES: u32 = 5;

/// How a memory analysis goes. The figures are in its [`Analysis`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisProgress {
    /// Walking: keys gone past and keys measured so far.
    Running {
        scanned: u64,
        measured: u64,
    },
    Done,
    /// Stopped when asked; what it found so far stays.
    Stopped,
    /// The walk could not go on; what it found so far stays.
    Failed(String),
}

impl Client {
    /// Walks the keyspace and measures one key in `every` (1: all of them)
    /// with MEMORY USAGE. The analysis fills while the walk goes on: read
    /// it whenever an update comes. Dropping the updates ends the walk;
    /// their stopper ends it with [`AnalysisProgress::Stopped`].
    pub fn analyze_memory(&self, every: u32) -> (Arc<Mutex<Analysis>>, Updates<AnalysisProgress>) {
        let conn = self.conn.clone();
        let analysis = Arc::new(Mutex::new(Analysis::new(
            Some(self.config.separator_byte()),
            every,
        )));
        let every = u64::from(every.max(1));
        let shared = analysis.clone();
        let (reporter, updates) = progress::channel();
        runtime().spawn(async move {
            let started = Instant::now();
            let lock = || shared.lock().unwrap_or_else(|e| e.into_inner());
            let fail = |error: String| AnalysisProgress::Failed(error);
            let mut distinct = Distinct::default();
            let mut seen = 0u64;
            let (nodes, unowned) = conn.walk_nodes().await;
            for mut con in nodes {
                let mut cursor = NodeCursor::start(&con);
                loop {
                    if reporter.is_abandoned() {
                        return;
                    }
                    if reporter.stop_requested() {
                        reporter.update(AnalysisProgress::Stopped);
                        return;
                    }
                    let page = cmd("SCAN")
                        .arg(cursor.at())
                        .arg("COUNT")
                        .arg(SCAN_COUNT)
                        .query_async::<(u64, Vec<Vec<u8>>)>(&mut con)
                        .await;
                    let (next, keys) = match page {
                        Ok(page) => page,
                        Err(err) => return reporter.update(fail(format!("SCAN failed: {err}"))),
                    };
                    let step = match cursor.advance(next, con.epoch()) {
                        Ok(CursorStep::Restarted) => {
                            return reporter.update(fail(
                                "the server changed (a failover) during the analysis; run it again"
                                    .into(),
                            ));
                        }
                        Ok(step) => step,
                        Err(err) => return reporter.update(fail(format!("{err:#}"))),
                    };
                    let mut measure = Vec::new();
                    for key in keys {
                        if distinct.insert(&key) {
                            seen += 1;
                            if (seen - 1).is_multiple_of(every) {
                                measure.push(key);
                            }
                        }
                    }
                    if !measure.is_empty() {
                        let mut ask = pipe();
                        for key in &measure {
                            ask.cmd("TYPE").arg(key);
                            ask.cmd("PTTL").arg(key);
                            ask.cmd("MEMORY")
                                .arg("USAGE")
                                .arg(key)
                                .arg("SAMPLES")
                                .arg(MEMORY_SAMPLES);
                        }
                        let replies: Vec<Value> =
                            match ask.ignore_errors().query_async(&mut con).await {
                                Ok(replies) => replies,
                                Err(err) => {
                                    return reporter
                                        .update(fail(format!("MEMORY USAGE failed: {err}")));
                                }
                            };
                        let mut analysis = lock();
                        for (key, replies) in measure.iter().zip(replies.chunks(3)) {
                            let [kind, ttl, bytes] = replies else {
                                continue;
                            };
                            let bytes = match bytes {
                                Value::Int(n) => u64::try_from(*n).unwrap_or(0),
                                // Gone since SCAN named it.
                                Value::Nil => continue,
                                Value::ServerError(err) => {
                                    drop(analysis);
                                    return reporter.update(fail(format!(
                                        "MEMORY USAGE failed: {}{}",
                                        err.code(),
                                        err.details().map(|d| format!(" {d}")).unwrap_or_default()
                                    )));
                                }
                                _ => continue,
                            };
                            let kind = KeyType::parse(&into_string(kind.clone()));
                            if kind == KeyType::None {
                                continue;
                            }
                            let ttl_ms = match ttl {
                                Value::Int(ms) if *ms >= 0 => Some(*ms as u64),
                                _ => None,
                            };
                            analysis.add(key, kind, ttl_ms, bytes);
                        }
                    }
                    let (scanned, measured) = {
                        let mut analysis = lock();
                        analysis.scanned = seen;
                        (analysis.scanned, analysis.measured.keys)
                    };
                    reporter.update(AnalysisProgress::Running { scanned, measured });
                    if step == CursorStep::Done {
                        break;
                    }
                }
                // A key lives on one node only.
                distinct.forget();
            }
            lock().elapsed = Some(started.elapsed());
            if unowned > 0 {
                return reporter.update(fail(format!("{:#}", unowned_error(unowned))));
            }
            reporter.update(AnalysisProgress::Done);
        });
        (analysis, updates)
    }
}
