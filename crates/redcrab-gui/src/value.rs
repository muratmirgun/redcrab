use std::{ops::Range, time::Duration};

use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    Window,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Editor, EditorState, InputEvent},
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
    Client, Key, ValueData, format,
    viewer::{self, Loaded, PAGE, TextValue},
};

use crate::style;

const ROW_HEIGHT: f32 = 26.;
/// Width of a narrow first column (list index, score, stream ID).
const NARROW_COLUMN: f32 = 150.;

pub enum ValueEvent {
    Deleted(Key),
}

/// What the text editor currently shows.
struct TextView {
    editor: Entity<EditorState>,
    /// The editing rules; its text was handed to the editor.
    value: TextValue,
    dirty: bool,
    _subscription: Subscription,
}

/// The selected key's value. Loading, paging and the text rules live in
/// [`viewer`]; this view draws them and runs the requests.
pub struct ValuePanel {
    client: Client,
    key: Option<Key>,
    loaded: Option<Loaded>,
    loading: bool,
    loading_more: bool,
    error: Option<SharedString>,
    selected_row: Option<usize>,
    text: Option<TextView>,
    confirm_delete: bool,
    scroll: UniformListScrollHandle,
    _load: Option<Task<()>>,
    _more: Option<Task<()>>,
    _confirm_reset: Option<Task<()>>,
}

impl EventEmitter<ValueEvent> for ValuePanel {}

