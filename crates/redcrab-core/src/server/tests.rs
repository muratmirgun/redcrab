use std::time::Duration;

use super::*;

const INFO: &str = "# Server\r
redis_version:8.0.2\r
redis_mode:standalone\r
uptime_in_seconds:273600\r
\r
# Clients\r
connected_clients:7\r
blocked_clients:1\r
\r
# Memory\r
used_memory:1258291\r
used_memory_human:1.20M\r
used_memory_rss:2516582\r
used_memory_peak:3000000\r
maxmemory:0\r
maxmemory_policy:noeviction\r
mem_fragmentation_ratio:2.00\r
\r
# Persistence\r
rdb_changes_since_last_save:42\r
rdb_last_save_time:1793180000\r
aof_enabled:0\r
\r
# Stats\r
instantaneous_ops_per_sec:1520\r
instantaneous_input_kbps:12.50\r
instantaneous_output_kbps:80.25\r
keyspace_hits:900\r
keyspace_misses:100\r
expired_keys:5\r
evicted_keys:0\r
\r
# Replication\r
role:master\r
connected_slaves:2\r
\r
# Keyspace\r
db0:keys=12,expires=3,avg_ttl=0,subexpiry=0\r
db5:keys=8,expires=1,avg_ttl=1000\r
";

#[test]
fn info_reads_sections_and_fields() {
    let info = Info::parse(INFO);
    let names: Vec<_> = info.sections().iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Server",
            "Clients",
            "Memory",
            "Persistence",
            "Stats",
            "Replication",
            "Keyspace"
        ]
    );
    assert_eq!(info.get("redis_version"), Some("8.0.2"));
    assert_eq!(info.number("mem_fragmentation_ratio"), Some(2.0));
    assert_eq!(info.get("missing"), None);
    assert_eq!(
        info.keyspace(),
        [
            KeyspaceDb {
                db: 0,
                keys: 12,
                expires: 3
            },
            KeyspaceDb {
                db: 5,
                keys: 8,
                expires: 1
            }
        ]
    );
    // Values may hold colons; lines before any heading still count.
    let info = Info::parse("executable:/usr/bin/redis-server\nconfig_file:C:\\redis.conf\n");
    assert_eq!(info.get("config_file"), Some("C:\\redis.conf"));
    assert!(Info::parse("").sections().is_empty());
}

#[test]
fn the_overview_figures_come_from_info() {
    let overview = Overview::from_info(&Info::parse(INFO), None);
    assert_eq!(overview.version.as_deref(), Some("8.0.2"));
    assert_eq!(overview.role.as_deref(), Some("master"));
    assert_eq!(overview.uptime, Some(Duration::from_secs(273_600)));
    assert_eq!(
        (overview.clients, overview.blocked_clients),
        (Some(7), Some(1))
    );
    assert_eq!(overview.used_memory, Some(1_258_291));
    // No limit is no maximum.
    assert_eq!(overview.max_memory, None);
    assert_eq!(overview.ops_per_sec, Some(1520.0));
    assert_eq!(overview.hit_rate_total, Some(0.9));
    // Without an INFO before, nothing tells the recent rate.
    assert_eq!(overview.hit_rate, None);
    assert_eq!((overview.keys, overview.keys_with_ttl), (20, 4));
    assert_eq!(overview.aof, Some(false));
    assert_eq!(overview.replicas, Some(2));
}

#[test]
fn the_recent_hit_rate_counts_since_the_info_before() {
    let before = Info::parse("keyspace_hits:900\nkeyspace_misses:100\n");
    let now = Info::parse("keyspace_hits:930\nkeyspace_misses:170\n");
    let overview = Overview::from_info(&now, Some(&before));
    assert_eq!(overview.hit_rate, Some(0.3));
    // No lookups in between, and a restart (counters went back).
    assert_eq!(Overview::from_info(&before, Some(&before)).hit_rate, None);
    assert_eq!(Overview::from_info(&before, Some(&now)).hit_rate, None);
}

#[test]
fn the_monitor_keeps_two_minutes_of_trends() {
    let mut monitor = Monitor::default();
    for i in 0..(TREND_LEN + 5) {
        monitor.record(Info::parse(&format!(
            "instantaneous_ops_per_sec:{i}\nused_memory:{}\n",
            i * 10
        )));
    }
    assert_eq!(monitor.ops.values().len(), TREND_LEN);
    assert_eq!(monitor.ops.values().next(), Some(5.0));
    assert_eq!(monitor.ops.max(), (TREND_LEN + 4) as f64);
    // A figure the server does not give leaves no gap of zeros.
    assert_eq!(monitor.clients.values().len(), 0);
    monitor.reset();
    assert_eq!(monitor.ops.values().len(), 0);
    assert!(monitor.info().is_none());
}

#[test]
fn durations_read_short() {
    assert_eq!(duration(Duration::from_secs(42)), "42s");
    assert_eq!(duration(Duration::from_secs(312)), "5m 12s");
    assert_eq!(duration(Duration::from_secs(5 * 3600 + 12 * 60)), "5h 12m");
    assert_eq!(duration(Duration::from_secs(273_600)), "3d 4h");
    assert_eq!(micros(Duration::from_micros(850)), "850 µs");
    assert_eq!(micros(Duration::from_micros(12_400)), "12.4 ms");
    assert_eq!(micros(Duration::from_micros(2_310_000)), "2.31 s");
}

#[test]
fn slow_log_settings_read_as_a_sentence() {
    let mut log = SlowLog {
        slower_than_micros: Some(10_000),
        max_len: Some(128),
        ..SlowLog::default()
    };
    assert_eq!(
        log.settings().as_deref(),
        Some("Commands slower than 10.0 ms are logged; the last 128 are kept.")
    );
    log.slower_than_micros = Some(-1);
    assert_eq!(
        log.settings().as_deref(),
        Some("The slow log is off; the last 128 are kept.")
    );
    log.slower_than_micros = None;
    assert_eq!(log.settings(), None);
    let entry = SlowEntry {
        id: 1,
        at: 0,
        duration: Duration::from_millis(20),
        args: vec![b"HSET".to_vec(), b"user:1".to_vec(), b"a b".to_vec()],
        client: None,
        client_name: None,
    };
    assert_eq!(entry.command(), r#"HSET user:1 "a b""#);
}

#[test]
fn client_list_lines_are_read() {
    let text = "id=3 addr=127.0.0.1:52555 laddr=127.0.0.1:6379 fd=8 name=worker age=4 idle=2 flags=N db=0 sub=0 psub=0 multi=-1 qbuf=26 tot-mem=22426 events=r cmd=client|list user=default resp=2\n\
id=7 addr=10.0.0.2:40000 name= age=100 idle=100 flags=S db=0 cmd=replconf\n\
garbage line\n";
    let list = ClientList::parse(text, Some(3));
    assert_eq!(list.clients.len(), 2);
    let first = &list.clients[0];
    assert_eq!(first.id, 3);
    assert_eq!(first.name, "worker");
    assert_eq!(first.idle, Duration::from_secs(2));
    assert_eq!(first.cmd, "client|list");
    assert_eq!(first.user.as_deref(), Some("default"));
    assert_eq!(first.memory, Some(22_426));
    assert_eq!(list.own_id, Some(3));
    // An older server without user or tot-mem.
    let replica = &list.clients[1];
    assert_eq!((replica.user.as_deref(), replica.memory), (None, None));
    assert_eq!(replica.kind(), ["replica"]);
    assert!(replica.matches("10.0.0"));
    assert!(first.matches("work"));
    assert!(!first.matches("replconf"));
    assert!(first.matches(""));
}
