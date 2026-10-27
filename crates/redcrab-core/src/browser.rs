//! The key list every front end shows: the tree SCAN fills, its flattened
//! rows, the selection and keyboard navigation, and the type/TTL cache of
//! the keys on screen.
//!
//! No I/O happens here. The model hands out requests ([`ScanRequest`],
//! [`MetaRequest`]) that the front end runs on its [`Client`] however suits
//! its event loop, and takes the answers back. Every request carries the
//! scan generation it belongs to, so answers for a tree that has since been
//! replaced are dropped.
//!
//! [`Client`]: crate::Client

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::Result;

use crate::{
    client::{Key, ScanEvent, ScanOptions},
    format,
    tree::{KeyTree, NodeId, Row, RowKind},
    value::KeyMeta,
};

/// How many keys a scan loads before it stops, unless told otherwise.
pub const DEFAULT_KEY_LIMIT: usize = 2_000_000;
/// A filtered scan with at most this many matches opens every folder, so
/// the matches are visible without clicking through.
pub const EXPAND_ALL_MAX: usize = 5_000;
/// Cached type/TTL entries before the cache starts over. Only rows on
/// screen are fetched, so this bounds memory when scrolling through
/// millions of keys, at the cost of refetching rows scrolled back to.
const META_CACHE_MAX: usize = 50_000;

/// The SCAN pattern for what the user typed: plain text matches anywhere in
/// the key, text with glob characters is used as is. Plain text is matched
/// literally, so a `\` in it (`C:\tmp`) is not taken as a glob escape.
pub fn pattern_for(query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        "*".into()
    } else if query.contains(['*', '?', '[']) {
        query.into()
    } else {
        // Escaping only inserts ASCII backslashes, so the text stays UTF-8.
        format!(
            "*{}*",
            String::from_utf8_lossy(&escape_glob(query.as_bytes()))
        )
    }
}

/// Escapes the glob characters of `bytes` (`* ? [ ] \`), so that a literal
/// key prefix can be used in a MATCH pattern.
pub fn escape_glob(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum ScanStatus {
    #[default]
    Idle,
    Scanning {
        keys: usize,
    },
    Done {
        keys: usize,
        elapsed: Duration,
        /// Stopped at the key limit with keys left out.
        truncated: bool,
    },
    Failed(String),
}

/// A scan for the front end to start with
/// [`Client::scan_into`](crate::Client::scan_into)`(tree, options)` and to
/// feed back through [`KeyBrowser::on_scan_event`] with `generation`.
pub struct ScanRequest {
    pub generation: u64,
    pub tree: Arc<Mutex<KeyTree>>,
    pub options: ScanOptions,
}

/// Type and TTL to fetch with
/// [`Client::key_meta_permitted`](crate::Client::key_meta_permitted)`(keys)`
/// and hand back to [`KeyBrowser::apply_meta`].
pub struct MetaRequest {
    pub generation: u64,
    pub keys: Vec<Key>,
    nodes: Vec<NodeId>,
}

/// A row as the front end drew it, for acting on it later (a click). An
/// index can point at another row by then, since keys stream in between
/// two frames; a `RowId` cannot, and one from before a rescan matches
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowId {
    generation: u64,
    node: NodeId,
    kind: RowKind,
}

/// What a row shows.
#[derive(Clone, Debug, PartialEq)]
pub struct RowView {
    pub id: RowId,
    pub node: NodeId,
    /// The segment name, escaped to one printable line.
    pub name: String,
    pub depth: u16,
    pub kind: RowKind,
    /// Only meaningful for folders.
    pub expanded: bool,
    /// Keys under a folder, the folder's own key included; 1 for a key.
    pub key_count: u32,
    /// Type and TTL of a key, once fetched; never for a key the user's ACL
    /// hides.
    pub meta: Option<KeyMeta>,
    pub selected: bool,
}

/// Where a keyboard move landed: the row to scroll to, and the key to show
/// if the move selected a different key.
#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    pub ix: usize,
    pub key: Option<Key>,
}

pub struct KeyBrowser {
    separator: u8,
    limit: Option<usize>,
    tree: Arc<Mutex<KeyTree>>,
    /// Flattened rows, rebuilt in place when `rows_dirty`.
    rows: Vec<Row>,
    rows_dirty: bool,
    selected: Option<(NodeId, RowKind)>,
    /// Where the selection was last seen in `rows`, to skip a search.
    selected_hint: usize,
    /// A selection carried over a rescan, by key, until the new tree has it.
    reselect: Option<(Key, RowKind)>,
    /// Fetched type/TTL by key; `None` for a key the user may not see, so it
    /// is not asked for again on every frame.
    meta: HashMap<NodeId, Option<KeyMeta>>,
    meta_pending: HashSet<NodeId>,
    generation: u64,
    status: ScanStatus,
    /// Whether the current scan has a filter (decides "keys" vs "matches"
    /// and the expand-all behavior).
    filtered: bool,
}

