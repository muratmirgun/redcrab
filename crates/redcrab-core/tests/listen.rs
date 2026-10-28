//! Connections that only listen, against real servers: Pub/Sub messages
//! from a channel and a pattern, the channels with subscribers, PUBLISH and
//! its read-only gate, and MONITOR, on a single server and a cluster.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400) and the cluster, with channels named after the test.

#[macro_use]
mod common;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use common::*;
use redcrab_core::{
    ConnectionConfig, ReadOnlyError,
    feed::{Feed, Monitored},
};

/// Waits up to five seconds for `found` among the feed's entries.
fn wait_for<T>(feed: &Arc<Mutex<Feed<T>>>, found: impl Fn(&T) -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if feed.lock().unwrap().entries().iter().any(&found) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn messages_arrive_from_channels_and_patterns() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let subscriber = block_on(client.subscriber()).unwrap();
    let channel = t.key("news");
    let pattern = t.key("alerts.*");
    block_on(subscriber.subscribe(channel.clone().into_bytes())).unwrap();
    block_on(subscriber.subscribe(pattern.clone().into_bytes())).unwrap();

    assert_eq!(
        block_on(client.publish(channel.clone().into_bytes(), b"hello".to_vec())).unwrap(),
        1
    );
    let alert = t.key("alerts.disk");
    block_on(client.publish(alert.clone().into_bytes(), b"\xff full".to_vec())).unwrap();
    let feed = subscriber.feed();
    assert!(wait_for(&feed, |m| m.channel == channel.as_bytes()
        && m.payload == b"hello"
        && m.pattern.is_none()));
    assert!(wait_for(&feed, |m| m.channel == alert.as_bytes()
        && m.payload == b"\xff full"
        && m.pattern.as_deref() == Some(pattern.as_bytes())));

    // The channel is listed with its subscriber; patterns are not.
    let channels = block_on(client.channels()).unwrap();
    assert!(channels.contains(&(channel.clone().into_bytes(), 1)));

    block_on(subscriber.unsubscribe(channel.clone().into_bytes())).unwrap();
    assert_eq!(
        block_on(client.publish(channel.into_bytes(), b"again".to_vec())).unwrap(),
        0
    );
    // Dropping it closes the connection: nobody hears the pattern now.
    drop(subscriber);
    let until = Instant::now() + Duration::from_secs(5);
    while block_on(client.publish(alert.clone().into_bytes(), b"x".to_vec())).unwrap() > 0 {
        assert!(Instant::now() < until, "the subscriber is still connected");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_read_only_connection_does_not_publish() {
    let Some(t) = it!() else { return };
    let client = t.connect(ConnectionConfig {
        read_only: true,
        ..standalone()
    });
    let err = block_on(client.publish(t.key("c").into_bytes(), b"m".to_vec())).unwrap_err();
    assert!(err.downcast_ref::<ReadOnlyError>().is_some());
    // Listening is fine.
    let subscriber = block_on(client.subscriber()).unwrap();
    block_on(subscriber.subscribe(t.key("c").into_bytes())).unwrap();
}

#[test]
fn monitor_reports_the_commands_the_server_runs() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let monitor = block_on(client.monitor(None)).unwrap();
    let marker = t.key("monitored");
    let mut con = common::node(Server::Standalone.port());
    let _: String = redis::cmd("ECHO").arg(&marker).query(&mut con).unwrap();
    let feed = monitor.feed();
    assert!(wait_for(&feed, |entry: &Monitored| {
        entry.args.len() == 2 && entry.args[1] == marker.as_bytes() && entry.db == 0
    }));
}

#[test]
fn a_cluster_publishes_and_monitors_per_primary() {
    let Some(t) = it!() else { return };
    let client = t.connect(cluster());
    let subscriber = block_on(client.subscriber()).unwrap();
    let channel = t.key("cluster-news");
    block_on(subscriber.subscribe(channel.clone().into_bytes())).unwrap();
    block_on(client.publish(channel.clone().into_bytes(), b"hi".to_vec())).unwrap();
    assert!(wait_for(&subscriber.feed(), |m| m.channel == channel.as_bytes()));

    let nodes = block_on(client.server_nodes()).unwrap();
    let node = nodes.last().unwrap();
    let monitor = block_on(client.monitor(Some(node))).unwrap();
    let port: u16 = node.label().rsplit(':').next().unwrap().parse().unwrap();
    let marker = t.key("on-this-node");
    let mut con = common::node(port);
    let _: String = redis::cmd("ECHO").arg(&marker).query(&mut con).unwrap();
    assert!(wait_for(&monitor.feed(), |entry: &Monitored| {
        entry.args.get(1).map(Vec::as_slice) == Some(marker.as_bytes())
    }));
}

#[test]
fn a_sentinel_master_is_subscribed_to() {
    let Some(t) = it!() else { return };
    let client = t.connect(sentinel());
    let subscriber = block_on(client.subscriber()).unwrap();
    let channel = t.key("sentinel-news");
    block_on(subscriber.subscribe(channel.clone().into_bytes())).unwrap();
    assert_eq!(
        block_on(client.publish(channel.clone().into_bytes(), b"hi".to_vec())).unwrap(),
        1
    );
    assert!(wait_for(&subscriber.feed(), |m| m.channel == channel.as_bytes()));
}
