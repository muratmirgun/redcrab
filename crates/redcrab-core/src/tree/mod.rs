//! Folder view of the keyspace, built incrementally as SCAN batches arrive.
//!
//! Sized for millions of keys. Every path segment is a 16-byte node; what
//! only folders need (child list, sort state) lives in a side table; names
//! share chunked byte buffers; and child lookup goes through one hash table
//! keyed by `(parent, segment)`. Nothing is allocated per key, and node and
//! name storage grows in chunks rather than by reallocating (see [`arena`]).
//!
//! The scan inserts while holding the tree's lock, so the UI waits for
//! whatever an insert does. The one big step left is a hash table growing,
//! which moves all its entries: so the index is split into shards that
//! grow one at a time, and its entries carry half of their hash, so that
//! growing never reads a name. At two million keys the longest insert batch
//! went from about 70 ms to about 1.5 ms.
//!
//! Flattening only walks expanded folders and, while keys stream in, only
//! sorts what is new: new children are sorted on their own and merged into
//! the sorted ones in place, with one scratch buffer kept for every sort
//! (sort buffers sized anew on every redraw are what made a scanning
//! window's memory grow).

mod arena;
#[cfg(test)]
mod tests;

use std::{cmp::Ordering, hash::BuildHasher};

use hashbrown::HashTable;

use self::arena::{Chunked, Names};
use crate::client::Key;

pub type NodeId = u32;
type FolderId = u32;

const ROOT: NodeId = 0;
/// The root's folder, created with the tree.
const ROOT_FOLDER: FolderId = 0;
const NO_FOLDER: FolderId = FolderId::MAX;

// `Node::bits` holds the parent id and three flags.
const IS_KEY: u32 = 1 << 31;
const EXPANDED: u32 = 1 << 30;
/// The node was last sorted among its parent's folders rather than its
/// keys (see [`KeyTree::sort_children`]).
const SORTED_AS_FOLDER: u32 = 1 << 29;
const PARENT: u32 = SORTED_AS_FOLDER - 1;
/// Every node id must fit in the parent bits.
const MAX_NODES: usize = PARENT as usize + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// One path segment. A tree of a million keys has over a million of these,
/// so every byte here counts. Everything sorting and flattening read is
/// here, so neither has to look into `folders` for every row.
#[derive(Clone, Copy)]
struct Node {
    /// Where the segment name starts in [`Names`].
    name: u32,
    /// The parent's id, plus [`IS_KEY`], [`EXPANDED`] and
    /// [`SORTED_AS_FOLDER`].
    bits: u32,
    /// The node's entry in `folders` once it has had a child.
    folder: FolderId,
    /// Live keys in this subtree, the node's own key included.
    keys: u32,
}

impl Node {
    fn parent(self) -> NodeId {
        self.bits & PARENT
    }

    fn is_key(self) -> bool {
        self.bits & IS_KEY != 0
    }

    fn is_expanded(self) -> bool {
        self.bits & EXPANDED != 0
    }

    fn sorted_as_folder(self) -> bool {
        self.bits & SORTED_AS_FOLDER != 0
    }

    /// Whether the node has live keys below it, which is what makes it a
    /// folder row. A node whose children were all removed is a plain key
    /// again.
    fn is_folder(self) -> bool {
        self.keys > self.is_key() as u32
    }
}

/// What only nodes with children need.
struct Folder {
    children: Vec<NodeId>,
    /// Length of the prefix of `children` that is in display order, each
    /// child ranked as the kind of row it was sorted as.
    sorted_len: u32,
    /// A child in the sorted prefix now shows as the other kind of row (a
    /// key that got keys below it, say), so it has to move.
    stale: bool,
}

pub struct KeyTree {
    separator: Option<u8>,
    nodes: Chunked<Node>,
    folders: Chunked<Folder>,
    names: Names,
    /// [`INDEX_SHARDS`] tables of [`entry`]s, picked by [`shard`].
    index: Box<[HashTable<u64>]>,
    hasher: foldhash::fast::FixedState,
    /// `nodes.len()` when the last flatten sorted. Newer nodes have not been
    /// sorted anywhere yet: they are in the unsorted tails of their parents'
    /// child lists.
    sorted_nodes: u32,
    /// Reused by every sort, so that flattening as keys stream in does not
    /// allocate a buffer per redraw.
    scratch: Vec<SortKey>,
}

