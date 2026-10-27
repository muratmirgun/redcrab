use std::{ops::Range, time::Duration};

use anyhow::Context as _;
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
use redcrab_core::{Client, Cursor, Key, KeyMeta, KeyType, ValueData, format};

use crate::style;

/// Elements fetched per page of a collection.
const PAGE: usize = 500;
/// Bytes of a cell shown in the table; the full value is in the detail pane.
const CELL_PREVIEW: usize = 256;
const ROW_HEIGHT: f32 = 26.;

pub enum ValueEvent {
    Deleted(Key),
}

struct Loaded {
    meta: KeyMeta,
    data: ValueData,
    total: u64,
    next: Option<Cursor>,
    memory: Option<u64>,
}

/// What the text editor currently shows.
struct TextView {
    editor: Entity<EditorState>,
    /// Pretty-printed from compact JSON; saved back compact.
    reformatted_json: bool,
    editable: bool,
    dirty: bool,
    _subscription: Subscription,
}

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
            _confirm_reset: None,
        }
    }

    pub fn load(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        self.key = Some(key.clone());
        self.loaded = None;
        self.text = None;
        self.selected_row = None;
        self.error = None;
        self.confirm_delete = false;
        self.loading = true;
        cx.notify();

        let client = self.client.clone();
        // Replacing the task drops the previous load, so fast arrowing through
        // keys never shows a stale value.
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let meta = client
                    .key_meta(vec![key.clone()])
                    .await?
                    .pop()
                    .context("empty reply")?;
                let (page, memory) = futures::join!(
                    client.load(key.clone(), meta.ty.clone(), Cursor::Start, PAGE),
                    client.memory_usage(key.clone()),
                );
                let page = page?;
                anyhow::Ok(Loaded {
                    meta,
                    data: page.data,
                    total: page.total,
                    next: page.next,
                    memory: memory.ok().flatten(),
                })
            }
            .await;
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

    fn show(&mut self, loaded: Loaded, window: &mut Window, cx: &mut Context<Self>) {
        self.text = match &loaded.data {
            ValueData::String(bytes) => {
                let truncated = loaded.total > bytes.len() as u64;
                Some(self.text_view(bytes, !truncated, window, cx))
            }
            ValueData::Json(json) => Some(self.text_view(json.as_bytes(), false, window, cx)),
            _ => None,
        };
        self.loaded = Some(loaded);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// Builds an editor for `bytes`, pretty-printing JSON. Only lossless UTF-8
    /// text is ever editable.
    fn text_view(
        &self,
        bytes: &[u8],
        editable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TextView {
        let json = pretty_json(bytes);
        let lossless = matches!(format::bytes(bytes), std::borrow::Cow::Borrowed(_));
        let reformatted_json = json
            .as_ref()
            .is_some_and(|pretty| pretty.as_bytes() != bytes);
        let is_json = json.is_some();
        let text = json.unwrap_or_else(|| format::bytes(bytes).into_owned());
        let editable = editable && lossless;
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
                && text.editable
                && !text.dirty
            {
                text.dirty = true;
                cx.notify();
            }
        });
        TextView {
            editor,
            reformatted_json,
            editable,
            dirty: false,
            _subscription: subscription,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(key), Some(text)) = (self.key.clone(), self.text.as_mut()) else {
            return;
        };
        if !text.editable || !text.dirty {
            return;
        }
        let mut value = text.editor.read(cx).value().to_string();
        if text.reformatted_json
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&value)
        {
            value = parsed.to_string();
        }
        text.dirty = false;
        let request = self.client.set_string(key, value.into_bytes());
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
        let (Some(key), Some(loaded)) = (self.key.clone(), self.loaded.as_ref()) else {
            return;
        };
        let Some(cursor) = loaded.next.clone() else {
            return;
        };
        if self.loading_more {
            return;
        }
        self.loading_more = true;
        cx.notify();
        let request = self
            .client
            .load(key.clone(), loaded.meta.ty.clone(), cursor, PAGE);
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.loading_more = false;
                // The user may have moved on to another key meanwhile.
                if this.key.as_ref() != Some(&key) {
                    return;
                }
                match (result, this.loaded.as_mut()) {
                    (Ok(page), Some(loaded)) => {
                        loaded.data.extend(page.data);
                        loaded.next = page.next;
                        loaded.total = page.total;
                    }
                    (Err(err), _) => {
                        window.push_notification(Notification::error(format!("{err:#}")), cx)
                    }
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// First call arms the button; a second within three seconds deletes.
    pub fn request_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let request = self.client.delete(vec![key.clone()]);
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
                        this.key = None;
                        this.loaded = None;
                        this.text = None;
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
        let bytes = self.loaded.as_ref().and_then(|l| row_value(&l.data, ix));
        self.text = bytes.map(|b| self.text_view(&b, false, window, cx));
        cx.notify();
    }

    fn columns(ty: &KeyType) -> (&'static str, &'static str, f32) {
        match ty {
            KeyType::Hash => ("Field", "Value", 0.35),
            KeyType::List => ("#", "Value", 0.0),
            KeyType::Set => ("", "Member", 0.0),
            KeyType::ZSet => ("Score", "Member", 0.0),
            KeyType::Stream => ("ID", "Fields", 0.0),
            _ => ("", "", 0.0),
        }
    }

    fn render_header(&self, loaded: Option<&Loaded>, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let key = self
            .key
            .as_ref()
            .map(|k| format::bytes_inline(k).into_owned())
            .unwrap_or_default();
        let summary = loaded.and_then(|l| match l.meta.ty {
            KeyType::String | KeyType::Json => Some(format::size(l.total)),
            KeyType::Other(_) | KeyType::None => None,
            _ => Some(format!("{} items", format::count(l.total))),
        });
        let ttl = loaded.map(|l| format::ttl(l.meta.ttl_ms).unwrap_or_else(|| "No expiry".into()));
        let saveable = self.text.as_ref().is_some_and(|t| t.editable && t.dirty);

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
                        cx.write_to_clipboard(ClipboardItem::new_string(key.clone()));
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
            .child(
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
                    .on_click(cx.listener(|this, _, window, cx| this.request_delete(window, cx))),
            )
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
        let (_, _, first_ratio) = Self::columns(&loaded.meta.ty);
        let mono = theme.mono_font_family.clone();
        range
            .map(|ix| {
                let (first, second) = row_cells(&loaded.data, ix);
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
                                .when(first_ratio > 0.0, |d| d.w(gpui_kit::relative(first_ratio)))
                                .when(first_ratio == 0.0, |d| d.w(px(150.)))
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
        let (first, second, first_ratio) = Self::columns(&loaded.meta.ty);
        let count = loaded.data.len();
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
                    .when(!first.is_empty(), |this| {
                        this.child(
                            div()
                                .when(first_ratio > 0.0, |d| d.w(gpui_kit::relative(first_ratio)))
                                .when(first_ratio == 0.0, |d| d.w(px(150.)))
                                .flex_none()
                                .child(first),
                        )
                    })
                    .child(div().flex_1().child(second)),
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
                    .child(format!(
                        "Showing {} of {}",
                        format::count(count as u64),
                        format::count(loaded.total)
                    ))
                    .when(loaded.next.is_some(), |this| {
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

impl ValuePanel {
    fn render_text(&self, loaded: &Loaded, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let shown = match &loaded.data {
            ValueData::String(b) => b.len() as u64,
            _ => loaded.total,
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .when(shown < loaded.total, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .bg(theme.warning.opacity(0.12))
                        .text_color(theme.warning)
                        .child(format!(
                            "Showing the first {} of {}. Large values are read-only.",
                            format::size(shown),
                            format::size(loaded.total)
                        )),
                )
            })
            .when_some(self.text.as_ref(), |this, text| {
                this.child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&text.editor)
                            .readonly(!text.editable)
                            .bordered(false)
                            .h_full(),
                    ),
                )
            })
    }
}

fn pretty_json(bytes: &[u8]) -> Option<String> {
    let first = bytes.iter().find(|b| !b.is_ascii_whitespace())?;
    if !matches!(first, b'{' | b'[') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

fn cell(bytes: &[u8]) -> SharedString {
    let end = bytes.len().min(CELL_PREVIEW);
    format::bytes_inline(&bytes[..end]).into_owned().into()
}

/// The (optional first column, second column) text of row `ix`.
fn row_cells(data: &ValueData, ix: usize) -> (Option<SharedString>, SharedString) {
    match data {
        ValueData::Hash(pairs) => (Some(cell(&pairs[ix].0)), cell(&pairs[ix].1)),
        ValueData::List(items) => (Some(ix.to_string().into()), cell(&items[ix])),
        ValueData::Set(items) => (None, cell(&items[ix])),
        ValueData::ZSet(members) => (Some(members[ix].1.to_string().into()), cell(&members[ix].0)),
        ValueData::Stream(entries) => {
            let entry = &entries[ix];
            let mut fields = String::new();
            for (k, v) in &entry.fields {
                if fields.len() > CELL_PREVIEW {
                    break;
                }
                if !fields.is_empty() {
                    fields.push_str("  ");
                }
                fields.push_str(&format::bytes_inline(k));
                fields.push('=');
                fields.push_str(&format::bytes_inline(v));
            }
            (Some(entry.id.clone().into()), fields.into())
        }
        _ => (None, SharedString::default()),
    }
}

/// The full value of row `ix`, for the detail pane.
fn row_value(data: &ValueData, ix: usize) -> Option<Vec<u8>> {
    Some(match data {
        ValueData::Hash(pairs) => pairs.get(ix)?.1.clone(),
        ValueData::List(items) | ValueData::Set(items) => items.get(ix)?.clone(),
        ValueData::ZSet(members) => members.get(ix)?.0.clone(),
        ValueData::Stream(entries) => {
            let entry = entries.get(ix)?;
            let object: serde_json::Map<String, serde_json::Value> = entry
                .fields
                .iter()
                .map(|(k, v)| {
                    (
                        format::bytes(k).into_owned(),
                        format::bytes(v).into_owned().into(),
                    )
                })
                .collect();
            serde_json::to_vec(&object).ok()?
        }
        _ => return None,
    })
}
