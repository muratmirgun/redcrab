//! Fills a Redis database with realistic test data of every type.
//!
//!     cargo run -p redcrab-core --release --example seed -- [redis://127.0.0.1/0] [keys]

use redis::pipe;

#[tokio::main]
async fn main() -> redis::RedisResult<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "redis://127.0.0.1/0".into());
    let n: u64 = args
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1_000_000);
    let mut con = redis::Client::open(url)?
        .get_multiplexed_async_connection()
        .await?;

    let started = std::time::Instant::now();
    for chunk in (0..n).collect::<Vec<_>>().chunks(10_000) {
        let mut p = pipe();
        for &i in chunk {
            match i % 10 {
                0..=4 => p.set_ex(
                    format!("session:{:016x}", i * 2654435761),
                    "token",
                    3600 + i % 7200,
                ),
                5..=6 => p.set(
                    format!("cache:product:{i}"),
                    format!(
                        r#"{{"id":{i},"name":"Product {i}","price":{}.99,"tags":["a","b"]}}"#,
                        i % 500
                    ),
                ),
                7 => p.hset_multiple(
                    format!("user:{i}:profile"),
                    &[
                        ("name", format!("User {i}")),
                        ("email", format!("user{i}@example.com")),
                        ("plan", "pro".into()),
                    ],
                ),
                8 => p.sadd(format!("user:{}:tags", i - 1), &["redis", "rust", "gpui"]),
                _ => p.set(format!("counter:{}:{}", i % 97, i), i),
            };
        }
        p.query_async::<()>(&mut con).await?;
    }

    // A few large and unusual values.
    let mut p = pipe();
    for i in 0..20_000 {
        p.rpush("queue:jobs", format!(r#"{{"job":{i},"kind":"email"}}"#));
        p.zadd(
            "leaderboard:global",
            format!("player:{i}"),
            (i * 7919 % 100_000) as f64,
        );
        p.hset("config:features", format!("flag_{i}"), i % 2 == 0);
    }
    for i in 0..5_000 {
        p.cmd("XADD")
            .arg("events:orders")
            .arg("*")
            .arg("order")
            .arg(i)
            .arg("amount")
            .arg(i * 3);
    }
    p.set("blob:binary", &[0u8, 159, 146, 150, 255, 10, 13][..]);
    p.set("blob:large", "x".repeat(5 << 20));
    p.set("odd::double:sep", "1");
    p.set("plain", "no separator");
    p.set("emoji:🦀", "ferris");
    p.query_async::<()>(&mut con).await?;

    let size: u64 = redis::cmd("DBSIZE").query_async(&mut con).await?;
    println!(
        "seeded {n} keys in {:?}, dbsize = {size}",
        started.elapsed()
    );
    Ok(())
}
