//! redcrab in a terminal: the desktop app's connections, key tree, values,
//! console, server tools, memory analysis and Pub/Sub, on the same
//! `redcrab-core`, for a server reached over SSH or a machine without a GPU.
//!
//! `redcrab-tui` opens the saved connections; `redcrab-tui <name>` connects
//! to a saved one, `redcrab-tui redis://host:port/db` to a URL.

mod app;
mod connect;
mod console;
mod editor;
mod input;
mod keys;
mod list;
mod memory;
mod popup;
mod pubsub;
#[cfg(test)]
mod screenshots;
mod server;
#[cfg(test)]
mod tests;
mod theme;
mod value;
mod workspace;

use anyhow::Result;
use redcrab_core::ConnectionConfig;

/// What to show first.
pub enum Start {
    /// The saved connections.
    Pick,
    /// Connect to the saved connection with this name.
    Saved(String),
    /// Connect to this URL.
    Url(Box<ConnectionConfig>),
}

const USAGE: &str = "\
redcrab-tui: Redis and Valkey in a terminal

Usage:
  redcrab-tui              pick a saved connection
  redcrab-tui <name>       connect to the saved connection <name>
  redcrab-tui <url>        connect to redis://[user:pass@]host[:port][/db]
                           (rediss:// for TLS; a Unix socket, a cluster, a
                           Sentinel or an SSH tunnel through a saved one)

Connections are the desktop app's (REDCRAB_CONFIG_DIR moves them); press ?
in the app for its keys. RUST_LOG=debug writes a log to redcrab-tui.log in
the temporary directory.";

fn main() -> Result<()> {
    let arg = std::env::args().nth(1);
    let start = match arg.as_deref() {
        Some("--version" | "-V") => {
            println!("redcrab-tui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(url) if url.contains("://") => match ConnectionConfig::parse_url(url) {
            Ok(config) => Start::Url(Box::new(config)),
            Err(err) => {
                eprintln!("redcrab-tui: {err:#}");
                std::process::exit(2);
            }
        },
        Some(name) => Start::Saved(name.to_owned()),
        None => Start::Pick,
    };
    log_to_file();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(app::run(start))
}

/// A log on stderr would draw over the screen, so with RUST_LOG set it goes
/// to a file; without, nowhere.
fn log_to_file() {
    let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() else {
        return;
    };
    let path = std::env::temp_dir().join("redcrab-tui.log");
    let Ok(file) = std::fs::File::create(path) else {
        return;
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
}
