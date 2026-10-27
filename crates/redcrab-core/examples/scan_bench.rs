//! Walks the whole keyspace into a `KeyTree` and reports timings.
//!
//!     cargo run -p redcrab-core --release --example scan_bench

use std::time::Instant;

use futures::StreamExt as _;
use redcrab_core::{Client, ConnectionConfig, Cursor, KeyTree, ScanOptions, format};

fn main() -> anyhow::Result<()> {
    futures::executor::block_on(async {
        let client = Client::connect(ConnectionConfig::default()).await?;
        let info = client.server_info().await?;
        println!(
            "redis {} ({}), {} keys, {}",
            info.version, info.mode, info.db_size, info.used_memory_human
        );

        let started = Instant::now();
        let mut tree = KeyTree::new(Some(b':'));
        let mut scan = client.scan(ScanOptions::default());
        while let Some(batch) = scan.next().await {
            for key in batch? {
                tree.insert(&key);
            }
        }
        let scanned = started.elapsed();
        let t = Instant::now();
        let rows = tree.rows();
        println!(
            "scan+tree: {} keys in {scanned:?}; top-level rows: {} in {:?}",
            tree.len(),
            rows.len(),
            t.elapsed()
        );

        // Expand the widest folder and time a full flatten.
        let session = tree.find(b"session").unwrap();
        tree.set_expanded(session, true);
        let t = Instant::now();
        let rows = tree.rows();
        println!(
            "expanded session/ -> {} rows in {:?}",
            rows.len(),
            t.elapsed()
        );
        let t = Instant::now();
        let rows = tree.rows();
        println!(
            "re-flatten (already sorted): {} rows in {:?}",
            rows.len(),
            t.elapsed()
        );

        let keys: Vec<_> = rows.iter().take(60).map(|r| tree.key(r.node)).collect();
        let t = Instant::now();
        let meta = client.key_meta(keys.clone()).await?;
        println!("meta for {} visible keys in {:?}", meta.len(), t.elapsed());

        for (key, ty) in [
            ("queue:jobs", "list"),
            ("leaderboard:global", "zset"),
            ("config:features", "hash"),
            ("user:7:tags", "set"),
            ("events:orders", "stream"),
            ("blob:binary", "string"),
            ("blob:large", "string"),
        ] {
            let t = Instant::now();
            let page = client
                .load(
                    key.as_bytes().into(),
                    redcrab_core::KeyType::parse(ty),
                    Cursor::Start,
                    200,
                )
                .await?;
            let preview = match &page.data {
                redcrab_core::ValueData::String(b) => {
                    format::bytes_inline(&b[..b.len().min(24)]).into_owned()
                }
                other => format!("{} items", other.len()),
            };
            println!(
                "load {key:<20} total={:<10} next={:?} {preview} ({:?})",
                page.total,
                page.next.is_some(),
                t.elapsed()
            );
        }
        println!("ping {:?}", client.ping().await?);
        Ok(())
    })
}
