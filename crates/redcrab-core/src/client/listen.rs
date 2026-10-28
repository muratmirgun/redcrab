//! Connections that only listen: Pub/Sub subscriptions and MONITOR, each on
//! a connection of its own, and PUBLISH and PUBSUB CHANNELS on the shared
//! one. What they hear goes into a [`Feed`], which the front end reads.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use redis::{Value, aio::PubSubSink, cmd};
use tokio::task::AbortHandle;

use super::{Client, ServerNode};
use crate::{
    feed::{Feed, Listening, Message, Monitored, is_pattern},
    progress::{self, Updates},
    runtime::{runtime, spawn},
    value::into_string,
};

/// How long opening a listening connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A Pub/Sub connection of its own: what it subscribes to and the messages
/// it got. Dropping it unsubscribes from everything and closes it.
pub struct Subscriber {
    sink: Arc<tokio::sync::Mutex<PubSubSink>>,
    feed: Arc<Mutex<Feed<Message>>>,
    updates: Option<Updates<Listening>>,
    reader: AbortHandle,
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Subscriber {
    /// The messages, newest last.
    pub fn feed(&self) -> Arc<Mutex<Feed<Message>>> {
        self.feed.clone()
    }

    /// How the connection goes, as the messages come. Taken once.
    pub fn take_updates(&mut self) -> Option<Updates<Listening>> {
        self.updates.take()
    }

    /// Subscribes to a channel, or with glob characters to a pattern
    /// (PSUBSCRIBE).
    pub fn subscribe(
        &self,
        channel: Vec<u8>,
    ) -> impl Future<Output = Result<()>> + Send + 'static + use<> {
        let sink = self.sink.clone();
        spawn(async move {
            let mut sink = sink.lock().await;
            if is_pattern(&channel) {
                sink.psubscribe(&channel).await?;
            } else {
                sink.subscribe(&channel).await?;
            }
            Ok(())
        })
    }

    pub fn unsubscribe(
        &self,
        channel: Vec<u8>,
    ) -> impl Future<Output = Result<()>> + Send + 'static + use<> {
        let sink = self.sink.clone();
        spawn(async move {
            let mut sink = sink.lock().await;
            if is_pattern(&channel) {
                sink.punsubscribe(&channel).await?;
            } else {
                sink.unsubscribe(&channel).await?;
            }
            Ok(())
        })
    }
}

/// MONITOR on a connection of its own: every command the server runs.
/// Dropping it stops MONITOR and closes the connection.
pub struct Monitor {
    feed: Arc<Mutex<Feed<Monitored>>>,
    updates: Option<Updates<Listening>>,
    reader: AbortHandle,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Monitor {
    pub fn feed(&self) -> Arc<Mutex<Feed<Monitored>>> {
        self.feed.clone()
    }

    pub fn take_updates(&mut self) -> Option<Updates<Listening>> {
        self.updates.take()
    }
}

impl Client {
    /// Opens a Pub/Sub connection, subscribed to nothing yet. On a cluster
    /// it goes to one primary: classic Pub/Sub reaches every node.
    pub fn subscriber(&self) -> impl Future<Output = Result<Subscriber>> + Send + 'static + use<> {
        let connector = self.connector.clone();
        let config = self.config.clone();
        spawn(async move {
            let client = connector.listener(&config, None).await?;
            let pubsub = tokio::time::timeout(CONNECT_TIMEOUT, client.get_async_pubsub())
                .await
                .context("the server did not answer")?
                .context("could not open a Pub/Sub connection")?;
            let (sink, mut stream) = pubsub.split();
            let feed = Arc::new(Mutex::new(Feed::default()));
            let (reporter, updates) = progress::channel();
            let shared = feed.clone();
            let reader = runtime().spawn(async move {
                while let Some(message) = stream.next().await {
                    let received = {
                        let mut feed = shared.lock().unwrap_or_else(|e| e.into_inner());
                        feed.push(Message {
                            at: SystemTime::now(),
                            channel: message.get_channel_name().as_bytes().to_vec(),
                            pattern: message.get_pattern::<Option<Vec<u8>>>().ok().flatten(),
                            payload: message.get_payload_bytes().to_vec(),
                        });
                        feed.received()
                    };
                    reporter.update(Listening::Receiving(received));
                }
                reporter.update(Listening::Ended("the connection closed".into()));
            });
            Ok(Subscriber {
                sink: Arc::new(tokio::sync::Mutex::new(sink)),
                feed,
                updates: Some(updates),
                reader: reader.abort_handle(),
            })
        })
    }

