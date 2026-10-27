use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use futures::StreamExt as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    Window,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        notification::Notification,
        scroll::ScrollableElement as _,
        spinner::Spinner,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, uniform_list,
};
use redcrab_core::{
    Client, Key, KeyMeta, KeyTree, NodeId, Row, RowKind, ScanEvent, ScanOptions, format,
};

use crate::{Collapse, Expand, SelectNext, SelectPrev, style};

const ROW_HEIGHT: f32 = 26.;
const INDENT: f32 = 14.;

pub enum KeyEvent {
    Selected(Key),
}

/// The searchable key tree. Keys stream in from SCAN on the I/O threads;
/// this view only flattens the expanded part and draws the visible rows.
pub struct KeyBrowser {
    client: Client,
    tree: Arc<Mutex<KeyTree>>,
    rows: Vec<Row>,
    rows_dirty: bool,
    selected: Option<(NodeId, RowKind)>,
    meta: HashMap<NodeId, KeyMeta>,
    meta_pending: HashSet<NodeId>,
    search: Entity<InputState>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    scanning: bool,
    status: SharedString,
    _scan: Option<Task<()>>,
    _debounce: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<KeyEvent> for KeyBrowser {}

impl Focusable for KeyBrowser {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl KeyBrowser {
    pub fn new(client: Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter keys, e.g. user:* or session")
        });
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => {
                        // Debounce typing; replacing the task cancels the previous wait.
                        this._debounce = Some(cx.spawn_in(window, async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(250))
                                .await;
                            this.update_in(cx, |this, window, cx| this.rescan(window, cx))
                                .ok();
                        }));
                    }
                    InputEvent::PressEnter { .. } => {
                        this._debounce = None;
                        this.rescan(window, cx);
                    }
                    _ => {}
                }
            },
        );
        let separator = client.config().separator_byte();
        let mut this = Self {
            client,
            tree: Arc::new(Mutex::new(KeyTree::new(Some(separator)))),
            rows: Vec::new(),
            rows_dirty: false,
            selected: None,
            meta: HashMap::new(),
            meta_pending: HashSet::new(),
            search,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            scanning: false,
            status: SharedString::default(),
            _scan: None,
            _debounce: None,
            _subscriptions: vec![subscription],
        };
        this.rescan(window, cx);
        this
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
    }

    fn tree(&self) -> MutexGuard<'_, KeyTree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Plain text matches anywhere in the key; text with glob characters is
    /// passed to SCAN MATCH as is.
    fn pattern(&self, cx: &Context<Self>) -> String {
        let query = self.search.read(cx).value().trim().to_string();
        if query.is_empty() {
            "*".into()
        } else if query.contains(['*', '?', '[']) {
            query
        } else {
            format!("*{query}*")
        }
    }

    pub fn rescan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pattern = self.pattern(cx);
        let separator = self.client.config().separator_byte();
        let filtered = pattern != "*";
        self.tree = Arc::new(Mutex::new(KeyTree::new(Some(separator))));
        self.rows.clear();
        self.rows_dirty = true;
        self.meta.clear();
        self.meta_pending.clear();
        self.scanning = true;
        self.status = "Scanning…".into();

        let mut events = self.client.scan_into(
            self.tree.clone(),
            ScanOptions {
                pattern,
                ..Default::default()
            },
        );
        self._scan = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                let finished = !matches!(event, ScanEvent::Progress { .. });
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        this.on_scan_event(event, filtered, window, cx)
                    })
                    .is_ok();
                if finished || !alive {
                    break;
                }
                // At most ~15 redraws a second while keys pour in; progress
                // events coalesce in the meantime.
                cx.background_executor()
                    .timer(Duration::from_millis(66))
                    .await;
            }
        }));
        cx.notify();
    }

    fn on_scan_event(
        &mut self,
        event: ScanEvent,
        filtered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rows_dirty = true;
        match event {
            ScanEvent::Progress { keys } => {
                self.status = format!("Scanning… {} keys", format::count(keys as u64)).into();
            }
            ScanEvent::Done { keys, elapsed } => {
                self.scanning = false;
                let noun = if filtered { "matches" } else { "keys" };
                self.status = format!(
                    "{} {noun} · {:.0} ms",
                    format::count(keys as u64),
                    elapsed.as_secs_f64() * 1000.0
                )
                .into();
                // With a filter, open everything so matches are visible.
                if filtered && keys <= 5_000 {
                    let mut tree = self.tree();
                    for row in tree.rows() {
                        if row.kind == RowKind::Folder {
                            tree.set_expanded(row.node, true);
                        }
                    }
                    // Nested folders appear only after their parent opens.
                    loop {
                        let closed: Vec<_> = tree
                            .rows()
                            .into_iter()
                            .filter(|r| r.kind == RowKind::Folder && !tree.is_expanded(r.node))
                            .collect();
                        if closed.is_empty() {
                            break;
                        }
                        for row in closed {
                            tree.set_expanded(row.node, true);
                        }
                    }
                }
            }
            ScanEvent::Failed(err) => {
                self.scanning = false;
                self.status = "Scan failed".into();
                window.push_notification(Notification::error(err).title("SCAN failed"), cx);
            }
        }
        cx.notify();
    }

    /// Drops a deleted key from the tree without rescanning.
    pub fn remove_key(&mut self, key: &[u8], cx: &mut Context<Self>) {
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
        cx.notify();
    }

    fn refresh_rows(&mut self) {
        if self.rows_dirty {
            let rows = self.tree().rows();
            self.rows = rows;
            self.rows_dirty = false;
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let (node, kind) = self.selected?;
        self.rows
            .iter()
            .position(|r| r.node == node && r.kind == kind)
    }

    fn activate(&mut self, ix: usize, toggle: bool, cx: &mut Context<Self>) {
        let Some(&row) = self.rows.get(ix) else {
            return;
        };
        self.selected = Some((row.node, row.kind));
        match row.kind {
            RowKind::Folder => {
                if toggle {
                    self.tree().toggle(row.node);
                    self.rows_dirty = true;
                }
            }
            RowKind::Key => {
                let key = self.tree().key(row.node);
                cx.emit(KeyEvent::Selected(key));
            }
        }
        cx.notify();
    }

    fn on_row_click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.activate(ix, true, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index().map_or(0, |ix| ix.saturating_sub(1));
        self.move_to(ix, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self
            .selected_index()
            .map_or(0, |ix| (ix + 1).min(self.rows.len().saturating_sub(1)));
        self.move_to(ix, cx);
    }

    fn move_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.rows.len() {
            self.activate(ix, false, cx);
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        self.set_selected_expanded(true, cx);
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else {
            return;
        };
        let row = self.rows[ix];
        let expanded_folder = row.kind == RowKind::Folder && self.tree().is_expanded(row.node);
        if expanded_folder {
            self.set_selected_expanded(false, cx);
        } else if let Some(parent) = self.rows[..ix]
            .iter()
            .rposition(|r| r.kind == RowKind::Folder && r.depth + 1 == row.depth)
        {
            // Like a file tree: left on a child jumps to its folder.
            self.move_to(parent, cx);
        }
    }

    fn set_selected_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if let Some((node, RowKind::Folder)) = self.selected {
            self.tree().set_expanded(node, expanded);
            self.rows_dirty = true;
            cx.notify();
        }
    }

    fn collapse_all(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.tree().collapse_all();
        self.rows_dirty = true;
        cx.notify();
    }

    /// Fetches type and TTL for visible keys that have none yet, in one
    /// pipelined request.
    fn request_meta(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let nodes: Vec<NodeId> = self.rows[range]
            .iter()
            .filter(|r| r.kind == RowKind::Key)
            .map(|r| r.node)
            .filter(|n| !self.meta.contains_key(n) && !self.meta_pending.contains(n))
            .collect();
        if nodes.is_empty() {
            return;
        }
        let keys: Vec<Key> = {
            let tree = self.tree();
            nodes.iter().map(|&n| tree.key(n)).collect()
        };
        self.meta_pending.extend(&nodes);
        let tree = Arc::downgrade(&self.tree);
        let request = self.client.key_meta(keys);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                // Ignore answers for a tree that has since been replaced.
                if !tree.upgrade().is_some_and(|t| Arc::ptr_eq(&t, &this.tree)) {
                    return;
                }
                for node in &nodes {
                    this.meta_pending.remove(node);
                }
                if let Ok(metas) = result {
                    this.meta.extend(nodes.into_iter().zip(metas));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<impl IntoElement + use<>> {
        self.request_meta(range.clone(), cx);
        let theme = cx.theme();
        let tree = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        range
            .map(|ix| {
                let row = self.rows[ix];
                let selected = self.selected == Some((row.node, row.kind));
                let name = format::bytes_inline(tree.name(row.node)).into_owned();
                let base = h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .pl(px(8. + row.depth as f32 * INDENT))
                    .pr_2()
                    .gap_1p5()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(theme.list_active))
                    .when(!selected, |this| {
                        this.hover(|this| this.bg(theme.list_hover))
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.on_row_click(ix, window, cx)),
                    );
                match row.kind {
                    RowKind::Folder => {
                        let expanded = tree.is_expanded(row.node);
                        base.child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall()
                            .text_color(theme.muted_foreground),
                        )
                        .child(
                            Icon::new(if expanded {
                                IconName::FolderOpen
                            } else {
                                IconName::Folder
                            })
                            .small()
                            .text_color(theme.muted_foreground),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format::count(tree.key_count(row.node) as u64)),
                        )
                    }
                    RowKind::Key => {
                        let meta = self.meta.get(&row.node);
                        base.child(div().w(px(12.)).flex_none())
                            .child(match meta {
                                Some(meta) => style::type_badge(&meta.ty, cx).into_any_element(),
                                None => div().w(px(44.)).flex_none().into_any_element(),
                            })
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            .when_some(meta.and_then(|m| format::ttl(m.ttl_ms)), |this, ttl| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(ttl),
                                )
                            })
                    }
                }
            })
            .collect()
    }
}

