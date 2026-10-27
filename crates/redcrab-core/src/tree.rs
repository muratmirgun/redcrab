//! Folder view of the keyspace, built incrementally as SCAN batches arrive.
//!
//! Sized for millions of keys: nodes live in one arena, segment names in one
//! byte buffer, and child lookup goes through a single hash table keyed by
//! `(parent, segment)`, so there is no per-node allocation besides the child
//! list of folders. Flattening only walks expanded folders.

use std::{cmp::Ordering, hash::BuildHasher};

use hashbrown::HashTable;

use crate::client::Key;

pub type NodeId = u32;

const ROOT: NodeId = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Folder,
    Key,
}

/// One visible line of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub node: NodeId,
    pub depth: u16,
    pub kind: RowKind,
}

struct Node {
    name_start: u32,
    name_len: u32,
    parent: NodeId,
    /// Live keys in this subtree, this node included.
    keys: u32,
    /// Length of the prefix of `children` that is already in order.
    sorted_len: u32,
    children: Vec<NodeId>,
    is_key: bool,
    expanded: bool,
}

pub struct KeyTree {
    separator: Option<u8>,
    nodes: Vec<Node>,
    names: Vec<u8>,
    index: HashTable<NodeId>,
    hasher: foldhash::fast::FixedState,
}

impl KeyTree {
    /// `separator` splits keys into folders; `None` gives a flat list.
    pub fn new(separator: Option<u8>) -> Self {
        let root = Node {
            name_start: 0,
            name_len: 0,
            parent: ROOT,
            keys: 0,
            sorted_len: 0,
            children: Vec::new(),
            is_key: false,
            expanded: true,
        };
        Self {
            separator,
            nodes: vec![root],
            names: Vec::new(),
            index: HashTable::new(),
            hasher: foldhash::fast::FixedState::default(),
        }
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.nodes[ROOT as usize].keys as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Adds a key. Returns `false` if it was already present (SCAN may return
    /// a key more than once).
    pub fn insert(&mut self, key: &[u8]) -> bool {
        let mut node = ROOT;
        match self.separator {
            Some(sep) => {
                for segment in key.split(|b| *b == sep) {
                    node = self.child_or_insert(node, segment);
                }
            }
            None => node = self.child_or_insert(ROOT, key),
        }
        let n = &mut self.nodes[node as usize];
        if n.is_key {
            return false;
        }
        n.is_key = true;
        self.adjust_counts(node, 1);
        true
    }

    /// Marks a key as gone. Its node stays in the arena but is no longer shown.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        let Some(node) = self.find(key) else {
            return false;
        };
        let n = &mut self.nodes[node as usize];
        if !n.is_key {
            return false;
        }
        n.is_key = false;
        self.adjust_counts(node, -1);
        true
    }

    pub fn find(&self, key: &[u8]) -> Option<NodeId> {
        match self.separator {
            Some(sep) => key
                .split(|b| *b == sep)
                .try_fold(ROOT, |parent, segment| self.child(parent, segment)),
            None => self.child(ROOT, key),
        }
    }

    /// The segment name of a node.
    pub fn name(&self, node: NodeId) -> &[u8] {
        let n = &self.nodes[node as usize];
        &self.names[n.name_start as usize..(n.name_start + n.name_len) as usize]
    }

    /// The full key (or folder prefix, without a trailing separator) a node
    /// stands for.
    pub fn key(&self, node: NodeId) -> Key {
        let mut segments = Vec::new();
        let mut n = node;
        while n != ROOT {
            segments.push(n);
            n = self.nodes[n as usize].parent;
        }
        let mut out = Vec::new();
        for (i, &segment) in segments.iter().rev().enumerate() {
            if i > 0 {
                out.extend(self.separator);
            }
            out.extend_from_slice(self.name(segment));
        }
        out.into()
    }

    /// Keys under a node, the node itself included.
    pub fn key_count(&self, node: NodeId) -> u32 {
        self.nodes[node as usize].keys
    }

    pub fn is_expanded(&self, node: NodeId) -> bool {
        self.nodes[node as usize].expanded
    }

    pub fn set_expanded(&mut self, node: NodeId, expanded: bool) {
        self.nodes[node as usize].expanded = expanded;
    }

    pub fn toggle(&mut self, node: NodeId) {
        let n = &mut self.nodes[node as usize];
        n.expanded = !n.expanded;
    }

    pub fn collapse_all(&mut self) {
        for n in &mut self.nodes[1..] {
            n.expanded = false;
        }
    }

    /// The visible rows: folders first, then keys, each in byte order. A key
    /// that is also a folder prefix shows up as both a key row and a folder row.
    pub fn rows(&mut self) -> Vec<Row> {
        self.sort_expanded(ROOT);
        let mut rows = Vec::new();
        self.emit(ROOT, 0, &mut rows);
        rows
    }

    fn sort_expanded(&mut self, node: NodeId) {
        let n = &self.nodes[node as usize];
        if n.sorted_len as usize != n.children.len() {
            let mut children = std::mem::take(&mut self.nodes[node as usize].children);
            let sorted_len = self.nodes[node as usize].sorted_len as usize;
            let cmp = |a: &NodeId, b: &NodeId| self.compare(*a, *b);
            children[sorted_len..].sort_unstable_by(cmp);
            // Two sorted runs: the stable sort detects them and just merges.
            children.sort_by(cmp);
            let n = &mut self.nodes[node as usize];
            n.sorted_len = children.len() as u32;
            n.children = children;
        }
        for i in 0..self.nodes[node as usize].children.len() {
            let child = self.nodes[node as usize].children[i];
            let c = &self.nodes[child as usize];
            if c.expanded && !c.children.is_empty() {
                self.sort_expanded(child);
            }
        }
    }

