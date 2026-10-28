use std::{collections::BTreeSet, ops::Range, rc::Rc, time::Duration};

use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UniformListScrollHandle,
    WeakEntity, Window,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariant, ButtonVariants as _},
        h_flex,
        input::{Editor, EditorState, Input, InputEvent, InputState},
        menu::{DropdownMenu as _, PopupMenuItem},
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
    Client, EditError, Key, ValueData, format,
    formats::ValueFormat,
    viewer::{self, Applied, Loaded, Mutation, Outcome, PAGE, RowClick, RowEdit, TextEdit},
};

use crate::{
    edit::{self, ChangeForm, OnDone},
    style,
};

const ROW_HEIGHT: f32 = 26.;
/// Width of a narrow first column (list index, score, stream ID).
const NARROW_COLUMN: f32 = 150.;

pub enum ValueEvent {
    Deleted(Key),
    /// The key shown was renamed.
    Renamed {
        from: Key,
        to: Key,
    },
    /// The key's type or TTL changed.
    Changed(Key),
}

/// A text editor showing a value (or a row's value) through a [`TextEdit`].
struct TextPane {
    edit: TextEdit,
    editor: Entity<EditorState>,
    /// Typed into since it was filled.
    dirty: bool,
    _subscription: Subscription,
}

/// The row of a collection open in the detail pane.
struct RowPane {
    edit: RowEdit,
    text: TextPane,
    /// A hash field's name or a sorted set member's score.
    name: Option<Entity<InputState>>,
    name_dirty: bool,
    _subscription: Option<Subscription>,
}

impl RowPane {
    fn dirty(&self) -> bool {
        self.text.dirty || self.name_dirty
    }
}

/// Which text a format change or a save is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Value,
    Row,
}

/// The selected key's value. Loading, paging, formats and the editing
/// rules live in [`viewer`]; this view draws them and runs the requests.
pub struct ValuePanel {
    client: Client,
    key: Option<Key>,
    loaded: Option<Loaded>,
    loading: bool,
    loading_more: bool,
    error: Option<SharedString>,
    /// A string or JSON value as text.
    text: Option<TextPane>,
    /// Rows picked for deleting: the clicked one, plus any ⌘-clicked.
    picked: BTreeSet<usize>,
    /// The row open in the detail pane (the last one clicked).
    row: Option<RowPane>,
    confirm_delete: bool,
    /// A change is on its way to the server.
    saving: bool,
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
            text: None,
            picked: BTreeSet::new(),
            row: None,
            confirm_delete: false,
            saving: false,
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
        self.row = None;
        self.picked.clear();
        self.error = None;
        self.confirm_delete = false;
        self.loading = true;
        self.loading_more = false;
        self._more = None;
        cx.notify();

