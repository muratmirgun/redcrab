use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use redis::{Value, cmd, pipe};

use super::{Client, ConsoleSession, ServerInfo};
use crate::{
    runtime::spawn,
    value::{as_i64, into_string},
};

impl Client {
    pub fn ping(&self) -> impl Future<Output = Result<Duration>> + Send + 'static {
        let mut con = self.conn.clone();
        spawn(async move {
            let started = Instant::now();
            cmd("PING").query_async::<()>(&mut con).await?;
            Ok(started.elapsed())
        })
    }

    /// Version, mode, memory, clients and uptime from INFO, and the size of
    /// the selected database. A user without permission for INFO (ACL users
    /// often lack `@dangerous`) or DBSIZE gets those facts as `None` instead
    /// of an error, since the connection itself works.
    pub fn server_info(&self) -> impl Future<Output = Result<ServerInfo>> + Send + 'static {
        let mut con = self.conn.clone();
        spawn(async move {
            let replies: Vec<Value> = pipe()
                .cmd("INFO")
                .cmd("DBSIZE")
                .ignore_errors()
                .query_async(&mut con)
                .await?;
            let mut replies = replies.into_iter();
            let info = match permitted(replies.next())? {
                Value::Nil => None,
                info => Some(into_string(info)),
            };
            let db_size = match permitted(replies.next())? {
                Value::Nil => None,
                size => u64::try_from(as_i64(&size)).ok(),
            };
            let field = |name: &str| {
                info.as_deref()?
                    .lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let number = |name: &str| field(name)?.parse().ok();
            Ok(ServerInfo {
                version: field("redis_version"),
                mode: field("redis_mode"),
                used_memory_human: field("used_memory_human"),
                connected_clients: number("connected_clients"),
                uptime_secs: number("uptime_in_seconds"),
                db_size,
            })
        })
    }

    /// A console session with its own connection, opened on its first
    /// command, so that SELECT, MULTI or WATCH typed into a console never
    /// affect the app's requests. Keep one per console view.
    pub fn console(&self) -> ConsoleSession {
        ConsoleSession::new(self.redis.clone(), self.config.clone())
    }
}

/// A reply of a pipeline sent with `ignore_errors`: the value, nil if the
/// user lacks the permission for the command, or the error.
fn permitted(reply: Option<Value>) -> Result<Value> {
    match reply {
        Some(Value::ServerError(err)) if err.code() == "NOPERM" => Ok(Value::Nil),
        Some(Value::ServerError(err)) => Err(redis::RedisError::from(err).into()),
        Some(value) => Ok(value),
        None => Err(anyhow!("the server sent too few replies")),
    }
}