    /// Publishes `message` on `channel`; how many clients got it. Refused on
    /// a read-only connection: subscribers act on what they receive.
    pub fn publish(
        &self,
        channel: Vec<u8>,
        message: Vec<u8>,
    ) -> impl Future<Output = Result<u64>> + Send + 'static + use<> {
        let mut con = self.conn.clone();
        let writable = self.ensure_writable();
        spawn(async move {
            writable?;
            let receivers: i64 = cmd("PUBLISH")
                .arg(channel)
                .arg(message)
                .query_async(&mut con)
                .await
                .context("PUBLISH failed")?;
            Ok(receivers.max(0) as u64)
        })
    }

    /// The channels with subscribers (PUBSUB CHANNELS) and how many each
    /// has (PUBSUB NUMSUB), most subscribers first.
    pub fn channels(
        &self,
    ) -> impl Future<Output = Result<Vec<(Vec<u8>, u64)>>> + Send + 'static + use<> {
        let mut con = self.conn.clone();
        spawn(async move {
            let channels: Vec<Vec<u8>> = cmd("PUBSUB")
                .arg("CHANNELS")
                .query_async(&mut con)
                .await
                .context("PUBSUB CHANNELS failed")?;
            if channels.is_empty() {
                return Ok(Vec::new());
            }
            let counts: Value = cmd("PUBSUB")
                .arg("NUMSUB")
                .arg(&channels)
                .query_async(&mut con)
                .await
                .context("PUBSUB NUMSUB failed")?;
            let counts: Vec<Value> = match counts {
                Value::Array(items) => items,
                Value::Map(pairs) => pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
                _ => Vec::new(),
            };
            let mut listed: Vec<(Vec<u8>, u64)> = counts
                .chunks(2)
                .filter_map(|pair| match pair {
                    [channel, Value::Int(n)] => Some((
                        into_string(channel.clone()).into_bytes(),
                        (*n).max(0) as u64,
                    )),
                    _ => None,
                })
                .collect();
            listed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(listed)
        })
    }

    /// Starts MONITOR on `node` (a cluster's primary; the server itself
    /// otherwise). It slows a busy server down: stop it when done.
    pub fn monitor(
        &self,
        node: Option<&ServerNode>,
    ) -> impl Future<Output = Result<Monitor>> + Send + 'static + use<> {
        let connector = self.connector.clone();
        let config = self.config.clone();
        let node = node.and_then(ServerNode::host_port);
        spawn(async move {
            let client = connector.listener(&config, node).await?;
            let monitor = tokio::time::timeout(CONNECT_TIMEOUT, client.get_async_monitor())
                .await
                .context("the server did not answer")?
                .context("could not start MONITOR")?;
            let mut stream = monitor.into_on_message::<String>();
            let feed = Arc::new(Mutex::new(Feed::default()));
            let (reporter, updates) = progress::channel();
            let shared = feed.clone();
            let reader = runtime().spawn(async move {
                while let Some(line) = stream.next().await {
                    let Some(entry) = Monitored::parse(&line) else {
                        continue;
                    };
                    let received = {
                        let mut feed = shared.lock().unwrap_or_else(|e| e.into_inner());
                        feed.push(entry);
                        feed.received()
                    };
                    reporter.update(Listening::Receiving(received));
                }
                reporter.update(Listening::Ended("the connection closed".into()));
            });
            Ok(Monitor {
                feed,
                updates: Some(updates),
                reader: reader.abort_handle(),
            })
        })
    }
}
