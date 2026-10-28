//! The server tools against real servers: INFO, the slow log and the client
//! list of a standalone server and of each cluster primary, and the
//! read-only gate on the two requests that change the server.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), whose slow-log threshold a test lowers for a moment
//! (the other tests do not read the slow log), and the cluster.

#[macro_use]
mod common;

use common::*;
use redcrab_core::{ConnectionConfig, ReadOnlyError, server::Overview};
use redis::Commands as _;

#[test]
fn a_single_server_is_one_node_with_its_info() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let nodes = block_on(client.server_nodes()).unwrap();
    assert_eq!(nodes.len(), 1);
    let info = block_on(nodes[0].info()).unwrap();
    assert!(info.get("redis_version").is_some());
    let overview = Overview::from_info(&info, None);
    assert_eq!(overview.role.as_deref(), Some("master"));
    assert!(overview.clients.unwrap() >= 1);
    assert!(overview.used_memory.unwrap() > 0);
}

#[test]
fn slow_commands_are_logged_and_the_log_resets() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let node = block_on(client.server_nodes()).unwrap().remove(0);
    let mut admin = common::node(Server::Standalone.port());
    let threshold: Vec<String> = redis::cmd("CONFIG")
        .arg("GET")
        .arg("slowlog-log-slower-than")
        .query(&mut admin)
        .unwrap();
    // Log everything for as long as it takes to run one command.
    let marker = t.key("slow");
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("slowlog-log-slower-than")
        .arg(0)
        .query(&mut admin)
        .unwrap();
    let _: String = redis::cmd("ECHO").arg(&marker).query(&mut admin).unwrap();
    let _: () = redis::cmd("CONFIG")
        .arg("SET")
        .arg("slowlog-log-slower-than")
        .arg(&threshold[1])
        .query(&mut admin)
        .unwrap();

    let log = block_on(node.slow_log(1024)).unwrap();
    let entry = log
        .entries
        .iter()
        .find(|entry| entry.args.last().map(Vec::as_slice) == Some(marker.as_bytes()))
        .expect("the ECHO was logged");
    assert_eq!(entry.command(), format!("ECHO {marker}"));
    assert!(entry.client.is_some());
    assert!(log.len.unwrap() >= 1);
    assert_eq!(log.slower_than_micros, Some(threshold[1].parse().unwrap()));
    assert!(log.max_len.unwrap() > 0);
    assert!(log.settings().is_some());

    block_on(node.reset_slow_log()).unwrap();
    let log = block_on(node.slow_log(1024)).unwrap();
    assert!(
        !log.entries
            .iter()
            .any(|entry| entry.args.last().map(Vec::as_slice) == Some(marker.as_bytes()))
    );
}

#[test]
fn clients_are_listed_and_one_can_be_killed() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let node = block_on(client.server_nodes()).unwrap().remove(0);
    let name = t.key("victim").replace(':', "-");
    let mut victim = common::node(Server::Standalone.port());
    let _: () = redis::cmd("CLIENT")
        .arg("SETNAME")
        .arg(&name)
        .query(&mut victim)
        .unwrap();

    let list = block_on(node.clients()).unwrap();
    let own = list.own_id.expect("CLIENT ID answered");
    assert!(list.clients.iter().any(|c| c.id == own));
    let target = list
        .clients
        .iter()
        .find(|c| c.name == name)
        .expect("the named client is listed")
        .clone();
    assert!(target.matches(&name.to_lowercase()));

    assert!(block_on(node.kill_client(target.id)).unwrap());
    assert!(victim.ping::<String>().is_err());
    // Gone already.
    assert!(!block_on(node.kill_client(target.id)).unwrap());
    let list = block_on(node.clients()).unwrap();
    assert!(!list.clients.iter().any(|c| c.id == target.id));
}

#[test]
fn a_read_only_connection_changes_nothing() {
    let Some(t) = it!() else { return };
    let client = t.connect(ConnectionConfig {
        read_only: true,
        ..standalone()
    });
    let node = block_on(client.server_nodes()).unwrap().remove(0);
    let name = t.key("spared").replace(':', "-");
    let mut spared = common::node(Server::Standalone.port());
    let _: () = redis::cmd("CLIENT")
        .arg("SETNAME")
        .arg(&name)
        .query(&mut spared)
        .unwrap();
    let list = block_on(node.clients()).unwrap();
    let id = list.clients.iter().find(|c| c.name == name).unwrap().id;

    let err = block_on(node.kill_client(id)).unwrap_err();
    assert!(err.downcast_ref::<ReadOnlyError>().is_some());
    assert_eq!(spared.ping::<String>().unwrap(), "PONG");
    let err = block_on(node.reset_slow_log()).unwrap_err();
    assert!(err.downcast_ref::<ReadOnlyError>().is_some());
    // Reading is fine.
    block_on(node.slow_log(10)).unwrap();
    block_on(node.info()).unwrap();
}

#[test]
fn a_cluster_has_a_node_per_primary() {
    let Some(t) = it!() else { return };
    let client = t.connect(cluster());
    let nodes = block_on(client.server_nodes()).unwrap();
    let mut ports: Vec<u16> = nodes
        .iter()
        .map(|node| node.label().rsplit(':').next().unwrap().parse().unwrap())
        .collect();
    ports.sort();
    let mut primaries = cluster_primaries();
    primaries.sort();
    assert_eq!(ports, primaries);
    for node in &nodes {
        let info = block_on(node.info()).unwrap();
        assert_eq!(info.get("role"), Some("master"));
        assert_eq!(info.get("redis_mode"), Some("cluster"));
        let clients = block_on(node.clients()).unwrap();
        // Each primary's list shows redcrab's own connection to it.
        let own = clients.own_id.unwrap();
        assert!(clients.clients.iter().any(|c| c.id == own));
        block_on(node.slow_log(10)).unwrap();
    }
}
