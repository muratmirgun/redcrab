//! Walks the whole keyspace into a `KeyTree` and reports timings and memory.
//!
//!     cargo run -p redcrab-core --release --example scan_bench -- [redis://127.0.0.1:6379] [--gui]
//!     cargo run -p redcrab-core --release --example scan_bench -- --synthetic 1000000 [SHAPE] [--reflatten K]
//!
//! `--synthetic N` needs no server: it builds the tree from N generated keys,
//! inserted in a shuffled order as SCAN returns them, so memory per key can
//! be measured at any size. SHAPE is one of
//!
//! - `seed` (default): the shapes `examples/seed.rs` writes;
//! - `shared`: zero-padded ids at the top level (`item_000000123456`), whose
//!   names share their first eight bytes;
//! - `root`: hex ids at the top level, no folders at all.
//!
//! `--reflatten K` flattens K times while the keys go in, and `--gui` (live)
//! redraws the way the app does while it scans (a flatten and the visible
//! rows after every event, at most ~15 times a second). Both measure the
//! path a window takes, whose memory and pauses a scan without redraws does
//! not show. Run each mode in a fresh process, since the RSS figures are
//! deltas of the whole process.

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use futures::StreamExt as _;
use redcrab_core::{
    Client, ConnectionConfig, Cursor, KeyTree, KeyType, RowKind, ScanEvent, ScanOptions, ValueData,
    browser::{DEFAULT_KEY_LIMIT, KeyBrowser},
    format,
};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
    };
    if let Some(n) = value("--synthetic") {
        let n = n.parse().context("--synthetic takes a key count")?;
        let shape = match args
            .iter()
            .find(|a| ["seed", "shared", "root"].contains(&a.as_str()))
        {
            Some(shape) => shape.as_str(),
            None => "seed",
        };
        let reflatten = value("--reflatten")
            .map(|k| k.parse())
            .transpose()
            .context("--reflatten takes a count")?
            .unwrap_or(0);
        return synthetic(n, shape, reflatten);
    }
    let url = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map_or("redis://127.0.0.1:6379", String::as_str);
    futures::executor::block_on(live(url, flag("--gui")))
}

/// Resident set size of this process, as `ps` reports it.
fn rss() -> Option<usize> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kib: usize = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kib * 1024)
}

fn mib(bytes: usize) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Heap and RSS per key of a freshly built tree.
fn report_memory(tree: &KeyTree, rss_before: Option<usize>) {
    let keys = tree.len().max(1);
    let heap = tree.heap_bytes();
    print!(
        "memory: tree heap {} = {:.1} B/key",
        mib(heap),
        heap as f64 / keys as f64
    );
    if let (Some(before), Some(after)) = (rss_before, rss()) {
        let grown = after.saturating_sub(before);
        print!(
            "; RSS {} -> {} (+{} = {:.1} B/key)",
            mib(before),
            mib(after),
            mib(grown),
            grown as f64 / keys as f64
        );
    }
    println!();
}

/// Times flattening: the top level, then with the widest folder expanded
/// (if there is one), then once more with nothing to sort.
fn report_flatten(tree: &mut KeyTree) -> anyhow::Result<()> {
    let t = Instant::now();
    let mut rows = tree.rows();
    println!("top-level rows: {} in {:?}", rows.len(), t.elapsed());

    let widest = rows
        .iter()
        .filter(|row| row.kind == RowKind::Folder)
        .max_by_key(|row| tree.key_count(row.node))
        .map(|row| row.node);
    if let Some(widest) = widest {
        tree.set_expanded(widest, true);
        let name = String::from_utf8_lossy(tree.name(widest)).into_owned();
        let t = Instant::now();
        rows = tree.rows();
        println!(
            "expanded {name}/ -> {} rows in {:?}",
            rows.len(),
            t.elapsed()
        );
    }
    // Single runs are noisy at this scale: report the spread of several.
    let mut times: Vec<Duration> = (0..9)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(tree.rows());
            t.elapsed()
        })
        .collect();
    times.sort();
    println!(
        "re-flatten (already sorted): {} rows, min {:?} / median {:?} of {}",
        rows.len(),
        times[0],
        times[times.len() / 2],
        times.len()
    );
    Ok(())
}

