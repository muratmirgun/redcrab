//! The dialogs that change keys: a new key, a row added to a collection, a
//! rename and a new TTL. Each is a small form; what was typed is checked
//! and turned into a [`Mutation`] by core ([`viewer`]), which also sends it.

use std::{cell::Cell, rc::Rc};

use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window,
    base::Selectable as _,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
        button::{Button, ButtonGroup, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
        notification::Notification,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use redcrab_core::{
    Client, EditError, Key, ListEnd, format,
    viewer::{self, AddRow, Mutation, NewKeyType, Outcome},
};

/// Called once the server took the change.
pub type OnDone = Rc<dyn Fn(Mutation, Outcome, &mut Window, &mut App)>;

enum Kind {
    NewKey(NewKeyType),
    AddRow(AddRow, ListEnd),
    Rename(Key),
    /// The key, and the TTL the input started with.
    Ttl(Key, String),
}

/// One dialog's form: its inputs, the error to show, and the change it
/// sends.
pub struct ChangeForm {
    client: Client,
    kind: Kind,
    /// The key name (new key, rename), the TTL, or a row's field, score
    /// or entry ID.
    first: Entity<InputState>,
    /// A new key's field, score or entry ID.
    second: Entity<InputState>,
    /// A new key's TTL.
    ttl: Entity<InputState>,
    /// The value, element, member, stream fields or JSON document.
    value: Entity<TextareaState>,
    error: Option<SharedString>,
    /// A change refused because what it writes exists, waiting for the
    /// user to replace it.
    overwrite: Option<Mutation>,
    busy: bool,
    on_done: OnDone,
    _subscriptions: Vec<Subscription>,
}

impl ChangeForm {
    fn new(
        client: Client,
        kind: Kind,
        on_done: OnDone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let first = cx.new(|cx| InputState::new(window, cx));
        let second = cx.new(|cx| InputState::new(window, cx));
        let ttl = cx.new(|cx| {
            InputState::new(window, cx).placeholder("No expiry, or e.g. 3600, 1h 30m, 2d")
        });
        let value = cx.new(|cx| TextareaState::new(window, cx).rows(5));
        let mut subscriptions: Vec<Subscription> =
            [&first, &second, &ttl]
                .into_iter()
                .map(|input| {
                    cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::PressEnter { .. } => this.submit(window, cx),
                            InputEvent::Change => this.edited(cx),
                            _ => {}
                        }
                    })
                })
                .collect();
        subscriptions.push(cx.subscribe(&value, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.edited(cx);
            }
        }));
        let mut this = Self {
            client,
            kind,
            first,
            second,
            ttl,
            value,
            error: None,
            overwrite: None,
            busy: false,
            on_done,
            _subscriptions: subscriptions,
        };
        this.set_placeholders(window, cx);
        this
    }

    /// A new key of any type.
    pub fn new_key(
        client: Client,
        on_done: OnDone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(
            client,
            Kind::NewKey(NewKeyType::default()),
            on_done,
            window,
            cx,
        )
    }

    /// A row added to the collection `add` is for.
    pub fn add_row(
        client: Client,
        add: AddRow,
        on_done: OnDone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(
            client,
            Kind::AddRow(add, ListEnd::Tail),
            on_done,
            window,
            cx,
        )
    }

    /// A new name for `key`.
    pub fn rename(
        client: Client,
        key: Key,
        on_done: OnDone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = Self::new(client, Kind::Rename(key.clone()), on_done, window, cx);
        // A name that is not plain text cannot be shown as typed; the new
        // one is typed from scratch.
        if let std::borrow::Cow::Borrowed(name) = format::bytes_inline(&key) {
            let name = name.to_string();
            this.first
                .update(cx, |input, cx| input.set_value(name, window, cx));
        }
        this
    }

    /// A new TTL for `key`, which has `ttl_ms` left (negative: none).
    pub fn ttl(
        client: Client,
        key: Key,
        ttl_ms: i64,
        on_done: OnDone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let current = viewer::ttl_prefill(ttl_ms);
        let this = Self::new(client, Kind::Ttl(key, current.clone()), on_done, window, cx);
        this.first
            .update(cx, |input, cx| input.set_value(current, window, cx));
        this
    }

    fn set_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (first, second, value) = match &self.kind {
            Kind::NewKey(ty) => ("user:1001", ty.name_placeholder(), ty.value_placeholder()),
            Kind::AddRow(add, _) => (add.name_placeholder(), "", add.value_placeholder()),
            Kind::Rename(_) => ("New name", "", ""),
            Kind::Ttl(..) => ("No expiry, or e.g. 3600, 1h 30m, 2d", "", ""),
        };
        self.first
            .update(cx, |input, cx| input.set_placeholder(first, window, cx));
        self.second
            .update(cx, |input, cx| input.set_placeholder(second, window, cx));
        self.value
            .update(cx, |input, cx| input.set_placeholder(value, window, cx));
    }

    /// The labels of the inputs the form shows: the first one, the second
    /// one (a new key's field, score or entry ID), and the value; `None`
    /// for one it does not show.
    fn labels(&self) -> [Option<&'static str>; 3] {
        match &self.kind {
            Kind::NewKey(ty) => [Some("Name"), ty.name_label(), Some(ty.value_label())],
            Kind::AddRow(add, _) => [add.name_label(), None, Some(add.value_label())],
            Kind::Rename(_) => [Some("New name"), None, None],
            Kind::Ttl(..) => [Some("Expires in"), None, None],
        }
    }

    /// Focuses the first input the form shows: the value, for a list's or
    /// a set's new row, which has no other.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.labels()[0].is_some() {
            self.first.update(cx, |input, cx| input.focus(window, cx));
        } else {
            self.value.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    fn edited(&mut self, cx: &mut Context<Self>) {
        // A question about the old input no longer applies.
        if self.overwrite.take().is_some() || self.error.take().is_some() {
            cx.notify();
        }
    }

    /// The change for what was typed, or `None` if it changes nothing.
    fn mutation(&self, cx: &App) -> anyhow::Result<Option<Mutation>> {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let value = self.value.read(cx).value().to_string();
        Ok(Some(match &self.kind {
            Kind::NewKey(ty) => viewer::new_key(
                *ty,
                &text(&self.first),
                &text(&self.second),
                &value,
                &text(&self.ttl),
            )?,
            Kind::AddRow(add, end) => add.build(&text(&self.first), &value, *end)?,
            Kind::Rename(key) => match viewer::rename_key(key, &text(&self.first))? {
                Some(rename) => rename,
                None => return Ok(None),
            },
            // Saved untouched, the TTL read at load time would push the
            // expiry back.
            Kind::Ttl(key, prefilled) => {
                match viewer::change_ttl(key, prefilled, &text(&self.first))? {
                    Some(ttl) => ttl,
                    None => return Ok(None),
                }
            }
        }))
    }

    /// Sends the change; closes the dialog once the server took it.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let mutation = match self.overwrite.take() {
            Some(refused) => refused.overwriting(),
            None => match self.mutation(cx) {
                Ok(mutation) => mutation,
                Err(err) => {
                    self.error = Some(format!("{err:#}").into());
                    cx.notify();
                    return;
                }
            },
        };
        let Some(mutation) = mutation else {
            window.close_dialog(cx);
            return;
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let request = mutation.clone().send(&self.client);
        let on_done = self.on_done.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut settled = Some((mutation, request.await));
            this.update_in(cx, |this, window, cx| {
                let Some((mutation, result)) = settled.take() else {
                    return;
                };
                this.busy = false;
                match result {
                    Ok(outcome) => {
                        window.close_dialog(cx);
                        (this.on_done)(mutation, outcome, window, cx);
                    }
                    Err(err) => {
                        let exists = err.downcast_ref() == Some(&EditError::ElementExists);
                        match mutation.overwrite_question().filter(|_| exists) {
                            Some(question) => {
                                this.error = Some(question.into());
                                this.overwrite = Some(mutation);
                            }
                            None => this.error = Some(error_text(&err).into()),
                        }
                    }
                }
                cx.notify();
            })
            .ok();
            // The dialog was closed (Cancel, Escape) while the change was on
            // its way. The server took it or refused it all the same: the
            // views must show it, and a refusal must still be told.
            if let Some((mutation, result)) = settled {
                cx.update(|window, cx| match result {
                    Ok(outcome) => on_done(mutation, outcome, window, cx),
                    Err(err) => window.push_notification(
                        Notification::error(error_text(&err)).title("Change not made"),
                        cx,
                    ),
                })
                .ok();
            }
        })
        .detach();
    }

    fn submit_label(&self) -> &'static str {
        if self.overwrite.is_some() {
            return "Replace";
        }
        match self.kind {
            Kind::NewKey(_) => "Create",
            Kind::AddRow(..) => "Add",
            Kind::Rename(_) => "Rename",
            Kind::Ttl(..) => "Save",
        }
    }

    fn render_type_picker(&self, ty: NewKeyType, cx: &Context<Self>) -> impl IntoElement {
        ButtonGroup::new("new-key-type")
            .outline()
            .small()
            .children(NewKeyType::ALL.into_iter().enumerate().map(|(ix, t)| {
                Button::new(("new-key-type", ix))
                    .label(t.label())
                    .selected(t == ty)
            }))
            .on_click(cx.listener(|this, clicked: &Vec<usize>, window, cx| {
                if let (Kind::NewKey(ty), Some(&ix)) = (&mut this.kind, clicked.first())
                    && let Some(&picked) = NewKeyType::ALL.get(ix)
                {
                    *ty = picked;
                    this.error = None;
                    this.overwrite = None;
                    this.set_placeholders(window, cx);
                    cx.notify();
                }
            }))
    }

    fn render_ends(&self, end: ListEnd, cx: &Context<Self>) -> impl IntoElement {
        ButtonGroup::new("list-end")
            .outline()
            .small()
            .child(
                Button::new("tail")
                    .label("At the end")
                    .selected(end == ListEnd::Tail),
            )
            .child(
                Button::new("head")
                    .label("At the start")
                    .selected(end == ListEnd::Head),
            )
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                if let Kind::AddRow(_, end) = &mut this.kind {
                    *end = if clicked.first() == Some(&1) {
                        ListEnd::Head
                    } else {
                        ListEnd::Tail
                    };
                    cx.notify();
                }
            }))
    }
}

