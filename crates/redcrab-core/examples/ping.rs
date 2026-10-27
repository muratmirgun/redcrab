//! Connects with the given settings and prints a one-line server summary.
//! Handy for checking connection and TLS settings without the GUI.
//!
//!     cargo run -p redcrab-core --example ping -- rediss://host:6380 \
//!         [--ca ca.crt] [--cert client.crt --key client.key]

use anyhow::{Context as _, bail};
use redcrab_core::{Client, ConnectionConfig, format};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .context("usage: ping <redis[s]://...> [--ca FILE] [--cert FILE --key FILE]")?;
    let mut config = ConnectionConfig::parse_url(&url)?;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a file"))?;
        let Some(tls) = config.tls.as_mut() else {
            bail!("{flag} needs a rediss:// URL");
        };
        match flag.as_str() {
            "--ca" => tls.ca_file = Some(value.into()),
            "--cert" => tls.client_cert_file = Some(value.into()),
            "--key" => tls.client_key_file = Some(value.into()),
            _ => bail!("unknown option {flag}"),
        }
    }

    futures::executor::block_on(async {
        let client = Client::connect(config).await?;
        let latency = client.ping().await?;
        let info = client.server_info().await?;
        println!(
            "redis {} ({}) · {} keys · {:.2} ms",
            info.version,
            info.mode,
            format::count(info.db_size),
            latency.as_secs_f64() * 1000.0
        );
        Ok(())
    })
}