impl ValuePanel {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            key: None,
            loaded: None,
            loading: false,
            loading_more: false,
            error: None,
            selected_row: None,
            text: None,
            confirm_delete: false,
            scroll: UniformListScrollHandle::new(),
            _load: None,
            _more: None,
            _confirm_reset: None,
        }
    }

    /// A read-only connection shows values but never offers to change them.
    fn read_only(&self) -> bool {
        self.client.config().read_only
    }

    pub fn load(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        self.key = Some(key.clone());
        self.loaded = None;
        self.text = None;
        self.selected_row = None;
        self.error = None;
        self.confirm_delete = false;
        self.loading = true;
        self.loading_more = false;
        self._more = None;
        cx.notify();

        let request = viewer::load(&self.client, key);
        // Replacing the task drops the previous load, so fast arrowing through
        // keys never shows a stale value.
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok(loaded) => this.show(loaded, window, cx),
                    Err(err) => this.error = Some(format!("{err:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(key) = self.key.clone() {
            self.load(key, window, cx);
        }
    }

    /// Moves to another connection (another database of the server); what
    /// the panel showed belonged to the old one.
    pub fn set_client(&mut self, client: Client, cx: &mut Context<Self>) {
        self.client = client;
        self.clear(cx);
    }

    /// Clears the panel if it shows `key`, which was deleted elsewhere.
    pub fn key_deleted(&mut self, key: &[u8], cx: &mut Context<Self>) {
        if self.key.as_deref() == Some(key) {
            self.clear(cx);
        }
    }

    /// Clears the panel if it shows a key of the folder `prefix`, whose keys
    /// were all deleted.
    pub fn folder_deleted(&mut self, prefix: &[u8], separator: u8, cx: &mut Context<Self>) {
        let inside = self.key.as_ref().is_some_and(|key| {
            key.strip_prefix(prefix)
                .is_some_and(|rest| rest.first() == Some(&separator))
        });
        if inside {
            self.clear(cx);
        }
    }

    /// Shows no key, dropping any load still on its way.
    fn clear(&mut self, cx: &mut Context<Self>) {
        self.key = None;
        self.loaded = None;
        self.text = None;
        self.selected_row = None;
        self.error = None;
        self.confirm_delete = false;
        self.loading = false;
        self.loading_more = false;
        self._load = None;
        self._more = None;
        cx.notify();
    }

    fn show(&mut self, loaded: Loaded, window: &mut Window, cx: &mut Context<Self>) {
        self.text = loaded
            .text(!self.read_only())
            .map(|value| self.text_view(value, window, cx));
        self.loaded = Some(loaded);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn text_view(
        &self,
        mut value: TextValue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TextView {
        let text = std::mem::take(&mut value.text);
        let is_json = value.is_json;
        let editor = cx.new(|cx| {
            let state = EditorState::new(window, cx)
                .line_number(true)
                .soft_wrap(true)
                .default_value(text);
            if is_json {
                state.language("json")
            } else {
                state
            }
        });
        let subscription = cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if let (InputEvent::Change, Some(text)) = (event, this.text.as_mut())
                && text.value.editable
                && !text.dirty
            {
                text.dirty = true;
                cx.notify();
            }
        });
        TextView {
            editor,
            value,
            dirty: false,
            _subscription: subscription,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(key), Some(text)) = (self.key.clone(), self.text.as_mut()) else {
            return;
        };
        if !text.value.editable || !text.dirty {
            return;
        }
        let edited = text.editor.read(cx).value();
        let bytes = text.value.bytes_to_save(&edited);
        text.dirty = false;
        let request = self.client.set_string(key, bytes);
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    window.push_notification(Notification::success("Value saved"), cx);
                    this.reload(window, cx);
                }
                Err(err) => {
                    if let Some(text) = this.text.as_mut() {
                        text.dirty = true;
                    }
                    window.push_notification(
                        Notification::error(format!("{err:#}")).title("Save failed"),
                        cx,
                    );
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading_more {
            return;
        }
        let Some(request) = self
            .loaded
            .as_ref()
            .and_then(|loaded| loaded.load_more(&self.client))
        else {
            return;
        };
        self.loading_more = true;
        cx.notify();
        // Kept in a field: a new load drops it, so a page never lands on
        // another key (and `apply_page` refuses one that does not fit).
        self._more = Some(cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.loading_more = false;
                match (result, this.loaded.as_mut()) {
                    (Ok(page), Some(loaded)) => {
                        loaded.apply_page(page);
                    }
                    (Err(err), _) => {
                        window.push_notification(Notification::error(format!("{err:#}")), cx)
                    }
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// First call arms the button; a second within three seconds deletes.
    pub fn request_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only() {
            return;
        }
        let Some(key) = self.key.clone() else {
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            self._confirm_reset = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                this.update(cx, |this, cx| {
                    this.confirm_delete = false;
                    cx.notify();
                })
                .ok();
            }));
            cx.notify();
            return;
        }
        self.confirm_delete = false;
        let client = self.client.clone();
        let request = client.delete(vec![key.clone()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        window.push_notification(
                            Notification::success(format!(
                                "Deleted {}",
                                format::bytes_inline(&key)
                            )),
                            cx,
                        );
                        // The panel may have moved to another database
                        // meanwhile, where a key of that name stays.
                        if !client.same_connection(&this.client) {
                            return;
                        }
                        // Only if the deleted key is still the one shown.
                        if this.key.as_ref() == Some(&key) {
                            this.key = None;
                            this.loaded = None;
                            this.text = None;
                        }
                        cx.emit(ValueEvent::Deleted(key));
                    }
                    Err(err) => window.push_notification(
                        Notification::error(format!("{err:#}")).title("Delete failed"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_row = Some(ix);
        let value = self.loaded.as_ref().and_then(|l| l.row_text(ix));
        self.text = value.map(|value| self.text_view(value, window, cx));
        cx.notify();
    }

    fn render_header(&self, loaded: Option<&Loaded>, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let key = self
            .key
            .as_ref()
            .map(|k| format::bytes_inline(k).into_owned())
            .unwrap_or_default();
        // A binary name is copied quoted, so it pastes into the console as
        // the same bytes.
        let copied = self
            .key
            .as_ref()
            .map(|k| format::copyable(k).into_owned())
            .unwrap_or_default();
        let summary = loaded.and_then(Loaded::summary);
        let ttl = loaded.map(Loaded::ttl_text);
        let saveable = self
            .text
            .as_ref()
            .is_some_and(|t| t.value.editable && t.dirty);

        h_flex()
            .gap_2()
            .px_3()
            .h(px(44.))
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .when_some(loaded, |this, l| {
                this.child(style::type_badge(&l.meta.ty, cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(key.clone()),
            )
            .child(
                Button::new("copy-key")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Copy)
                    .tooltip("Copy key name")
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        window.push_notification("Key name copied", cx);
                    }),
            )
            .when_some(ttl, |this, ttl| {
                this.child(
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(gpui_kit::assets::IconName::Clock).xsmall())
                        .child(ttl),
                )
            })
            .when_some(summary, |this, summary| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(summary),
                )
            })
            .when_some(loaded.and_then(|l| l.memory), |this, memory| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("mem {}", format::size(memory))),
                )
            })
            .when(saveable, |this| {
                this.child(
                    Button::new("save")
                        .primary()
                        .small()
                        .label("Save")
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                )
            })
            .child(
                Button::new("reload")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip("Reload value")
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
            .when(!self.read_only(), |this| {
                this.child(
                    Button::new("delete")
                        .small()
                        .map(|b| {
                            if self.confirm_delete {
                                b.danger().label("Delete?")
                            } else {
                                b.ghost()
                            }
                        })
                        .icon(gpui_kit::assets::IconName::Trash)
                        .tooltip("Delete key (⌘⌫)")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.request_delete(window, cx)),
                        ),
                )
            })
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<impl IntoElement + use<>> {
        let theme = cx.theme();
        let Some(loaded) = self.loaded.as_ref() else {
            return Vec::new();
        };
        let share = loaded.columns().first_share;
        let mono = theme.mono_font_family.clone();
        range
            .map(|ix| {
                let (first, second) = loaded.row_cells(ix);
                let selected = self.selected_row == Some(ix);
                h_flex()
                    .id(ix)
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .gap_3()
                    .text_sm()
                    .font_family(mono.clone())
                    .border_b_1()
                    .border_color(theme.table_row_border)
                    .cursor_pointer()
                    .when(selected, |this| this.bg(theme.table_active))
                    .when(!selected, |this| {
                        this.hover(|this| this.bg(theme.table_hover))
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select_row(ix, window, cx)),
                    )
                    .when_some(first, |this, first| {
                        this.child(
                            div()
                                .map(|d| match share {
                                    Some(share) => d.w(gpui_kit::relative(share)),
                                    None => d.w(px(NARROW_COLUMN)),
                                })
                                .flex_none()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(first),
                        )
                    })
                    .child(div().flex_1().min_w_0().truncate().child(second))
            })
            .collect()
    }

    fn render_collection(&self, loaded: &Loaded, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let columns = loaded.columns();
        let count = loaded.row_count();
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .h(px(28.))
                    .px_3()
                    .gap_3()
                    .flex_none()
                    .bg(theme.table_head)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .when_some(columns.first, |this, first| {
                        this.child(
                            div()
                                .map(|d| match columns.first_share {
                                    Some(share) => d.w(gpui_kit::relative(share)),
                                    None => d.w(px(NARROW_COLUMN)),
                                })
                                .flex_none()
                                .child(first),
                        )
                    })
                    .child(div().flex_1().child(columns.second)),
            )
            .child(
                div()
                    .id("value-rows")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "value-rows",
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
                    .h(px(32.))
                    .px_3()
                    .gap_2()
                    .flex_none()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(loaded.footer())
                    .when(loaded.has_more(), |this| {
                        this.child(
                            Button::new("more")
                                .outline()
                                .xsmall()
                                .label(format!("Load {PAGE} more"))
                                .loading(self.loading_more)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.load_more(window, cx)),
                                ),
                        )
                    }),
            )
    }

    fn render_text(&self, loaded: &Loaded, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .min_h_0()
            .when_some(loaded.preview_notice(), |this, notice| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .bg(theme.warning.opacity(0.12))
                        .text_color(theme.warning)
                        .child(notice),
                )
            })
            .when_some(self.text.as_ref(), |this, text| {
                this.child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&text.editor)
                            .readonly(!text.value.editable)
                            .bordered(false)
                            .h_full(),
                    ),
                )
            })
    }
}