impl KeyBrowser {
    /// `separator` splits keys into folders; `limit` caps the keys a scan
    /// loads (see [`DEFAULT_KEY_LIMIT`]).
    pub fn new(separator: u8, limit: Option<usize>) -> Self {
        Self {
            separator,
            limit,
            tree: Arc::new(Mutex::new(KeyTree::new(Some(separator)))),
            rows: Vec::new(),
            rows_dirty: false,
            selected: None,
            selected_hint: 0,
            reselect: None,
            meta: HashMap::new(),
            meta_pending: HashSet::new(),
            generation: 0,
            status: ScanStatus::Idle,
            filtered: false,
        }
    }

    fn tree(&self) -> MutexGuard<'_, KeyTree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts over with an empty tree for `query` (see [`pattern_for`]). The
    /// selection is kept by key and comes back once the new scan finds it.
    pub fn begin_scan(&mut self, query: &str) -> ScanRequest {
        let pattern = pattern_for(query);
        self.filtered = pattern != "*";
        if let Some((node, kind)) = self.selected.take() {
            let key = self.tree().key(node);
            self.reselect = Some((key, kind));
        }
        self.generation += 1;
        self.tree = Arc::new(Mutex::new(KeyTree::new(Some(self.separator))));
        self.rows.clear();
        self.rows_dirty = true;
        self.meta.clear();
        self.meta_pending.clear();
        self.status = ScanStatus::Scanning { keys: 0 };
        ScanRequest {
            generation: self.generation,
            tree: self.tree.clone(),
            options: ScanOptions {
                pattern,
                limit: self.limit,
                ..ScanOptions::default()
            },
        }
    }

    /// Applies a scan event. Returns `false` for an event of an older scan,
    /// which changes nothing.
    pub fn on_scan_event(&mut self, generation: u64, event: ScanEvent) -> bool {
        if generation != self.generation {
            return false;
        }
        self.rows_dirty = true;
        self.status = match event {
            ScanEvent::Progress { keys } => ScanStatus::Scanning { keys },
            ScanEvent::Done {
                keys,
                elapsed,
                truncated,
            } => {
                if self.filtered && keys <= EXPAND_ALL_MAX {
                    self.tree().expand_all();
                }
                ScanStatus::Done {
                    keys,
                    elapsed,
                    truncated,
                }
            }
            ScanEvent::Failed(err) => ScanStatus::Failed(err),
        };
        if let Some((key, kind)) = self.reselect.take() {
            let tree = self.tree();
            let is_key = tree.contains(&key);
            let found = tree.find(&key).filter(|&node| match kind {
                RowKind::Key => is_key,
                RowKind::Folder => tree.key_count(node) > is_key as u32,
            });
            drop(tree);
            match found {
                Some(node) => self.selected = Some((node, kind)),
                None => self.reselect = Some((key, kind)),
            }
        }
        if !matches!(self.status, ScanStatus::Scanning { .. }) {
            self.reselect = None;
        }
        true
    }

    pub fn status(&self) -> &ScanStatus {
        &self.status
    }

    pub fn is_scanning(&self) -> bool {
        matches!(self.status, ScanStatus::Scanning { .. })
    }

    /// One line for a status bar: "Scanning… 12,000 keys",
    /// "1,000,000 keys · 512 ms", "42 matches · 3 ms", "Scan failed".
    pub fn status_text(&self) -> String {
        let noun = if self.filtered { "matches" } else { "keys" };
        match &self.status {
            ScanStatus::Idle => String::new(),
            ScanStatus::Scanning { keys: 0 } => "Scanning…".into(),
            ScanStatus::Scanning { keys } => {
                format!("Scanning… {} keys", format::count(*keys as u64))
            }
            ScanStatus::Done { keys, elapsed, .. } => format!(
                "{} {noun} · {:.0} ms",
                format::count(*keys as u64),
                elapsed.as_secs_f64() * 1000.0
            ),
            ScanStatus::Failed(_) => "Scan failed".into(),
        }
    }

    /// Set when the last scan stopped at the key limit.
    pub fn truncation_notice(&self) -> Option<String> {
        match self.status {
            ScanStatus::Done {
                keys,
                truncated: true,
                ..
            } => Some(format!(
                "Showing the first {} keys. Narrow the filter to see the rest.",
                format::count(keys as u64)
            )),
            _ => None,
        }
    }