impl KeyTree {
    /// `separator` splits keys into folders; `None` gives a flat list.
    pub fn new(separator: Option<u8>) -> Self {
        let mut nodes = Chunked::default();
        nodes.push(Node {
            name: 0,
            bits: ROOT | EXPANDED,
            folder: ROOT_FOLDER,
            keys: 0,
        });
        let mut folders = Chunked::default();
        folders.push(Folder {
            children: Vec::new(),
            sorted_len: 0,
            stale: false,
        });
        Self {
            separator,
            nodes,
            folders,
            names: Names::new(),
            index: (0..INDEX_SHARDS).map(|_| HashTable::new()).collect(),
            hasher: foldhash::fast::FixedState::default(),
            sorted_nodes: 0,
            scratch: Vec::new(),
        }
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.nodes[ROOT].keys as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes allocated on the heap, unused capacity included.
    pub fn heap_bytes(&self) -> usize {
        self.nodes.heap_bytes()
            + self.folders.heap_bytes()
            + self
                .folders
                .iter()
                .map(|f| f.children.capacity() * size_of::<NodeId>())
                .sum::<usize>()
            + self.names.heap_bytes()
            + self.index.len() * size_of::<HashTable<u64>>()
            + self
                .index
                .iter()
                .map(HashTable::allocation_size)
                .sum::<usize>()
            + self.scratch.capacity() * size_of::<SortKey>()
    }

    /// Adds a key. Returns `false` if it was already present (SCAN may return
    /// a key more than once), or in the unlikely case that the tree is full
    /// (half a billion segments or 4 GiB of segment names).
    pub fn insert(&mut self, key: &[u8]) -> bool {
        let mut node = ROOT;
        match self.separator {
            Some(sep) => {
                for segment in key.split(|b| *b == sep) {
                    match self.child_or_insert(node, segment) {
                        Some(child) => node = child,
                        None => return false,
                    }
                }
            }
            None => match self.child_or_insert(ROOT, key) {
                Some(child) => node = child,
                None => return false,
            },
        }
        let n = &mut self.nodes[node];
        if n.is_key() {
            return false;
        }
        n.bits |= IS_KEY;
        self.adjust_counts(node, 1);
        true
    }

    /// Marks a key as gone. Its node stays in the arena but is no longer shown.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        let Some(node) = self.find(key) else {
            return false;
        };
        let n = &mut self.nodes[node];
        if !n.is_key() {
            return false;
        }
        n.bits &= !IS_KEY;
        self.adjust_counts(node, -1);
        true
    }

    /// Marks every key below `node` as gone, but not `node`'s own key: what
    /// is left of a folder once the keys `prefix + separator + *` are
    /// deleted. Returns how many keys went. Like [`Self::remove`], the nodes
    /// stay in the arena; they show again if their keys are inserted again.
    pub fn remove_below(&mut self, node: NodeId) -> u32 {
        let n = self.nodes[node];
        let below = n.keys - n.is_key() as u32;
        if below == 0 {
            return 0;
        }
        // Every node below ends up without keys, so each is cleared outright
        // and only `node` and its ancestors are counted down. Nodes without
        // keys are hidden wherever they were sorted; see `sort_children`.
        let mut pending = vec![n.folder];
        while let Some(folder) = pending.pop() {
            for &child in &self.folders[folder].children {
                let n = &mut self.nodes[child];
                if n.keys == 0 {
                    continue;
                }
                n.keys = 0;
                n.bits &= !IS_KEY;
                if n.folder != NO_FOLDER {
                    pending.push(n.folder);
                }
            }
        }
        self.adjust_counts(node, -(below as i32));
        below
    }

    /// Whether `node` is `ancestor` or lies below it.
    pub fn is_under(&self, node: NodeId, ancestor: NodeId) -> bool {
        let mut n = node;
        loop {
            if n == ancestor {
                return true;
            }
            if n == ROOT {
                return false;
            }
            n = self.nodes[n].parent();
        }
    }

    /// The node for a key or folder prefix, if one was ever inserted; it may
    /// no longer hold any key (see [`Self::contains`]).
    pub fn find(&self, key: &[u8]) -> Option<NodeId> {
        match self.separator {
            Some(sep) => key
                .split(|b| *b == sep)
                .try_fold(ROOT, |parent, segment| self.child(parent, segment)),
            None => self.child(ROOT, key),
        }
    }

    /// Whether `key` is a live key.
    pub fn contains(&self, key: &[u8]) -> bool {
        self.find(key).is_some_and(|node| self.nodes[node].is_key())
    }

    /// The segment name of a node.
    pub fn name(&self, node: NodeId) -> &[u8] {
        name_of(&self.nodes, &self.names, node)
    }

    /// The full key (or folder prefix, without a trailing separator) a node
    /// stands for.
    pub fn key(&self, node: NodeId) -> Key {
        let mut segments = Vec::new();
        let mut n = node;
        while n != ROOT {
            segments.push(n);
            n = self.nodes[n].parent();
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
        self.nodes[node].keys
    }

    /// Whether a node is a live key.
    pub fn is_key(&self, node: NodeId) -> bool {
        self.nodes[node].is_key()
    }

    /// Whether a node has live keys below it, which shows it as a folder.
    pub fn is_folder(&self, node: NodeId) -> bool {
        self.nodes[node].is_folder()
    }

    /// The folder a node is in; `None` for a node at the top level.
    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        let parent = self.nodes[node].parent();
        (node != ROOT && parent != ROOT).then_some(parent)
    }

    pub fn is_expanded(&self, node: NodeId) -> bool {
        self.nodes[node].is_expanded()
    }

    pub fn set_expanded(&mut self, node: NodeId, expanded: bool) {
        let n = &mut self.nodes[node];
        if expanded {
            n.bits |= EXPANDED;
        } else {
            n.bits &= !EXPANDED;
        }
    }

    pub fn toggle(&mut self, node: NodeId) {
        self.nodes[node].bits ^= EXPANDED;
    }

    /// Opens or closes `node` and every folder below it.
    pub fn set_expanded_below(&mut self, node: NodeId, expanded: bool) {
        self.set_expanded(node, expanded);
        let mut pending: Vec<FolderId> = Vec::new();
        pending.extend(Some(self.nodes[node].folder).filter(|&f| f != NO_FOLDER));
        while let Some(folder) = pending.pop() {
            for &child in &self.folders[folder].children {
                let n = &mut self.nodes[child];
                if expanded {
                    n.bits |= EXPANDED;
                } else {
                    n.bits &= !EXPANDED;
                }
                if n.folder != NO_FOLDER {
                    pending.push(n.folder);
                }
            }
        }
    }

    pub fn collapse_all(&mut self) {
        // The root is always shown open, whatever its flag says.
        for n in self.nodes.iter_mut() {
            n.bits &= !EXPANDED;
        }
    }

    /// Opens every folder, e.g. to show all matches of a small filtered scan.
    pub fn expand_all(&mut self) {
        for n in self.nodes.iter_mut() {
            n.bits |= EXPANDED;
        }
    }

    /// The visible rows: folders first, then keys, each in byte order. A key
    /// that is also a folder prefix shows up as both a key row and a folder
    /// row, next to each other among the folders.
    pub fn rows(&mut self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.rows_into(&mut rows);
        rows
    }

    /// [`Self::rows`] into a reused buffer, which a view that re-flattens
    /// while keys stream in keeps instead of allocating a new one each time.
    pub fn rows_into(&mut self, rows: &mut Vec<Row>) {
        rows.clear();
        self.sort_expanded();
        self.emit(rows);
    }

    /// Brings every expanded folder's children into display order.
    fn sort_expanded(&mut self) {
        self.sorted_nodes = self.nodes.len() as u32;
        let mut scratch = std::mem::take(&mut self.scratch);
        let mut pending = vec![ROOT_FOLDER];
        while let Some(f) = pending.pop() {
            let folder = &self.folders[f];
            if folder.stale || folder.sorted_len as usize != folder.children.len() {
                let (sorted_len, stale) = (folder.sorted_len as usize, folder.stale);
                let mut children = std::mem::take(&mut self.folders[f].children);
                self.sort_children(&mut children, sorted_len, stale, &mut scratch);
                let folder = &mut self.folders[f];
                folder.sorted_len = children.len() as u32;
                folder.stale = false;
                folder.children = children;
                // The children just placed were ranked as the kind they show as now.
                for key in &scratch {
                    let n = &mut self.nodes[key.id];
                    if key.folder {
                        n.bits |= SORTED_AS_FOLDER;
                    } else {
                        n.bits &= !SORTED_AS_FOLDER;
                    }
                }
            }
            for &child in &self.folders[f].children {
                let n = self.nodes[child];
                if n.is_expanded() && n.is_folder() {
                    pending.push(n.folder);
                }
            }
        }
        scratch.clear();
        self.scratch = scratch;
    }

    /// Orders folders (nodes with live keys below them) before keys, each
    /// in byte order. `children[..sorted_len]` is in order already, each
    /// child ranked as the kind it was sorted as ([`SORTED_AS_FOLDER`]).
    ///
    /// Only the children whose place is not known are sorted, in `scratch`:
    /// the unsorted tail and, if the folder is `stale`, the sorted children
    /// that now show as the other kind. They are then merged into the
    /// sorted ones from the back, in place, so a re-flatten while keys
    /// stream in costs no allocation and no full sort. On return `scratch`
    /// holds exactly the children that were (re)ranked, with the kind they
    /// were ranked as.
    ///
    /// A child without live keys is not shown, so it may keep its place
    /// whatever kind it was sorted as; it is only moved once it shows again.
    fn sort_children(
        &self,
        children: &mut Vec<NodeId>,
        sorted_len: usize,
        stale: bool,
        scratch: &mut Vec<SortKey>,
    ) {
        let total = children.len();
        scratch.clear();
        let mut kept = sorted_len;
        if stale {
            kept = 0;
            for i in 0..sorted_len {
                let id = children[i];
                let n = self.nodes[id];
                if n.keys != 0 && n.is_folder() != n.sorted_as_folder() {
                    scratch.push(self.sort_key(id, n.is_folder()));
                } else {
                    children[kept] = id;
                    kept += 1;
                }
            }
        }
        scratch.extend(children[sorted_len..].iter().map(|&id| {
            let n = self.nodes[id];
            self.sort_key(id, n.is_folder())
        }));
        let cmp = |a: &SortKey, b: &SortKey| self.compare(a, b);
        scratch.sort_unstable_by(cmp);
        children.truncate(kept);
        if kept == 0 {
            children.extend(scratch.iter().map(|key| key.id));
            return;
        }
        children.resize(total, ROOT);
        // Merge from the back: the slot written next is always past the
        // sorted children not yet moved, since it is `i + j` (both counts).
        let (mut i, mut j) = (kept, scratch.len());
        let mut sorted: Option<SortKey> = None;
        while j > 0 {
            if i == 0 {
                for (slot, key) in children[..j].iter_mut().zip(&scratch[..j]) {
                    *slot = key.id;
                }
                break;
            }
            let last = *sorted.get_or_insert_with(|| {
                let id = children[i - 1];
                self.sort_key(id, self.nodes[id].sorted_as_folder())
            });
            if cmp(&last, &scratch[j - 1]) == Ordering::Greater {
                children[i + j - 1] = last.id;
                i -= 1;
                sorted = None;
            } else {
                children[i + j - 1] = scratch[j - 1].id;
                j -= 1;
            }
        }
    }

    /// What `id` is ordered by, ranked as a folder or a key.
    fn sort_key(&self, id: NodeId, folder: bool) -> SortKey {
        let name = self.name(id);
        SortKey {
            head: name_head(name),
            name: self.nodes[id].name,
            len: name.len() as u32,
            id,
            folder,
        }
    }

    /// Folders first, then byte order of the names. Most comparisons are
    /// decided by the heads; a tie reads both names straight from `names`
    /// and compares what follows the bytes the heads already matched.
    fn compare(&self, a: &SortKey, b: &SortKey) -> Ordering {
        b.folder
            .cmp(&a.folder)
            .then(a.head.cmp(&b.head))
            .then_with(|| {
                let a = self.names.slice(a.name, a.len);
                let b = self.names.slice(b.name, b.len);
                let same = a.len().min(b.len()).min(8);
                a[same..].cmp(&b[same..])
            })
    }

    /// Depth-first walk of the expanded folders, without recursion: a key can
    /// have thousands of separators.
    fn emit(&self, rows: &mut Vec<Row>) {
        // (folder, next child to visit, depth of its children)
        let mut stack: Vec<(FolderId, usize, u16)> = vec![(ROOT_FOLDER, 0, 0)];
        'folders: while let Some(&(f, start, depth)) = stack.last() {
            let children = &self.folders[f].children;
            for (i, &child) in children.iter().enumerate().skip(start) {
                let n = self.nodes[child];
                if n.keys == 0 {
                    continue;
                }
                if n.is_key() {
                    rows.push(Row {
                        node: child,
                        depth,
                        kind: RowKind::Key,
                    });
                }
                if n.is_folder() {
                    rows.push(Row {
                        node: child,
                        depth,
                        kind: RowKind::Folder,
                    });
                    if n.is_expanded() {
                        if let Some(top) = stack.last_mut() {
                            top.1 = i + 1;
                        }
                        stack.push((n.folder, 0, depth.saturating_add(1)));
                        continue 'folders;
                    }
                }
            }
            stack.pop();
        }
    }

    /// Adds `delta` to the key count of `node` and of every ancestor. A
    /// node that now shows as the other kind of row than it was sorted as
    /// (a key that gained keys below it, a folder down to its own key, or a
    /// node showing again after its keys were all removed) has to move in
    /// its parent's list, which is marked for the next sort. Nodes newer
    /// than the last sort need nothing: they are not sorted yet.
    fn adjust_counts(&mut self, node: NodeId, delta: i32) {
        let mut id = node;
        loop {
            let n = &mut self.nodes[id];
            n.keys = n.keys.wrapping_add_signed(delta);
            let n = *n;
            if id == ROOT {
                return;
            }
            if id < self.sorted_nodes && n.keys != 0 && n.is_folder() != n.sorted_as_folder() {
                let folder = self.nodes[n.parent()].folder;
                self.folders[folder].stale = true;
            }
            id = n.parent();
        }
    }

    fn hash(&self, parent: NodeId, name: &[u8]) -> u64 {
        self.hasher.hash_one((parent, name))
    }

    /// The index entry of `(parent, name)` hashing to `hash`, if there is one.
    fn find_entry(&self, hash: u64, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        self.index[shard(hash)]
            .find(table_hash(hash), |&entry| {
                entry >> 32 == hash >> 32 && {
                    let id = entry as NodeId;
                    self.nodes[id].parent() == parent && self.name(id) == name
                }
            })
            .map(|&entry| entry as NodeId)
    }

    fn child(&self, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        self.find_entry(self.hash(parent, name), parent, name)
    }

    /// The child of `parent` called `name`, created if needed. `None` only
    /// when the tree is full.
    fn child_or_insert(&mut self, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        let hash = self.hash(parent, name);
        if let Some(id) = self.find_entry(hash, parent, name) {
            return Some(id);
        }
        if self.nodes.len() >= MAX_NODES {
            return None;
        }
        // Names are appended in node order: see `Names`.
        let start = self.names.push(name)?;
        let id = self.nodes.push(Node {
            name: start,
            bits: parent,
            folder: NO_FOLDER,
            keys: 0,
        });

        let folder = match self.nodes[parent].folder {
            NO_FOLDER => {
                let folder = self.folders.push(Folder {
                    children: Vec::new(),
                    sorted_len: 0,
                    stale: false,
                });
                self.nodes[parent].folder = folder;
                folder
            }
            folder => folder,
        };
        self.folders[folder].children.push(id);
        // Growing a shard rehashes its entries from what they carry.
        self.index[shard(hash)].insert_unique(table_hash(hash), entry(hash, id), |&entry| {
            table_hash(entry)
        });
        Some(id)
    }
}

