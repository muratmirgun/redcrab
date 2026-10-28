use std::{ops::Range, time::Duration};

use futures::StreamExt as _;
use gpui_kit::{
    AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariant, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
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
    Client, DbInfo, Key, RowKind, ScanEvent,
    browser::{self, RowId, Selection},
    bulk::{Refresh, Target},
    format,
};

use crate::{
    Collapse, Expand, SelectNext, SelectPrev,
    bulk::{BulkDeleteView, BulkEvent},
    style,
};

const ROW_HEIGHT: f32 = 26.;
const INDENT: f32 = 14.;
/// Databases the switcher always lists; past these, only those with keys
/// and the current one (a server configured with thousands would make an
/// endless menu).
const LISTED_DATABASES: u32 = 64;

pub enum KeyEvent {
    Selected(Key),
    /// A key was deleted from the list.
    Deleted(Key),
    /// Every key in the folder `prefix` was deleted (not `prefix` itself).
    FolderDeleted(Key),
    /// Keys were deleted by pattern; which ones is not known here.
    KeysDeleted,
    /// The user picked another database of the server.
    SwitchDb(u32),
}

/// A row action of the context menu.
type RowAction = fn(&mut KeyBrowser, RowId, &mut Window, &mut Context<KeyBrowser>);

/// The searchable key tree. Keys stream in from SCAN on the I/O threads; the
/// list logic lives in [`browser::KeyBrowser`], and this view draws its
/// visible rows and runs the requests it hands out.
pub struct KeyBrowser {
    client: Client,
    model: browser::KeyBrowser,
    search: Entity<InputState>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// The server's databases, for the switcher; empty until known.
    databases: Vec<DbInfo>,
    /// A database the user picked, until the workspace has switched to it.
    switching: Option<u32>,
    /// Bulk deletes whose dialog is open, or that were closed while
    /// stopping and have yet to say how far they got.
    bulk: Vec<(Entity<BulkDeleteView>, Subscription)>,
    _scan: Option<Task<()>>,
    _debounce: Option<Task<()>>,
    _databases: Option<Task<()>>,
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
        let model = Self::model(&client);
        let mut this = Self {
            client,
            model,
            search,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            databases: Vec::new(),
            switching: None,
            bulk: Vec::new(),
            _scan: None,
            _debounce: None,
            _databases: None,
            _subscriptions: vec![subscription],
        };
        this.rescan(window, cx);
        this
    }

    fn model(client: &Client) -> browser::KeyBrowser {
        browser::KeyBrowser::new(
            client.config().separator_byte(),
            Some(browser::DEFAULT_KEY_LIMIT),
        )
    }

    fn read_only(&self) -> bool {
        self.client.config().read_only
    }

    /// Starts over on another connection (another database of the server):
    /// a new tree, scanned from scratch. The filter stays. The databases
    /// are refreshed by the workspace's poll, which starts over too.
    pub fn set_client(&mut self, client: Client, window: &mut Window, cx: &mut Context<Self>) {
        // Reset rather than replaced: type/TTL answers still on their way
        // from the old database, and rows of a menu left open, must not
        // match the new tree's rows.
        self.model.reset(client.config().separator_byte());
        self.client = client;
        self.switching = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.rescan(window, cx);
    }

    /// The workspace could not switch to the database the user picked.
    pub fn switch_failed(&mut self, cx: &mut Context<Self>) {
        self.switching = None;
        cx.notify();
    }

    /// Fetches the databases and their sizes for the switcher. The
    /// workspace calls this as it polls the server's stats; deletes call it
    /// to show the new sizes right away.
    pub fn refresh_databases(&mut self, cx: &mut Context<Self>) {
        // A cluster has database 0 only: no switcher to fill.
        if !self.client.topology().supports_databases() {
            return;
        }
        // A user the server does not tell the sizes (no INFO) will not be
        // told later either; asking again only adds to the ACL log.
        if !self.databases.is_empty() && self.databases.iter().all(|db| db.keys.is_none()) {
            return;
        }
        let request = self.client.databases();
        self._databases = Some(cx.spawn(async move |this, cx| {
            // Without an answer the switcher stays as it was.
            let Ok(databases) = request.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.databases != databases {
                    this.databases = databases;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn pick_db(&mut self, db: u32, cx: &mut Context<Self>) {
        if db == self.client.config().db || self.switching.is_some() {
            return;
        }
        self.switching = Some(db);
        cx.emit(KeyEvent::SwitchDb(db));
        cx.notify();
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
    }

    pub fn rescan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_string();
        let request = self.model.begin_scan(&query);
        let generation = request.generation;
        let mut events = self.client.scan_into(request.tree, request.options);
        // Replacing the task drops the previous scan's receiver, which stops it.
        self._scan = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                let finished = !matches!(event, ScanEvent::Progress { .. });
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        this.on_scan_event(generation, event, window, cx)
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
        generation: u64,
        event: ScanEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let failure = match &event {
            ScanEvent::Failed(err) => Some(err.clone()),
            _ => None,
        };
        if !self.model.on_scan_event(generation, event) {
            return;
        }
        if let Some(err) = failure {
            window.push_notification(Notification::error(err).title("SCAN failed"), cx);
        }
        cx.notify();
    }

    /// Drops a deleted key from the tree without rescanning.
    pub fn remove_key(&mut self, key: &[u8], cx: &mut Context<Self>) {
        self.model.remove_key(key);
        self.refresh_databases(cx);
        cx.notify();
    }

    /// Whether deletes are offered: not on a read-only connection, and not
    /// while the list is about to move to another database (a delete
    /// started now would act on the one the user is leaving).
    fn can_delete(&self) -> bool {
        !self.read_only() && self.switching.is_none()
    }

    /// Opens the bulk-delete dialog for `target`.
    fn open_bulk(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_delete() {
            return;
        }
        // Those that reported have nothing more to say.
        self.bulk.retain(|(view, _)| !view.read(cx).is_done());
        let view = BulkDeleteView::open(self.client.clone(), target, window, cx);
        let subscription = cx.subscribe_in(&view, window, Self::on_bulk_event);
        self.bulk.push((view, subscription));
    }

    /// Catches the list up with a finished bulk delete, if the list still
    /// shows the database it ran on: the delete keeps to the database its
    /// dialog was opened on, and the list may have moved since.
    fn on_bulk_event(
        &mut self,
        view: &Entity<BulkDeleteView>,
        event: &BulkEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let BulkEvent::Finished(refresh) = event;
        let here = view.read(cx).client().same_connection(&self.client);
        match refresh.as_ref().filter(|_| here) {
            // A scan still running could put deleted keys back.
            Some(Refresh::RemoveFolder(prefix)) if !self.model.is_scanning() => {
                self.model.remove_folder(prefix);
                cx.emit(KeyEvent::FolderDeleted(prefix.clone()));
            }
            Some(Refresh::RemoveFolder(prefix)) => {
                self.rescan(window, cx);
                cx.emit(KeyEvent::FolderDeleted(prefix.clone()));
            }
            Some(Refresh::Rescan) => {
                self.rescan(window, cx);
                cx.emit(KeyEvent::KeysDeleted);
            }
            None => {}
        }
        self.refresh_databases(cx);
        cx.notify();
    }

    /// "Delete matching…": every key the filter matches.
    fn delete_matching(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_string();
        if query.trim().is_empty() {
            return;
        }
        let pattern = browser::pattern_for(&query).into_bytes();
        self.open_bulk(Target::Pattern(pattern), window, cx);
    }

    // The row context menu. Each action acts on the row as it was drawn,
    // and does nothing if that row has gone since.

    /// Copies a key's name, or a folder's prefix with its separator (what
    /// its keys start with), quoted when it is not plain text.
    fn copy_row(&mut self, id: RowId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.model.row_key(id) else {
            return;
        };
        let mut text = key.to_vec();
        let what = match id.kind() {
            RowKind::Folder => {
                text.push(self.model.separator());
                "Prefix copied"
            }
            RowKind::Key => "Key name copied",
        };
        let text = format::copyable(&text).into_owned();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification(what, cx);
    }

    /// Asks before deleting a key, naming the database it is in. The key
    /// is deleted there, on the connection the row was drawn from, even if
    /// the list moves to another database while the question is open.
    fn confirm_delete_row(&mut self, id: RowId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_delete() {
            return;
        }
        let Some(key) = self.model.row_key(id) else {
            return;
        };
        let client = self.client.clone();
        let name = format::bytes_inline(&key).into_owned();
        let config = client.config();
        let location = format!("{} · db{}", config.name, config.db);
        let view = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let view = view.clone();
            let key = key.clone();
            let client = client.clone();
            alert
                .confirm()
                .title("Delete key?")
                .description(
                    v_flex().gap_1().child(name.clone()).child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(location.clone()),
                    ),
                )
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let (client, key) = (client.clone(), key.clone());
                    view.update(cx, |this, cx| this.delete_key(client, key, window, cx))
                        .ok();
                    true
                })
        });
    }

    /// Deletes `key` on `client`, and drops it from the list if the list
    /// still shows that database.
    fn delete_key(
        &mut self,
        client: Client,
        key: Key,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = client.delete(vec![key.clone()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(_) if client.same_connection(&this.client) => {
                    let note = format!("Deleted {}", format::bytes_inline(&key));
                    window.push_notification(Notification::success(note), cx);
                    this.remove_key(&key, cx);
                    cx.emit(KeyEvent::Deleted(key));
                }
                // The list moved to another database meanwhile; a key of
                // the same name there stays.
                Ok(_) => {
                    let note = format!(
                        "Deleted {} from db{}",
                        format::bytes_inline(&key),
                        client.config().db
                    );
                    window.push_notification(Notification::success(note), cx);
                }
                Err(err) => window.push_notification(
                    Notification::error(format!("{err:#}")).title("Delete failed"),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn delete_folder_row(&mut self, id: RowId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prefix) = self.model.row_key(id) {
            let separator = self.model.separator();
            self.open_bulk(Target::Folder { prefix, separator }, window, cx);
        }
    }

    fn expand_row(&mut self, id: RowId, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.set_expanded_below(id, true) {
            cx.notify();
        }
    }

    fn collapse_row(&mut self, id: RowId, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.set_expanded_below(id, false) {
            cx.notify();
        }
    }

    /// The context menu of a row: copy, delete (greyed out unless
    /// `can_delete`), and for folders, open or close everything below.
    fn row_menu(
        view: WeakEntity<Self>,
        id: RowId,
        can_delete: bool,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        move |menu, _, _| {
            let item = |label: &'static str, icon: Icon, action: RowAction| {
                let view = view.clone();
                PopupMenuItem::new(label)
                    .icon(icon)
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| action(this, id, window, cx))
                            .ok();
                    })
            };
            let trash = || Icon::new(gpui_kit::assets::IconName::Trash);
            match id.kind() {
                RowKind::Key => menu
                    .item(item("Copy name", Icon::new(IconName::Copy), Self::copy_row))
                    .separator()
                    .item(item("Delete…", trash(), Self::confirm_delete_row).disabled(!can_delete)),
                RowKind::Folder => menu
                    .item(item(
                        "Copy prefix",
                        Icon::new(IconName::Copy),
                        Self::copy_row,
                    ))
                    .item(
                        item("Delete folder…", trash(), Self::delete_folder_row)
                            .disabled(!can_delete),
                    )
                    .separator()
                    .item(item(
                        "Expand all below",
                        Icon::new(IconName::FolderOpen),
                        Self::expand_row,
                    ))
                    .item(item(
                        "Collapse",
                        Icon::new(IconName::FolderClosed),
                        Self::collapse_row,
                    )),
            }
        }
    }

    /// A click acts on the row as it was drawn: keys streaming in can move
    /// rows before the frame the user clicked in is replaced.
    fn on_row_click(&mut self, row: RowId, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if let Some(key) = self.model.click(row) {
            cx.emit(KeyEvent::Selected(key));
        }
        cx.notify();
    }

    /// Scrolls to where a keyboard move landed and shows the key it selected.
    fn moved(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        if let Some(Selection { ix, key }) = selection {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            if let Some(key) = key {
                cx.emit(KeyEvent::Selected(key));
            }
        }
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.model.select_prev();
        self.moved(selection, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.model.select_next();
        self.moved(selection, cx);
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.expand() {
            cx.notify();
        }
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.model.collapse();
        self.moved(selection, cx);
    }

    fn collapse_all(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.model.collapse_all();
        cx.notify();
    }

    /// Fetches type and TTL for the visible keys that have none yet, in one
    /// pipelined request.
    fn request_meta(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let Some(request) = self.model.take_meta_request(range) else {
            return;
        };
        let fetch = self.client.key_meta_permitted(request.keys.clone());
        cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                if this.model.apply_meta(request, result) {
                    cx.notify();
                }
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
        let rows = self.model.row_views(range.clone());
        let view = cx.weak_entity();
        let can_delete = self.can_delete();
        let theme = cx.theme();
        range
            .zip(rows)
            .map(|(ix, row)| {
                let id = row.id;
                let menu = Self::row_menu(view.clone(), id, can_delete);
                let base = h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .pl(px(8. + row.depth as f32 * INDENT))
                    .pr_2()
                    .gap_1p5()
                    .text_sm()
                    .cursor_pointer()
                    .when(row.selected, |this| this.bg(theme.list_active))
                    .when(!row.selected, |this| {
                        this.hover(|this| this.bg(theme.list_hover))
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.on_row_click(id, window, cx)),
                    );
                match row.kind {
                    RowKind::Folder => base
                        .child(
                            Icon::new(if row.expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall()
                            .text_color(theme.muted_foreground),
                        )
                        .child(
                            Icon::new(if row.expanded {
                                IconName::FolderOpen
                            } else {
                                IconName::Folder
                            })
                            .small()
                            .text_color(theme.muted_foreground),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(row.name))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format::count(row.key_count as u64)),
                        ),
                    RowKind::Key => {
                        let ttl = row.meta.as_ref().and_then(|m| format::ttl(m.ttl_ms));
                        base.child(div().w(px(12.)).flex_none())
                            .child(match &row.meta {
                                Some(meta) => style::type_badge(&meta.ty, cx).into_any_element(),
                                None => div().w(px(44.)).flex_none().into_any_element(),
                            })
                            .child(div().flex_1().min_w_0().truncate().child(row.name))
                            .when_some(ttl, |this, ttl| {
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
                .context_menu(menu)
            })
            .collect()
    }

    /// The bar under the filter: the database switcher, and "Delete
    /// matching…" while a filter is set. `None` when it would be empty.
    fn render_scope(&self, cx: &Context<Self>) -> Option<impl IntoElement + use<>> {
        let current = self.client.config().db;
        // A cluster (or a server configured with one database) has nothing
        // to switch to.
        let switcher = self.databases.len() > 1;
        let filtered = !self.search.read(cx).value().trim().is_empty();
        let delete = filtered && !self.read_only();
        if !switcher && !delete {
            return None;
        }
        let label = match self.switching {
            Some(db) => format!("db{db}"),
            None => self
                .databases
                .iter()
                .find(|info| info.index == current)
                .map_or_else(|| format!("db{current}"), DbInfo::label),
        };
        let listed: Vec<DbInfo> = self
            .databases
            .iter()
            .filter(|info| {
                info.index < LISTED_DATABASES
                    || info.index == current
                    || info.keys.is_some_and(|keys| keys > 0)
            })
            .cloned()
            .collect();
        let view = cx.weak_entity();
        Some(
            h_flex()
                .gap_1()
                .px_2()
                .pb_1p5()
                .when(switcher, |this| {
                    this.child(
                        Button::new("database")
                            .ghost()
                            .xsmall()
                            .icon(gpui_kit::assets::IconName::Database)
                            .label(label)
                            .dropdown_caret(true)
                            .loading(self.switching.is_some())
                            .tooltip("Switch database")
                            .dropdown_menu(move |menu, _, _| {
                                let menu = menu.scrollable(true).max_h(px(360.));
                                listed.iter().fold(menu, |menu, info| {
                                    let view = view.clone();
                                    let db = info.index;
                                    menu.item(
                                        PopupMenuItem::new(info.label())
                                            .checked(db == current)
                                            .on_click(move |_, _, cx| {
                                                view.update(cx, |this, cx| this.pick_db(db, cx))
                                                    .ok();
                                            }),
                                    )
                                })
                            }),
                    )
                })
                .child(div().flex_1())
                .when(delete, |this| {
                    this.child(
                        Button::new("delete-matching")
                            .ghost()
                            .xsmall()
                            .icon(gpui_kit::assets::IconName::Trash)
                            .label("Delete matching…")
                            .tooltip("Delete every key the filter matches")
                            .disabled(!self.can_delete())
                            .on_click(cx.listener(Self::delete_matching)),
                    )
                }),
        )
    }
}

impl Render for KeyBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.model.len();
        let scanning = self.model.is_scanning();
        let status = self.model.status_text();
        let notice = self.model.truncation_notice();
        let scope = self.render_scope(cx);
        let theme = cx.theme();

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
                            .loading(scanning)
                            .tooltip("Rescan (⌘R)")
                            .on_click(cx.listener(|this, _, window, cx| this.rescan(window, cx))),
                    ),
            )
            .children(scope)
            .when_some(notice, |this, notice| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .mx_2()
                        .mb_2()
                        .px_2()
                        .py_1p5()
                        .rounded(theme.radius)
                        .bg(theme.warning.opacity(0.12))
                        .text_xs()
                        .text_color(theme.warning)
                        .child(Icon::new(IconName::TriangleAlert).xsmall().flex_none())
                        .child(div().flex_1().min_w_0().child(notice)),
                )
            })
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
                    .when(count == 0 && !scanning, |this| {
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
                    .when(scanning, |this| this.child(Spinner::new().xsmall()))
                    .child(status),
            )
    }
}