/// Key `i` of a synthetic keyspace of the given shape.
fn synthetic_key(shape: &str, i: u64, out: &mut Vec<u8>) {
    use std::io::Write as _;
    out.clear();
    let _ = match shape {
        "shared" => write!(out, "item_{i:012}"),
        "root" => write!(out, "{:016x}", i.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
        // The shapes `examples/seed.rs` writes.
        _ => match i % 10 {
            0..=4 => write!(out, "session:{:016x}", i.wrapping_mul(2654435761)),
            5..=6 => write!(out, "cache:product:{i}"),
            7 => write!(out, "user:{i}:profile"),
            8 => write!(out, "user:{}:tags", i - 1),
            _ => write!(out, "counter:{}:{i}", i % 97),
        },
    };
}

/// `0..n` shuffled with a fixed seed. SCAN returns keys in hash order, which
/// has nothing to do with their names; an order with structure (ascending
/// runs, say) would flatter the sorts.
fn shuffled(n: u64) -> Vec<u64> {
    let mut order: Vec<u64> = (0..n).collect();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for i in (1..order.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        order.swap(i, (state % (i as u64 + 1)) as usize);
    }
    order
}

fn synthetic(n: u64, shape: &str, reflatten: u64) -> anyhow::Result<()> {
    let order = shuffled(n);
    let rss_before = rss();
    let mut tree = KeyTree::new(Some(b':'));
    let mut key = Vec::new();
    let mut rows = Vec::new();
    let mut flattens = Vec::new();
    let every = (n / (reflatten + 1)).max(1);
    // Batches the size of a SCAN reply: the scan holds the tree's lock for
    // one, so the slowest is the longest the UI can wait for it.
    let mut batches = Vec::new();
    let t = Instant::now();
    for chunk in order.chunks(2000) {
        let batch = Instant::now();
        for &i in chunk {
            synthetic_key(shape, i, &mut key);
            tree.insert(&key);
        }
        batches.push(batch.elapsed());
        if reflatten > 0
            && tree.len() as u64 / every > flattens.len() as u64
            && (flattens.len() as u64) < reflatten
        {
            let t = Instant::now();
            tree.rows_into(&mut rows);
            flattens.push(t.elapsed());
        }
    }
    let inserted = t.elapsed() - flattens.iter().sum::<Duration>();
    println!(
        "synthetic {shape}: inserted {} keys in {inserted:?}",
        format::count(tree.len() as u64),
    );
    report_spread("2,000-key batches (the scan's lock hold)", &mut batches);
    if !flattens.is_empty() {
        report_spread("re-flattens while inserting", &mut flattens);
    }
    report_memory(&tree, rss_before);
    report_flatten(&mut tree)
}

/// min / median / max of some timings.
fn report_spread(what: &str, times: &mut [Duration]) {
    times.sort();
    println!(
        "{what}: {} times, min {:?} / median {:?} / max {:?}",
        times.len(),
        times[0],
        times[times.len() / 2],
        times[times.len() - 1]
    );
}

/// Scans like the app's key list does while a window is open: after every
/// event a flatten and the first screen of rows, then a pause so that at
/// most ~15 redraws a second happen.
async fn live_gui(client: &Client) -> anyhow::Result<()> {
    let rss_before = rss();
    let mut browser = KeyBrowser::new(b':', Some(DEFAULT_KEY_LIMIT));
    let request = browser.begin_scan("");
    let tree = request.tree.clone();
    let started = Instant::now();
    let mut events = client.scan_into(request.tree, request.options);
    let mut redraws = Vec::new();
    while let Some(event) = events.next().await {
        let finished = !matches!(event, ScanEvent::Progress { .. });
        browser.on_scan_event(request.generation, event);
        let t = Instant::now();
        let len = browser.len();
        std::hint::black_box(browser.row_views(0..len.min(40)));
        redraws.push(t.elapsed());
        if finished {
            break;
        }
        std::thread::sleep(Duration::from_millis(66));
    }
    println!(
        "gui scan: {} with {} redraws in {:?}",
        browser.status_text(),
        redraws.len(),
        started.elapsed()
    );
    report_spread("redraws (flatten + 40 rows)", &mut redraws);
    let tree = tree.lock().unwrap_or_else(|e| e.into_inner());
    report_memory(&tree, rss_before);
    Ok(())
}

async fn live(url: &str, gui: bool) -> anyhow::Result<()> {
    let client = Client::connect(ConnectionConfig::parse_url(url)?).await?;
    let info = client.server_info().await?;
    let unknown = || "?".to_string();
    println!(
        "redis {} ({}), {} keys, {}",
        info.version.unwrap_or_else(unknown),
        info.mode.unwrap_or_else(unknown),
        info.db_size.map_or_else(unknown, |n| n.to_string()),
        info.used_memory_human.unwrap_or_else(unknown)
    );
    if gui {
        return live_gui(&client).await;
    }

    // The app's path: SCAN straight into a shared tree on the I/O threads.
    let rss_before = rss();
    let tree = Arc::new(Mutex::new(KeyTree::new(Some(b':'))));
    let started = Instant::now();
    let mut events = client.scan_into(tree.clone(), ScanOptions::default());
    let mut scanned = Duration::ZERO;
    while let Some(event) = events.next().await {
        match event {
            ScanEvent::Progress { .. } => {}
            ScanEvent::Done { elapsed, .. } => scanned = elapsed,
            ScanEvent::Failed(err) => anyhow::bail!(err),
        }
    }
    let mut tree = Arc::try_unwrap(tree)
        .ok()
        .context("the scan still holds the tree")?
        .into_inner()?;
    println!(
        "scan_into: {} keys in {scanned:?} ({:?} wall)",
        format::count(tree.len() as u64),
        started.elapsed()
    );
    report_memory(&tree, rss_before);
    report_flatten(&mut tree)?;

    let rows = tree.rows();
    let keys: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Key)
        .take(60)
        .map(|r| tree.key(r.node))
        .collect();
    let t = Instant::now();
    let meta = client.key_meta(keys).await?;
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
                KeyType::parse(ty),
                Cursor::Start,
                200,
            )
            .await?;
        let preview = match &page.data {
            ValueData::String(b) => format::bytes_inline(&b[..b.len().min(24)]).into_owned(),
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
}
