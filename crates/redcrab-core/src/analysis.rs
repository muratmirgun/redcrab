//! A memory analysis of the keyspace: which keys, types and key prefixes
//! take the memory. [`Client::analyze_memory`](crate::Client::analyze_memory)
//! walks the keys and measures them with `MEMORY USAGE`; [`Analysis`] is
//! what it finds, read by a front end while the walk goes on.
//!
//! A large keyspace can be sampled: one key in `n` is measured, and every
//! total is then an estimate, the measured figure times `n`.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    time::Duration,
};

use crate::KeyType;

/// The biggest keys kept.
pub const BIGGEST: usize = 100;
/// How deep the prefix tree goes: `a:b:c:d` counts towards `a`, `a:b`,
/// `a:b:c` and no further.
pub const PREFIX_DEPTH: usize = 3;
/// Distinct prefixes kept under one prefix; the others are summed as one.
/// IDs (`user:1`, `user:2`, ...) would otherwise take as much memory here
/// as on the server.
pub const PREFIX_FANOUT: usize = 500;

/// Keys and the bytes they take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub keys: u64,
    pub bytes: u64,
}

impl Totals {
    fn add(&mut self, bytes: u64) {
        self.keys += 1;
        self.bytes += bytes;
    }

    /// The totals of the whole keyspace these sampled ones stand for.
    pub fn scaled(self, every: u32) -> Self {
        Self {
            keys: self.keys * u64::from(every),
            bytes: self.bytes * u64::from(every),
        }
    }
}

/// One of the biggest keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigKey {
    pub key: Vec<u8>,
    pub kind: KeyType,
    pub bytes: u64,
    /// Milliseconds to live; `None` without a TTL.
    pub ttl_ms: Option<u64>,
}

impl PartialOrd for BigKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bytes
            .cmp(&other.bytes)
            .then_with(|| other.key.cmp(&self.key))
    }
}

#[derive(Clone, Debug, Default)]
struct Prefix {
    totals: Totals,
    children: HashMap<Vec<u8>, Prefix>,
    /// The children past [`PREFIX_FANOUT`], summed.
    others: Totals,
}

/// One row of [`Analysis::prefixes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixRow {
    /// The prefix, from the start of the key (`user:sessions`); `None`
    /// for the row that sums the prefixes past [`PREFIX_FANOUT`].
    pub prefix: Option<Vec<u8>>,
    pub totals: Totals,
    /// Whether it has prefixes under it to look into.
    pub nested: bool,
}

/// What a memory analysis found so far.
#[derive(Clone, Debug)]
pub struct Analysis {
    separator: Option<u8>,
    /// One key in `every` is measured.
    every: u32,
    /// Keys the walk went past, measured or not.
    pub scanned: u64,
    /// Keys measured (deleted ones in between are not).
    pub measured: Totals,
    /// Measured keys without a TTL.
    pub without_ttl: Totals,
    by_type: HashMap<KeyType, Totals>,
    root: Prefix,
    biggest: BinaryHeap<Reverse<BigKey>>,
    /// How long the walk took, once it is over.
    pub elapsed: Option<Duration>,
}

impl Analysis {
    /// An empty analysis splitting keys on `separator` (none: no prefix
    /// tree) and measuring one key in `every`.
    pub fn new(separator: Option<u8>, every: u32) -> Self {
        Self {
            separator,
            every: every.max(1),
            scanned: 0,
            measured: Totals::default(),
            without_ttl: Totals::default(),
            by_type: HashMap::new(),
            root: Prefix::default(),
            biggest: BinaryHeap::new(),
            elapsed: None,
        }
    }

    pub fn every(&self) -> u32 {
        self.every
    }

    pub fn is_sampled(&self) -> bool {
        self.every > 1
    }

    /// Adds a measured key.
    pub fn add(&mut self, key: &[u8], kind: KeyType, ttl_ms: Option<u64>, bytes: u64) {
        self.measured.add(bytes);
        if ttl_ms.is_none() {
            self.without_ttl.add(bytes);
        }
        self.by_type.entry(kind.clone()).or_default().add(bytes);
        if let Some(separator) = self.separator {
            // Every segment but the last, which is the key's own name.
            let depth = key
                .iter()
                .filter(|&&b| b == separator)
                .count()
                .min(PREFIX_DEPTH);
            let mut node = &mut self.root;
            for segment in key.split(|&b| b == separator).take(depth) {
                if !node.children.contains_key(segment) && node.children.len() >= PREFIX_FANOUT {
                    node.others.add(bytes);
                    break;
                }
                node = node.children.entry(segment.to_vec()).or_default();
                node.totals.add(bytes);
            }
        }
        let big = BigKey {
            key: key.to_vec(),
            kind,
            bytes,
            ttl_ms,
        };
        if self.biggest.len() < BIGGEST {
            self.biggest.push(Reverse(big));
        } else if self
            .biggest
            .peek()
            .is_some_and(|Reverse(smallest)| big > *smallest)
        {
            self.biggest.pop();
            self.biggest.push(Reverse(big));
        }
    }

    /// The biggest keys measured, biggest first.
    pub fn biggest(&self) -> Vec<BigKey> {
        let mut keys: Vec<BigKey> = self.biggest.iter().map(|Reverse(k)| k.clone()).collect();
        keys.sort_by(|a, b| b.cmp(a));
        keys
    }

    /// The totals per type, the most memory first (measured, not scaled).
    pub fn by_type(&self) -> Vec<(KeyType, Totals)> {
        let mut types: Vec<_> = self
            .by_type
            .iter()
            .map(|(kind, totals)| (kind.clone(), *totals))
            .collect();
        types.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(b.1.keys.cmp(&a.1.keys)));
        types
    }

    /// The prefixes right under `prefix` (empty: the top ones), the most
    /// memory first, then the sum of those past [`PREFIX_FANOUT`], if any.
    /// Measured, not scaled. Empty for a prefix not in the tree.
    pub fn prefixes(&self, prefix: &[u8]) -> Vec<PrefixRow> {
        let Some(node) = self.find(prefix) else {
            return Vec::new();
        };
        let separator = self.separator.unwrap_or(b':');
        let mut rows: Vec<PrefixRow> = node
            .children
            .iter()
            .map(|(segment, child)| {
                let mut full = prefix.to_vec();
                if !full.is_empty() {
                    full.push(separator);
                }
                full.extend_from_slice(segment);
                PrefixRow {
                    prefix: Some(full),
                    totals: child.totals,
                    nested: !child.children.is_empty() || child.others.keys > 0,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.totals
                .bytes
                .cmp(&a.totals.bytes)
                .then_with(|| a.prefix.cmp(&b.prefix))
        });
        if node.others.keys > 0 {
            rows.push(PrefixRow {
                prefix: None,
                totals: node.others,
                nested: false,
            });
        }
        rows
    }

    fn find(&self, prefix: &[u8]) -> Option<&Prefix> {
        if prefix.is_empty() {
            return Some(&self.root);
        }
        let separator = self.separator?;
        let mut node = &self.root;
        for segment in prefix.split(|&b| b == separator) {
            node = node.children.get(segment)?;
        }
        Some(node)
    }
}

#[cfg(test)]
mod tests;