/// An error as one sentence for a form: the change's own refusal if it
/// is one, else the whole chain.
fn error_text(err: &anyhow::Error) -> String {
    match err.downcast_ref::<EditError>() {
        Some(EditError::KeyExists) => "A key with that name already exists.".into(),
        Some(edit) => format!("{edit}."),
        None => format!("{err:#}"),
    }
}

fn field(label: &'static str, input: impl IntoElement, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(input)
}

impl Render for ChangeForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let [first_label, second_label, value_label] = self.labels();
        let color = if self.overwrite.is_some() {
            theme.warning
        } else {
            theme.danger
        };
        let form = v_flex().gap_3().w_full();
        let form = match &self.kind {
            Kind::NewKey(ty) => form.child(field("Type", self.render_type_picker(*ty, cx), cx)),
            Kind::AddRow(add, end) if add.has_ends() => {
                form.child(field("Where", self.render_ends(*end, cx), cx))
            }
            _ => form,
        };
        form.when_some(first_label, |this, label| {
            this.child(field(label, Input::new(&self.first).small(), cx))
        })
        .when_some(second_label, |this, label| {
            this.child(field(label, Input::new(&self.second).small(), cx))
        })
        .when_some(value_label, |this, label| {
            this.child(field(
                label,
                Textarea::new(&self.value).small().h(px(110.)),
                cx,
            ))
        })
        .when(matches!(self.kind, Kind::NewKey(_)), |this| {
            this.child(field("Expires in", Input::new(&self.ttl).small(), cx))
        })
        .when(matches!(self.kind, Kind::Ttl(..)), |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Leave empty to remove the expiry. A bare number is seconds."),
            )
        })
        .when_some(self.error.clone(), |this, error| {
            this.child(
                div()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .bg(color.opacity(0.12))
                    .text_color(color)
                    .text_sm()
                    .child(error),
            )
        })
        .child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("cancel")
                        .label("Cancel")
                        .small()
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("submit")
                        .label(self.submit_label())
                        .small()
                        .map(|b| {
                            if self.overwrite.is_some() {
                                b.danger()
                            } else {
                                b.primary()
                            }
                        })
                        .loading(self.busy)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
        )
    }
}

/// Opens `form` in a dialog titled `title`, its first input focused.
pub fn open(
    form: Entity<ChangeForm>,
    title: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let title: SharedString = title.into();
    let focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus.replace(false) {
            form.update(cx, |form, cx| form.focus(window, cx));
        }
        dialog.title(title.clone()).w(px(520.)).child(form.clone())
    });
}
