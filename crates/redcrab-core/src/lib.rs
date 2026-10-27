//! UI-agnostic Redis layer shared by the desktop and terminal front ends.
//!
//! All network I/O runs on a small dedicated tokio runtime. The public async
//! functions spawn onto it and hand back plain futures, so callers can await
//! them from any executor (GPUI's, a TUI loop, or tokio itself).

mod client;
mod config;
pub mod format;
mod runtime;
pub mod store;
mod tree;
mod value;

pub use client::{Client, Key, ScanEvent, ScanOptions, ServerInfo};
pub use config::{ConnectionConfig, TlsConfig};
pub use redis::Value as RawValue;
pub use tree::{KeyTree, NodeId, Row, RowKind};
pub use value::{Cursor, KeyMeta, KeyType, StreamEntry, ValueData, ValuePage};