/// Shards of the child index. A shard grows on its own, moving 1/64 of the
/// entries, and the shards fill evenly, so they grow at slightly different
/// moments instead of all at once.
const INDEX_SHARDS: usize = 64;

/// The shard of the index a hash of `(parent, name)` goes to. Bits the
/// tables never look at: [`table_hash`] only keeps the high half.
fn shard(hash: u64) -> usize {
    (hash >> 16) as usize % INDEX_SHARDS
}

/// An index entry: the node id in the low half, the high half of the hash
/// of `(parent, name)` in the high half. The tables work on [`table_hash`],
/// which the entry alone determines, so growing one never resolves a name
/// (through two chunk tables per node, that was most of the cost).
fn entry(hash: u64, id: NodeId) -> u64 {
    (hash & !u64::from(u32::MAX)) | u64::from(id)
}

/// The hash the table files a `hash` (or an [`entry`] with it) under: its
/// high half, repeated, so the bucket bits and the control byte both come
/// from it.
fn table_hash(hash: u64) -> u64 {
    let high = hash >> 32;
    (high << 32) | high
}

/// What [`KeyTree::sort_children`] orders a child by.
#[derive(Clone, Copy)]
struct SortKey {
    /// The first eight bytes of the name; see [`name_head`].
    head: u64,
    /// Where the name starts in [`Names`], and its length, so a tie on
    /// `head` reads both names without going through `nodes`.
    name: u32,
    len: u32,
    id: NodeId,
    folder: bool,
}

/// The first eight bytes of `name`, zero-padded, as a big-endian number.
/// Comparing heads orders names like comparing the bytes does, except that
/// equal heads (a shared 8-byte prefix, or a name that differs only in
/// trailing zero bytes) need the whole names to decide.
fn name_head(name: &[u8]) -> u64 {
    let mut head = [0; 8];
    let n = name.len().min(8);
    head[..n].copy_from_slice(&name[..n]);
    u64::from_be_bytes(head)
}

/// A node's name ends where the next node's begins (see [`Names`]).
fn name_of<'a>(nodes: &Chunked<Node>, names: &'a Names, id: NodeId) -> &'a [u8] {
    let next = nodes.get(id + 1).map(|n| n.name);
    names.get(nodes[id].name, next)
}
