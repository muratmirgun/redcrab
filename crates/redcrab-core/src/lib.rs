//! UI-agnostic Redis layer shared by the desktop and terminal front ends.
//!
//! All network I/O runs on a small dedicated tokio runtime. The public async
//! functions spawn onto it and hand back plain futures, so callers can await
//! them from any executor (GPUI's, a TUI loop, or tokio itself).

pub mod analysis;
pub mod browser;
pub mod bulk;
mod client;
pub mod commands;
mod config;
pub mod console;
pub mod feed;
pub mod form;
pub mod format;
pub mod formats;
pub mod progress;
mod runtime;
pub mod server;
pub mod settings;
pub mod ssh;
pub mod store;
mod tree;
pub mod update;
mod value;
pub mod viewer;

pub use client::{
    AnalysisProgress, COUNT_EXACT_MAX, Client, ConnectOptions, ConsoleSession, DELETE_BATCH,
    DbInfo, DeleteProgress, DeleteStats, EditError, Key, ListEnd, Monitor, NewValue, ReadOnlyError,
    Removed, ScanEvent, ScanOptions, ScanProgress, ServerInfo, ServerNode, Subscriber, key_slot,
};
pub use config::{
    ConfigField, ConnectionConfig, Environment, HostKeyPolicy, InvalidConfig, SshAuth, SshConfig,
    TlsConfig, Topology, UNIX_SOCKETS, format_address, parse_address, split_addresses,
};
pub use redis::Value as RawValue;
pub use tree::{KeyTree, NodeId, Row, RowKind};
pub use value::{Cursor, KeyMeta, KeyType, StreamEntry, ValueData, ValuePage};