    /// Distinct keys loaded so far.
    pub fn key_count(&self) -> usize {
        self.tree().len()
    }

    /// Re-flattens the tree if anything changed since the last call.
    fn refresh(&mut self) {
        if self.rows_dirty {
            let tree = self.tree.clone();
            tree.lock()
                .unwrap_or_else(|e| e.into_inner())
                .rows_into(&mut self.rows);
            self.rows_dirty = false;
        }
    }

    /// The visible rows.
    pub fn rows(&mut self) -> &[Row] {
        self.refresh();
        &self.rows
    }

    /// Number of visible rows.
    pub fn len(&mut self) -> usize {
        self.rows().len()
    }

    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    pub fn row_view(&mut self, ix: usize) -> Option<RowView> {
        self.row_views(ix..ix + 1).pop()
    }

    /// The rows in `range` (clamped to the rows that exist), formatted under
    /// one lock of the tree.
    pub fn row_views(&mut self, range: Range<usize>) -> Vec<RowView> {
        self.refresh();
        let range = clamp(range, self.rows.len());
        let tree = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        self.rows[range]
            .iter()
            .map(|row| RowView {
                id: RowId {
                    generation: self.generation,
                    node: row.node,
                    kind: row.kind,
                },
                node: row.node,
                name: format::bytes_inline(tree.name(row.node)).into_owned(),
                depth: row.depth,
                kind: row.kind,
                expanded: row.kind == RowKind::Folder && tree.is_expanded(row.node),
                key_count: match row.kind {
                    RowKind::Folder => tree.key_count(row.node),
                    RowKind::Key => 1,
                },
                meta: match row.kind {
                    RowKind::Key => self.meta.get(&row.node).cloned().flatten(),
                    RowKind::Folder => None,
                },
                selected: self.selected == Some((row.node, row.kind)),
            })
            .collect()
    }

    /// Type and TTL to fetch for the keys in `range` that have neither, or
    /// `None` if there is nothing to fetch. They count as pending until
    /// [`Self::apply_meta`] gets the answer.
    pub fn take_meta_request(&mut self, range: Range<usize>) -> Option<MetaRequest> {
        self.refresh();
        let range = clamp(range, self.rows.len());
        let nodes: Vec<NodeId> = self.rows[range]
            .iter()
            .filter(|r| r.kind == RowKind::Key)
            .map(|r| r.node)
            .filter(|n| !self.meta.contains_key(n) && !self.meta_pending.contains(n))
            .collect();
        if nodes.is_empty() {
            return None;
        }
        let keys = {
            let tree = self.tree();
            nodes.iter().map(|&n| tree.key(n)).collect()
        };
        self.meta_pending.extend(&nodes);
        Some(MetaRequest {
            generation: self.generation,
            keys,
            nodes,
        })
    }

    /// Stores the answer to a [`MetaRequest`]: a type and TTL per key, or
    /// `None` for a key the user may not see, which is then not asked for
    /// again until the next scan. Returns whether anything visible changed.
    /// A failed request (say, the connection dropped) is retried when its
    /// rows are drawn again.
    pub fn apply_meta(
        &mut self,
        request: MetaRequest,
        result: Result<Vec<Option<KeyMeta>>>,
    ) -> bool {
        if request.generation != self.generation {
            return false;
        }
        for node in &request.nodes {
            self.meta_pending.remove(node);
        }
        let Ok(metas) = result else {
            return false;
        };
        if self.meta.len() + metas.len() > META_CACHE_MAX {
            self.meta.clear();
        }
        let shown = metas.iter().any(Option::is_some);
        self.meta.extend(request.nodes.into_iter().zip(metas));
        shown
    }

    /// The selected row's index, if it is visible.
    pub fn selected_index(&mut self) -> Option<usize> {
        self.refresh();
        let (node, kind) = self.selected?;
        let matches = |r: &Row| r.node == node && r.kind == kind;
        if self.rows.get(self.selected_hint).is_some_and(matches) {
            return Some(self.selected_hint);
        }
        let ix = self.rows.iter().position(matches)?;
        self.selected_hint = ix;
        Some(ix)
    }

    /// The selected key, if a key (not a folder) is selected.
    pub fn selected_key(&self) -> Option<Key> {
        match self.selected? {
            (node, RowKind::Key) => Some(self.tree().key(node)),
            (_, RowKind::Folder) => None,
        }
    }

