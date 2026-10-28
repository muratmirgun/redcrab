//! The memory analysis against real servers: every key of a database
//! measured, or a sample of them, stopping, and a cluster walked primary by
//! primary.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400) database `ANALYSIS_DB`, which it owns, and the
//! cluster with keys under its own prefix.

#[macro_use]
mod common;

use std::sync::{Arc, Mutex};

use common::*;
use futures::StreamExt as _;
use redcrab_core::{
    AnalysisProgress, ConnectionConfig, KeyType,
    analysis::{Analysis, Totals},
    progress::Updates,
};

/// Reads the updates to the end, and the analysis then.
fn finish(walk: (Arc<Mutex<Analysis>>, Updates<AnalysisProgress>)) -> (Analysis, AnalysisProgress) {
    let (analysis, updates) = walk;
    let last = block_on(updates.collect::<Vec<_>>()).pop().unwrap();
    let analysis = analysis.lock().unwrap().clone();
    (analysis, last)
}

#[test]
fn a_database_is_measured_key_by_key() {
    let Some(t) = it!() else { return };
    let mut con = t.seed(Server::Standalone, ANALYSIS_DB);
    redis::cmd("FLUSHDB").query::<()>(&mut con).unwrap();
    let big = "x".repeat(100_000);
    redis::cmd("SET")
        .arg(t.key("big"))
        .arg(&big)
        .query::<()>(&mut con)
        .unwrap();
    for id in 0..20 {
        redis::cmd("HSET")
            .arg(t.key(&format!("users:{id}")))
            .arg("name")
            .arg("ada")
            .query::<()>(&mut con)
            .unwrap();
    }
    for id in 0..5 {
        redis::cmd("SET")
            .arg(t.key(&format!("sessions:{id}")))
            .arg("s")
            .arg("EX")
            .arg(600)
            .query::<()>(&mut con)
            .unwrap();
    }
    // Read-only is enough: the analysis only reads.
    let client = t.connect(ConnectionConfig {
        db: ANALYSIS_DB,
        read_only: true,
        ..standalone()
    });

    let (all, last) = finish(client.analyze_memory(1));
    assert_eq!(last, AnalysisProgress::Done);
    assert_eq!((all.scanned, all.measured.keys), (26, 26));
    assert_eq!(all.without_ttl.keys, 21);
    assert!(all.elapsed.is_some());
    let biggest = all.biggest();
    assert_eq!(biggest[0].key, t.key("big").as_bytes());
    assert_eq!(biggest[0].kind, KeyType::String);
    assert!(biggest[0].bytes > 100_000);
    let types = all.by_type();
    assert_eq!(types[0].0, KeyType::String);
    assert!(
        types
            .iter()
            .any(|(kind, totals)| *kind == KeyType::Hash && totals.keys == 20)
    );
    // it:<test>:<pid>:<n>:users:<id>, three levels deep: it, it:<test>,
    // it:<test>:<pid>.
    let top = all.prefixes(b"");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].totals.keys, 26);

    // One key in two.
    let (half, last) = finish(client.analyze_memory(2));
    assert_eq!(last, AnalysisProgress::Done);
    assert_eq!(half.scanned, 26);
    assert_eq!(half.measured.keys, 13);
    assert_eq!(
        half.measured.scaled(half.every()),
        Totals {
            keys: 26,
            bytes: half.measured.bytes * 2
        }
    );

    // Stopped before it began.
    let (analysis, updates) = client.analyze_memory(1);
    updates.stopper().stop();
    let (stopped, last) = finish((analysis, updates));
    assert_eq!(last, AnalysisProgress::Stopped);
    assert_eq!(stopped.measured.keys, 0);
}

#[test]
fn a_cluster_is_walked_primary_by_primary() {
    let Some(t) = it!() else { return };
    let mut con = t.seed_cluster();
    for id in 0..30 {
        redis::cmd("SET")
            .arg(t.key(&format!("k{id}")))
            .arg(id)
            .query::<()>(&mut con)
            .unwrap();
    }
    let client = t.connect(cluster());
    let (analysis, last) = finish(client.analyze_memory(1));
    assert_eq!(last, AnalysisProgress::Done);
    // Other tests' keys share the cluster: look at this test's own.
    let prefix = t.prefix().trim_end_matches(':');
    let mut parts = prefix.splitn(4, ':');
    let under = format!("{}:{}", parts.next().unwrap(), parts.next().unwrap());
    let pid = parts.next().unwrap();
    let ours = analysis
        .prefixes(under.as_bytes())
        .into_iter()
        .find(|row| row.prefix.as_deref() == Some(format!("{under}:{pid}").as_bytes()))
        .expect("the test's keys are in the tree");
    assert!(ours.totals.keys >= 30);
}