impl Render for KeyBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_rows();
        let theme = cx.theme();
        let count = self.rows.len();

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .gap_1()
                    .p_2()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.search).small().cleanable(true).prefix(
                                Icon::new(IconName::Search)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            ),
                        ),
                    )
                    .child(
                        Button::new("collapse-all")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronsUpDown)
                            .tooltip("Collapse all")
                            .on_click(cx.listener(Self::collapse_all)),
                    )
                    .child(
                        Button::new("rescan")
                            .ghost()
                            .small()
                            .icon(IconName::RefreshCw)
                            .loading(self.scanning)
                            .tooltip("Rescan (⌘R)")
                            .on_click(cx.listener(|this, _, window, cx| this.rescan(window, cx))),
                    ),
            )
            .child(
                div()
                    .id("key-list")
                    .key_context("KeyList")
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::select_prev))
                    .on_action(cx.listener(Self::select_next))
                    .on_action(cx.listener(Self::expand))
                    .on_action(cx.listener(Self::collapse))
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .when(count == 0 && !self.scanning, |this| {
                        this.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("No keys"),
                        )
                    })
                    .child(
                        uniform_list(
                            "keys",
                            count,
                            cx.processor(|this, range: Range<usize>, _, cx| {
                                this.render_rows(range, cx)
                            }),
                        )
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
            .child(
                h_flex()
                    .h(px(28.))
                    .px_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(self.scanning, |this| this.child(Spinner::new().xsmall()))
                    .child(self.status.clone()),
            )
    }
}
