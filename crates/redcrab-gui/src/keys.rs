use std::{ops::Range, time::Duration};

use futures::StreamExt as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy,
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
    Client, Key, RowKind, ScanEvent,
    browser::{self, RowId, Selection},
    format,
};

use crate::{Collapse, Expand, SelectNext, SelectPrev, style};

const ROW_HEIGHT: f32 = 26.;
const INDENT: f32 = 14.;

pub enum KeyEvent {
    Selected(Key),
}

/// The searchable key tree. Keys stream in from SCAN on the I/O threads; the
/// list logic lives in [`browser::KeyBrowser`], and this view draws its
/// visible rows and runs the requests it hands out.
pub struct KeyBrowser {
    client: Client,
    model: browser::KeyBrowser,
    search: Entity<InputState>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
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
        let model = browser::KeyBrowser::new(
            client.config().separator_byte(),
            Some(browser::DEFAULT_KEY_LIMIT),
        );
        let mut this = Self {
            client,
            model,
            search,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
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
        cx.notify();
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
        let theme = cx.theme();
        range
            .zip(rows)
            .map(|(ix, row)| {
                let id = row.id;
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
            })
            .collect()
    }
}

impl Render for KeyBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.model.len();
        let scanning = self.model.is_scanning();
        let status = self.model.status_text();
        let notice = self.model.truncation_notice();
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