        let request = viewer::load(&self.client, key);
        let writable = !self.read_only();
        // Replacing the task drops the previous load, so fast arrowing through
        // keys never shows a stale value.
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            // Decoding a string (gzip, MessagePack, ...) into the text to
            // show takes a while for big values: not on the UI thread.
            let result = match request.await {
                Ok(loaded) => Ok(cx
                    .background_executor()
                    .spawn(async move {
                        let edit = loaded.text_edit(writable);
                        (loaded, edit)
                    })
                    .await),
                Err(err) => Err(err),
            };
            this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok((loaded, edit)) => this.show(loaded, edit, window, cx),
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
        self.row = None;
        self.picked.clear();
        self.error = None;
        self.confirm_delete = false;
        self.loading = false;
        self.loading_more = false;
        self._load = None;
        self._more = None;
        cx.notify();
    }

    /// Shows a loaded value, and `edit`, its text if it is a string or a
    /// JSON document ([`Loaded::text_edit`]).
    fn show(
        &mut self,
        loaded: Loaded,
        edit: Option<TextEdit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text = edit.map(|edit| Self::text_pane(edit, window, cx));
        self.loaded = Some(loaded);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// An editor for `edit`'s text, marking itself dirty on the first
    /// change.
    fn text_pane(edit: TextEdit, window: &mut Window, cx: &mut Context<Self>) -> TextPane {
        let text = edit.text().to_string();
        let is_json = edit.view().is_json;
        let editor = cx.new(|cx| {
            let state = EditorState::new(window, cx)
                .line_number(true)
                .soft_wrap(true)
                .default_value(text);
            // The language can only be set when the editor is made.
            if is_json {
                state.language("json")
            } else {
                state
            }
        });
        let watched = editor.entity_id();
        let subscription = cx.subscribe(&editor, move |this, _, event: &InputEvent, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let pane = match (&mut this.text, &mut this.row) {
                (Some(pane), _) if pane.editor.entity_id() == watched => pane,
                (_, Some(row)) if row.text.editor.entity_id() == watched => &mut row.text,
                _ => return,
            };
            if pane.edit.editable() && !pane.dirty {
                pane.dirty = true;
                cx.notify();
            }
        });
        TextPane {
            edit,
            editor,
            dirty: false,
            _subscription: subscription,
        }
    }

    fn pane(&self, pane: Pane) -> Option<&TextPane> {
        match pane {
            Pane::Value => self.text.as_ref(),
            Pane::Row => self.row.as_ref().map(|row| &row.text),
        }
    }

    /// Shows a text in another format, carrying unsaved typing over.
    fn set_format(
        &mut self,
        pane: Pane,
        format: ValueFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.pane(pane) else {
            return;
        };
        let mut edit = current.edit.clone();
        let typed = current.editor.read(cx).value().to_string();
        let draft = current.dirty.then_some(typed.as_str());
        if let Err(err) = edit.set_format(format, draft) {
            window.push_notification(
                Notification::error(format!("{err:#}"))
                    .title(format!("Cannot show as {}", format.label())),
                cx,
            );
            return;
        }
        let mut fresh = Self::text_pane(edit, window, cx);
        fresh.dirty = draft.is_some() && fresh.edit.is_changed(fresh.edit.text());
        match pane {
            Pane::Value => self.text = Some(fresh),
            Pane::Row => {
                if let Some(row) = self.row.as_mut() {
                    *row.edit.value_mut() = fresh.edit.clone();
                    row.text = fresh;
                }
            }
        }
        cx.notify();
    }

    /// Saves the open row if it was edited, else the whole value (⌘S).
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only() || self.saving {
            return;
        }
        if self.row.as_ref().is_some_and(RowPane::dirty) {
            self.save_row(window, cx);
        } else if self.text.as_ref().is_some_and(|text| text.dirty) {
            self.save_text(window, cx);
        }
    }

    fn save_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(loaded), Some(text)) = (self.loaded.as_ref(), self.text.as_mut()) else {
            return;
        };
        let edited = text.editor.read(cx).value();
        match loaded.save_text(&text.edit, &edited) {
            Ok(Some(mutation)) => self.send(mutation, window, cx),
            Ok(None) => {
                text.dirty = false;
                cx.notify();
            }
            Err(err) => window.push_notification(
                Notification::error(format!("{err:#}")).title("Cannot save"),
                cx,
            ),
        }
    }

    fn save_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(loaded), Some(row)) = (self.loaded.as_ref(), self.row.as_mut()) else {
            return;
        };
        let value = row.text.editor.read(cx).value();
        // A name not typed into stands for the stored one, whatever the
        // input made of it.
        let name = row
            .name
            .as_ref()
            .filter(|_| row.name_dirty)
            .map(|name| name.read(cx).value());
        // The key comes from the loaded value, which follows renames.
        match loaded.save_row(&row.edit, &value, name.as_deref()) {
            Ok(Some(mutation)) => self.send(mutation, window, cx),
            Ok(None) => {
                row.text.dirty = false;
                row.name_dirty = false;
                cx.notify();
            }
            Err(err) => window.push_notification(
                Notification::error(format!("{err:#}")).title("Cannot save"),
                cx,
            ),
        }
    }

    /// Discards the open row's edits.
    fn cancel_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.row.as_ref().map(|row| row.edit.row()) {
            self.open_row(ix, window, cx);
        }
    }

    /// Sends a change, then shows it. A change refused because what it
    /// writes exists asks whether to replace it.
    fn send(&mut self, mutation: Mutation, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.saving = true;
        cx.notify();
        let client = self.client.clone();
        let request = mutation.clone().send(&client);
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                if client.same_connection(&this.client) {
                    match result {
                        Ok(outcome) => this.done(mutation, outcome, window, cx),
                        Err(err) => this.refused(mutation, err, window, cx),
                    }
                } else {
                    Self::elsewhere(&client, &mutation, result, window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reports a change that ran on a connection (a database) the panel has
    /// left since: nothing here shows that database any more, so nothing is
    /// patched or asked again.
    fn elsewhere(
        client: &Client,
        mutation: &Mutation,
        result: anyhow::Result<Outcome>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let db = client.config().db;
        let note = match result {
            Ok(outcome) => {
                Notification::success(format!("{} (db{db})", mutation.done_message(&outcome)))
            }
            Err(err) => {
                Notification::error(format!("{err:#}")).title(format!("Save failed (db{db})"))
            }
        };
        window.push_notification(note, cx);
    }

    fn refused(
        &mut self,
        mutation: Mutation,
        err: anyhow::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edit = err.downcast_ref::<EditError>();
        if edit == Some(&EditError::ElementExists)
            && let (Some(question), Some(again)) =
                (mutation.overwrite_question(), mutation.overwriting())
        {
            let this = cx.entity().downgrade();
            window.open_alert_dialog(cx, move |alert, _, _| {
                let this = this.clone();
                let again = again.clone();
                alert
                    .title("Replace?")
                    .description(SharedString::from(question.clone()))
                    .ok_text("Replace")
                    .ok_variant(ButtonVariant::Danger)
                    .show_cancel(true)
                    .on_ok(move |_, window, cx| {
                        let again = again.clone();
                        this.update(cx, |this, cx| this.send(again, window, cx))
                            .ok();
                        true
                    })
            });
            return;
        }
        let title = match edit {
            Some(EditError::Changed | EditError::ElementMissing) => "The value changed",
            Some(EditError::KeyMissing) => "The key is gone",
            _ => "Save failed",
        };
        window.push_notification(Notification::error(format!("{err:#}")).title(title), cx);
        if matches!(edit, Some(EditError::KeyMissing | EditError::WrongType(_))) {
            self.reload(window, cx);
        }
    }

    /// Shows a change the server took: patched into the loaded value where
    /// that is possible (keeping pages and scroll), else loaded again.
    fn done(
        &mut self,
        mutation: Mutation,
        outcome: Outcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(Notification::success(mutation.done_message(&outcome)), cx);
        match &mutation {
            Mutation::Rename { from, to } => {
                if self.key.as_ref() == Some(from) {
                    self.key = Some(to.clone());
                }
                cx.emit(ValueEvent::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
            Mutation::SetTtl { key, .. } => cx.emit(ValueEvent::Changed(key.clone())),
            _ => {}
        }
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let row_change = !matches!(
            mutation,
            Mutation::Rename { .. }
                | Mutation::SetTtl { .. }
                | Mutation::SetString { .. }
                | Mutation::SetJson { .. }
        );
        match loaded.apply(&mutation, &outcome) {
            Applied::KeyGone => {
                let key = loaded.key.clone();
                self.key = None;
                self.loaded = None;
                self.text = None;
                self.row = None;
                self.picked.clear();
                cx.emit(ValueEvent::Deleted(key));
            }
            Applied::Reload => self.reload(window, cx),
            Applied::Patched(select) => {
                if matches!(
                    mutation,
                    Mutation::SetString { .. } | Mutation::SetJson { .. }
                ) {
                    // The text now stands for the saved bytes.
                    let edit = loaded.text_edit(!self.client.config().read_only);
                    let format = self.text.as_ref().map(|text| text.edit.format());
                    self.text = edit.map(|mut edit| {
                        if let Some(format) = format {
                            let _ = edit.set_format(format, None);
                        }
                        Self::text_pane(edit, window, cx)
                    });
                }
                if row_change {
                    self.picked.clear();
                    match select {
                        Some(ix) => {
                            self.open_row(ix, window, cx);
                            self.picked.insert(ix);
                            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
                        }
                        None => self.row = None,
                    }
                }
            }
        }
        cx.notify();
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
                            this.row = None;
                            this.picked.clear();
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

    /// A click opens the row; ⌘-click adds it to (or takes it out of) the
    /// rows picked for deleting. Unsaved edits of the open row are never
    /// dropped without asking (see [`viewer::click_row`]).
    fn click_row(&mut self, ix: usize, add: bool, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.row.as_ref().map(|row| row.edit.row());
        let dirty = self.row.as_ref().is_some_and(RowPane::dirty);
        match viewer::click_row(&mut self.picked, open, dirty, ix, add) {
            RowClick::Open(ix) => self.open_row(ix, window, cx),
            RowClick::Keep => {}
            RowClick::AskFirst(ix) => self.ask_to_discard(ix, window, cx),
        }
        cx.notify();
    }

    /// Asks before opening row `ix` in place of the open row's edits.
    fn ask_to_discard(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title("Discard changes?")
                .description("The open row has changes that are not saved.")
                .ok_text("Discard")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let open = this.row.as_ref().map(|row| row.edit.row());
                        if let RowClick::Open(ix) =
                            viewer::click_row(&mut this.picked, open, false, ix, false)
                        {
                            this.open_row(ix, window, cx);
                        }
                        cx.notify();
                    })
                    .ok();
                    true
                })
        });
    }

    /// Opens row `ix` in the detail pane, editable on a writable connection.
    fn open_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let writable = !self.read_only();
        let Some(edit) = self
            .loaded
            .as_ref()
            .and_then(|loaded| loaded.row_edit(ix, writable))
        else {
            self.row = None;
            return;
        };
        let text = Self::text_pane(edit.value().clone(), window, cx);
        let (name, subscription) = match edit.name() {
            Some(field) => {
                let value = field.text.clone();
                let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
                let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                    if let (InputEvent::Change, Some(row)) = (event, this.row.as_mut())
                        && !row.name_dirty
                    {
                        row.name_dirty = true;
                        cx.notify();
                    }
                });
                (Some(input), Some(subscription))
            }
            None => (None, None),
        };
        self.row = Some(RowPane {
            edit,
            text,
            name,
            name_dirty: false,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Asks before deleting the picked rows.
    fn request_delete_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let rows: Vec<usize> = self.picked.iter().copied().collect();
        let Some(mutation) = loaded.delete_rows(&rows) else {
            return;
        };
        let what = match rows.len() {
            1 => "this row".to_string(),
            n => format!("these {} rows", format::count(n as u64)),
        };
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let mutation = mutation.clone();
            alert
                .title("Delete?")
                .description(SharedString::from(format!("Delete {what} from the value?")))
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    let mutation = mutation.clone();
                    this.update(cx, |this, cx| this.send(mutation, window, cx))
                        .ok();
                    true
                })
        });
    }

    /// What a dialog's change does once the server took it: the same as a
    /// change made here.
    fn on_done(&self, cx: &Context<Self>) -> OnDone {
        let this: WeakEntity<Self> = cx.weak_entity();
        let client = self.client.clone();
        Rc::new(
            move |mutation: Mutation, outcome: Outcome, window: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| {
                    if client.same_connection(&this.client) {
                        this.done(mutation, outcome, window, cx);
                    } else {
                        Self::elsewhere(&client, &mutation, Ok(outcome), window, cx);
                    }
                })
                .ok();
            },
        )
    }

    fn open_add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(add) = self.loaded.as_ref().and_then(Loaded::add_row) else {
            return;
        };
        let title = add.title();
        let (client, on_done) = (self.client.clone(), self.on_done(cx));
        let form = cx.new(|cx| ChangeForm::add_row(client, add, on_done, window, cx));
        edit::open(form, title, window, cx);
    }

    fn open_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.key.clone() else {
            return;
        };
        let (client, on_done) = (self.client.clone(), self.on_done(cx));
        let form = cx.new(|cx| ChangeForm::rename(client, key, on_done, window, cx));
        edit::open(form, "Rename key", window, cx);
    }

    fn open_ttl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(key), Some(loaded)) = (self.key.clone(), self.loaded.as_ref()) else {
            return;
        };
        let ttl_ms = loaded.meta.ttl_ms;
        let (client, on_done) = (self.client.clone(), self.on_done(cx));
        let form = cx.new(|cx| ChangeForm::ttl(client, key, ttl_ms, on_done, window, cx));
        edit::open(form, "Expiry", window, cx);
    }

    fn copy(&self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.pane(pane) {
            let value = text.editor.read(cx).value().to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(value));
            window.push_notification("Value copied", cx);
        }
    }

    /// A small dropdown listing the formats a text can be shown in.
    fn format_picker(&self, pane: Pane, cx: &Context<Self>) -> Option<impl IntoElement + use<>> {
        let text = self.pane(pane)?;
        let current = text.edit.format();
        let formats = text.edit.formats();
        if formats.len() < 2 {
            return None;
        }
        let this = cx.entity().downgrade();
        let id = match pane {
            Pane::Value => "value-format",
            Pane::Row => "row-format",
        };
        Some(
            Button::new(id)
                .ghost()
                .xsmall()
                .label(current.label())
                .icon(IconName::ChevronDown)
                .tooltip("Show as")
                .dropdown_menu(move |menu, _, _| {
                    formats.iter().fold(menu, |menu, &format| {
                        let this = this.clone();
                        menu.item(
                            PopupMenuItem::new(format.label())
                                .checked(format == current)
                                .on_click(move |_, window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.set_format(pane, format, window, cx)
                                    })
                                    .ok();
                                }),
                        )
                    })
                }),
        )
    }

    fn render_header(&self, loaded: Option<&Loaded>, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let writable = !self.read_only();
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
        let saveable = self.text.as_ref().is_some_and(|t| t.dirty);

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
            .when(writable && loaded.is_some(), |this| {
                this.child(
                    Button::new("rename")
                        .ghost()
                        .xsmall()
                        .icon(gpui_kit::assets::IconName::Pencil)
                        .tooltip("Rename key")
                        .on_click(cx.listener(|this, _, window, cx| this.open_rename(window, cx))),
                )
            })
            .when_some(ttl, |this, ttl| {
                if writable {
                    this.child(
                        Button::new("ttl")
                            .ghost()
                            .xsmall()
                            .icon(gpui_kit::assets::IconName::Clock)
                            .label(ttl)
                            .tooltip("Change expiry")
                            .on_click(cx.listener(|this, _, window, cx| this.open_ttl(window, cx))),
                    )
                } else {
                    this.child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(gpui_kit::assets::IconName::Clock).xsmall())
                            .child(ttl),
                    )
                }
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
            .when_some(self.format_picker(Pane::Value, cx), |this, picker| {
                this.child(picker)
            })
            .when(self.text.is_some(), |this| {
                this.child(
                    Button::new("copy-value")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Copy)
                        .label("Copy")
                        .tooltip("Copy the value as shown")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.copy(Pane::Value, window, cx)),
                        ),
                )
            })
            .when(saveable && writable, |this| {
                this.child(
                    Button::new("save")
                        .primary()
                        .small()
                        .label("Save")
                        .loading(self.saving)
                        .tooltip("Save (⌘S)")
                        .on_click(cx.listener(|this, _, window, cx| this.save_text(window, cx))),
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
            .when(writable, |this| {
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
                // Highlighted: exactly the rows Delete would remove.
                let selected = self.picked.contains(&ix);
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
                        cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
                            let add = event.modifiers().platform || event.modifiers().control;
                            this.click_row(ix, add, window, cx)
                        }),
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
        let writable = !self.read_only();
        let add_title = loaded.add_row().map(|add| add.title());
        let picked = self.picked.len();
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
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().child(loaded.footer()))
                    .when_some(add_title.filter(|_| writable), |this, title| {
                        this.child(
                            Button::new("add-row")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .label(title)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.open_add_row(window, cx)
                                    }),
                                ),
                        )
                    })
                    .when(writable && picked > 0, |this| {
                        let label = if picked == 1 {
                            "Delete".to_string()
                        } else {
                            format!("Delete {}", format::count(picked as u64))
                        };
                        this.child(
                            Button::new("delete-rows")
                                .ghost()
                                .xsmall()
                                .icon(gpui_kit::assets::IconName::Trash)
                                .label(label)
                                .tooltip("Delete the selected rows (⌘-click to select more)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.request_delete_rows(window, cx)
                                })),
                        )
                    })
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

    /// The open row: its field name or score, format, Save / Cancel, and
    /// its value.
    fn render_row(&self, row: &RowPane, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let writable = !self.read_only();
        let dirty = writable && row.dirty();
        let label = match (&row.name, self.loaded.as_ref().map(|l| &l.data)) {
            (Some(_), _) => None,
            (None, Some(ValueData::List(_))) => Some(format!("#{}", row.edit.row())),
            (None, Some(ValueData::Stream(_))) => self
                .loaded
                .as_ref()
                .and_then(|l| l.row_cells(row.edit.row()).0),
            _ => None,
        };
        let name_label = row.edit.name().map(|field| (field.label, field.editable));
        let notice = row.text.edit.view().notice.clone().filter(|_| writable);
        v_flex()
            .h(px(260.))
            .flex_none()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .gap_2()
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when_some(
                        row.name.as_ref().zip(name_label),
                        |this, (input, (label, editable))| {
                            this.child(label).child(
                                div().w(px(220.)).child(
                                    Input::new(input).xsmall().disabled(!editable || !writable),
                                ),
                            )
                        },
                    )
                    .when_some(label, |this, label| {
                        this.child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .child(label),
                        )
                    })
                    .child(div().flex_1())
                    .when_some(self.format_picker(Pane::Row, cx), |this, picker| {
                        this.child(picker)
                    })
                    .child(
                        Button::new("copy-row")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip("Copy the value as shown")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.copy(Pane::Row, window, cx)),
                            ),
                    )
                    .when(dirty, |this| {
                        this.child(
                            Button::new("cancel-row")
                                .ghost()
                                .xsmall()
                                .label("Cancel")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.cancel_row(window, cx)),
                                ),
                        )
                        .child(
                            Button::new("save-row")
                                .primary()
                                .xsmall()
                                .label("Save")
                                .loading(self.saving)
                                .tooltip("Save (⌘S)")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.save_row(window, cx)),
                                ),
                        )
                    }),
            )
            .when_some(notice, |this, notice| this.child(notice_bar(notice, cx)))
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(&row.text.editor)
                        .readonly(!row.text.edit.editable() || !writable)
                        .bordered(false)
                        .h_full(),
                ),
            )
    }

    fn render_text(&self, loaded: &Loaded, cx: &Context<Self>) -> impl IntoElement {
        let writable = !self.read_only();
        let notice = self
            .text
            .as_ref()
            .and_then(|text| text.edit.view().notice.clone())
            .filter(|_| writable && !loaded.is_preview());
        v_flex()
            .flex_1()
            .min_h_0()
            .when_some(loaded.preview_notice(), |this, notice| {
                this.child(notice_bar(notice, cx))
            })
            .when_some(notice, |this, notice| this.child(notice_bar(notice, cx)))
            .when_some(self.text.as_ref(), |this, text| {
                this.child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&text.editor)
                            .readonly(!text.edit.editable() || !writable)
                            .bordered(false)
                            .h_full(),
                    ),
                )
            })
    }
}

/// A line of advice above an editor.
fn notice_bar(notice: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .px_3()
        .py_1()
        .flex_none()
        .text_xs()
        .bg(theme.warning.opacity(0.12))
        .text_color(theme.warning)
        .child(notice.into())
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
                    .when_some(self.row.as_ref(), |this, row| {
                        this.child(self.render_row(row, cx))
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