impl Render for ValuePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        if self.key.is_none() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(Icon::new(gpui_kit::assets::IconName::Key).large())
                .child(div().text_sm().child("Select a key to see its value"))
                .into_any_element();
        }

        let loaded = self.loaded.as_ref();
        let body = if self.loading {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element()
        } else if let Some(error) = self.error.clone() {
            div()
                .m_4()
                .p_3()
                .rounded(theme.radius)
                .bg(theme.danger.opacity(0.12))
                .text_color(theme.danger)
                .text_sm()
                .child(error)
                .into_any_element()
        } else if let Some(loaded) = loaded {
            match &loaded.data {
                ValueData::String(_) | ValueData::Json(_) => {
                    self.render_text(loaded, cx).into_any_element()
                }
                ValueData::Missing => div()
                    .p_4()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("This key no longer exists.")
                    .into_any_element(),
                ValueData::Unsupported(name) => div()
                    .p_4()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!("Values of type {name} cannot be shown yet."))
                    .into_any_element(),
                _ => v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_collection(loaded, cx))
                    .when_some(self.text.as_ref(), |this, text| {
                        this.child(
                            div()
                                .h(px(220.))
                                .flex_none()
                                .border_t_1()
                                .border_color(theme.border)
                                .child(
                                    Editor::new(&text.editor)
                                        .readonly(true)
                                        .bordered(false)
                                        .h_full(),
                                ),
                        )
                    })
                    .into_any_element(),
            }
        } else {
            div().into_any_element()
        };

        v_flex()
            .size_full()
            .child(self.render_header(loaded, cx))
            .child(body)
            .into_any_element()
    }
}