    /// Selects row `ix` of the current rows. A folder opens or closes when
    /// `toggle` is set; a key is returned, for the front end to show.
    ///
    /// For keyboard moves, which work on the rows as they are now. A click
    /// goes through [`Self::click`]: the rows may have changed since the
    /// frame the user clicked in was drawn.
    pub fn select(&mut self, ix: usize, toggle: bool) -> Option<Key> {
        self.refresh();
        let row = *self.rows.get(ix)?;
        self.selected_hint = ix;
        self.select_node(row.node, row.kind, toggle)
    }

    /// Selects the row the user clicked, as it was drawn ([`RowView::id`]),
    /// wherever it is now: a folder opens or closes, a key is returned for
    /// the front end to show. A row of an earlier scan, or one that has gone
    /// since (a deleted key), is ignored.
    pub fn click(&mut self, id: RowId) -> Option<Key> {
        if id.generation != self.generation {
            return None;
        }
        let live = {
            let tree = self.tree();
            match id.kind {
                RowKind::Key => tree.is_key(id.node),
                RowKind::Folder => tree.is_folder(id.node),
            }
        };
        if !live {
            return None;
        }
        self.select_node(id.node, id.kind, true)
    }

    fn select_node(&mut self, node: NodeId, kind: RowKind, toggle: bool) -> Option<Key> {
        self.selected = Some((node, kind));
        self.reselect = None;
        match kind {
            RowKind::Folder => {
                if toggle {
                    self.tree().toggle(node);
                    self.rows_dirty = true;
                }
                None
            }
            RowKind::Key => Some(self.tree().key(node)),
        }
    }

    /// Moves the selection up a row (to the first row if none is selected).
    pub fn select_prev(&mut self) -> Option<Selection> {
        let ix = self.selected_index().map_or(0, |ix| ix.saturating_sub(1));
        self.move_to(ix)
    }

    /// Moves the selection down a row (to the first row if none is selected).
    pub fn select_next(&mut self) -> Option<Selection> {
        let last = self.len().checked_sub(1)?;
        let ix = self.selected_index().map_or(0, |ix| (ix + 1).min(last));
        self.move_to(ix)
    }

    /// Selects row `ix` as a keyboard move does. `None` if there is no such
    /// row. Moving onto the row that is already selected (holding a key at
    /// the end of the list) returns no key, so the value is not reloaded.
    fn move_to(&mut self, ix: usize) -> Option<Selection> {
        if ix >= self.len() {
            return None;
        }
        let key = if self.selected_index() == Some(ix) {
            None
        } else {
            self.select(ix, false)
        };
        Some(Selection { ix, key })
    }

    /// Opens the selected folder. Returns whether it changed anything.
    pub fn expand(&mut self) -> bool {
        self.set_selected_expanded(true)
    }

    /// Closes the selected folder, or, like a file tree, moves from a closed
    /// folder or a key to the folder it is in.
    pub fn collapse(&mut self) -> Option<Selection> {
        let ix = self.selected_index()?;
        let row = self.rows[ix];
        if row.kind == RowKind::Folder && self.tree().is_expanded(row.node) {
            self.set_selected_expanded(false);
            return None;
        }
        // Found by node rather than by depth: depths stop counting at
        // `u16::MAX` for keys with that many separators.
        let parent = self.tree().parent(row.node)?;
        let parent = self.rows[..ix]
            .iter()
            .rposition(|r| r.kind == RowKind::Folder && r.node == parent)?;
        self.move_to(parent)
    }

    fn set_selected_expanded(&mut self, expanded: bool) -> bool {
        let Some((node, RowKind::Folder)) = self.selected else {
            return false;
        };
        let mut tree = self.tree();
        if tree.is_expanded(node) == expanded {
            return false;
        }
        tree.set_expanded(node, expanded);
        drop(tree);
        self.rows_dirty = true;
        true
    }

    pub fn collapse_all(&mut self) {
        self.tree().collapse_all();
        self.rows_dirty = true;
    }

    /// Drops a deleted key from the tree without rescanning.
    pub fn remove_key(&mut self, key: &[u8]) {
        let node = {
            let mut tree = self.tree();
            let node = tree.find(key);
            tree.remove(key);
            node
        };
        if let Some(node) = node {
            self.meta.remove(&node);
            if self.selected == Some((node, RowKind::Key)) {
                self.selected = None;
            }
        }
        self.rows_dirty = true;
    }
}

fn clamp(range: Range<usize>, len: usize) -> Range<usize> {
    range.start.min(len)..range.end.min(len)
}

#[cfg(test)]
mod tests;