    fn compare(&self, a: NodeId, b: NodeId) -> Ordering {
        let (na, nb) = (&self.nodes[a as usize], &self.nodes[b as usize]);
        na.children
            .is_empty()
            .cmp(&nb.children.is_empty())
            .then_with(|| self.name(a).cmp(self.name(b)))
    }

    fn emit(&self, node: NodeId, depth: u16, rows: &mut Vec<Row>) {
        for &child in &self.nodes[node as usize].children {
            let c = &self.nodes[child as usize];
            if c.keys == 0 {
                continue;
            }
            if c.is_key {
                rows.push(Row {
                    node: child,
                    depth,
                    kind: RowKind::Key,
                });
            }
            if c.keys > c.is_key as u32 {
                rows.push(Row {
                    node: child,
                    depth,
                    kind: RowKind::Folder,
                });
                if c.expanded {
                    self.emit(child, depth + 1, rows);
                }
            }
        }
    }

    fn adjust_counts(&mut self, mut node: NodeId, delta: i32) {
        loop {
            let n = &mut self.nodes[node as usize];
            n.keys = n.keys.wrapping_add_signed(delta);
            if node == ROOT {
                break;
            }
            node = n.parent;
        }
    }

    fn hash(&self, parent: NodeId, name: &[u8]) -> u64 {
        self.hasher.hash_one((parent, name))
    }

    fn child(&self, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        let hash = self.hash(parent, name);
        self.index
            .find(hash, |&id| {
                self.nodes[id as usize].parent == parent && self.name(id) == name
            })
            .copied()
    }

    fn child_or_insert(&mut self, parent: NodeId, name: &[u8]) -> NodeId {
        if let Some(id) = self.child(parent, name) {
            return id;
        }
        let id = self.nodes.len() as NodeId;
        let name_start = self.names.len() as u32;
        self.names.extend_from_slice(name);
        self.nodes.push(Node {
            name_start,
            name_len: name.len() as u32,
            parent,
            keys: 0,
            sorted_len: 0,
            children: Vec::new(),
            is_key: false,
            expanded: false,
        });

        let p = &mut self.nodes[parent as usize];
        if p.children.is_empty() && parent != ROOT {
            // The parent just became a folder, which moves it in its own
            // parent's order.
            let grandparent = p.parent;
            self.nodes[grandparent as usize].sorted_len = 0;
        }
        self.nodes[parent as usize].children.push(id);

        let hash = self.hash(parent, name);
        let Self {
            index,
            nodes,
            names,
            hasher,
            ..
        } = self;
        index.insert_unique(hash, id, |&id| {
            let n = &nodes[id as usize];
            let name = &names[n.name_start as usize..(n.name_start + n.name_len) as usize];
            hasher.hash_one((n.parent, name))
        });
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(tree: &mut KeyTree) -> Vec<String> {
        tree.rows()
            .iter()
            .map(|row| {
                let name = String::from_utf8_lossy(tree.name(row.node));
                let mark = match row.kind {
                    RowKind::Folder => format!("{name}/ ({})", tree.key_count(row.node)),
                    RowKind::Key => name.into_owned(),
                };
                format!("{}{mark}", "  ".repeat(row.depth as usize))
            })
            .collect()
    }

    #[test]
    fn builds_folders() {
        let mut tree = KeyTree::new(Some(b':'));
        for key in [
            "user:2:name",
            "user:1:name",
            "user:1:age",
            "config",
            "user",
            "user:1:name",
        ] {
            tree.insert(key.as_bytes());
        }
        assert_eq!(tree.len(), 5);
        assert_eq!(labels(&mut tree), ["user", "user/ (4)", "config"]);

        let user = tree.find(b"user").unwrap();
        tree.set_expanded(user, true);
        let one = tree.find(b"user:1").unwrap();
        tree.set_expanded(one, true);
        assert_eq!(
            labels(&mut tree),
            [
                "user",
                "user/ (4)",
                "  1/ (2)",
                "    age",
                "    name",
                "  2/ (1)",
                "config"
            ]
        );
        assert_eq!(&*tree.key(tree.find(b"user:1:age").unwrap()), b"user:1:age");
    }

    #[test]
    fn removes_keys() {
        let mut tree = KeyTree::new(Some(b':'));
        tree.insert(b"a:b");
        tree.insert(b"a:c");
        assert!(tree.remove(b"a:b"));
        assert!(!tree.remove(b"a:b"));
        assert_eq!(tree.len(), 1);
        assert!(tree.remove(b"a:c"));
        assert!(tree.rows().is_empty());
    }

    #[test]
    fn keeps_odd_keys_intact() {
        let mut tree = KeyTree::new(Some(b':'));
        for key in [&b"::"[..], b"a::b", b"trailing:", b"\xff\x00:x"] {
            tree.insert(key);
            assert_eq!(&*tree.key(tree.find(key).unwrap()), key);
        }
        assert_eq!(tree.len(), 4);
    }

    #[test]
    fn merges_new_keys_into_sorted_folders() {
        let mut tree = KeyTree::new(None);
        for key in ["d", "b"] {
            tree.insert(key.as_bytes());
        }
        assert_eq!(labels(&mut tree), ["b", "d"]);
        for key in ["c", "a", "e"] {
            tree.insert(key.as_bytes());
        }
        assert_eq!(labels(&mut tree), ["a", "b", "c", "d", "e"]);
    }
}
