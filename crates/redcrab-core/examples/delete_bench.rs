//! Writes N keys into a folder, then counts and deletes the folder with
//! `count_matching` and `delete_matching`, and reports timings.
//!
//!     cargo run -p redcrab-core --release --example delete_bench -- redis://127.0.0.1:6400/15 [N]
//!
//! It deletes what it wrote (`bench:delete:*`) and nothing else, but it
//! writes and deletes, so point it at a database meant for it: the URL has
//! no default.

use std::time::Instant;

use anyhow::Context as _;
use futures::StreamExt as _;
use redcrab_core::{Client, ConnectionConfig, DeleteProgress, ScanProgress, browser, format};

const FOLDER: &[u8] = b"bench:delete";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .context("usage: delete_bench redis://host:port/db [keys]")?;
    let n: usize = args
        .next()
        .map(|n| n.parse())
        .transpose()?
        .unwrap_or(500_000);
    let config = ConnectionConfig::parse_url(&url)?;
    let mut con = redis::Client::open(url.as_str())?.get_connection()?;

    let started = Instant::now();
    let keys: Vec<Vec<u8>> = (0..n)
        .map(|i| [FOLDER, format!(":{i:08}").as_bytes()].concat())
        .collect();
    for chunk in keys.chunks(10_000) {
        let mut mset = redis::cmd("MSET");
        for key in chunk {
            mset.arg(key).arg("v");
        }
        mset.query::<()>(&mut con)?;
    }
    println!(
        "seeded {} keys in {:?}",
        format::count(n as u64),
        started.elapsed()
    );

    futures::executor::block_on(async {
        let client = Client::connect(config).await?;
        let pattern = browser::folder_pattern(FOLDER, b':');

        let started = Instant::now();
        let mut counted = None;
        let mut events = client.count_matching(pattern.clone());
        while let Some(event) = events.next().await {
            match event {
                ScanProgress::Counting { .. } => {}
                ScanProgress::Done { keys, .. } => counted = Some(keys),
                ScanProgress::Failed { error, .. } => anyhow::bail!("count failed: {error}"),
            }
        }
        let elapsed = started.elapsed();
        println!(
            "counted {} keys in {elapsed:?} ({:.0} keys/s)",
            format::count(counted.unwrap_or(0)),
            n as f64 / elapsed.as_secs_f64()
        );

        let started = Instant::now();
        let mut events = client.delete_matching(pattern)?;
        let mut progress = 0;
        while let Some(event) = events.next().await {
            match event {
                DeleteProgress::Deleting(_) => progress += 1,
                DeleteProgress::Done { stats, elapsed } => println!(
                    "deleted {} keys in {elapsed:?} ({:.0} keys/s), {} progress events, {:?} end to end",
                    format::count(stats.deleted),
                    stats.deleted as f64 / elapsed.as_secs_f64(),
                    progress,
                    started.elapsed()
                ),
                DeleteProgress::Failed { error, .. } => anyhow::bail!("delete failed: {error}"),
                // Nothing here asks it to stop.
                DeleteProgress::Stopped { .. } => anyhow::bail!("the delete stopped"),
            }
        }
        Ok(())
    })
}
