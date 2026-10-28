//! The dialog of a bulk delete: it counts the keys, asks, deletes with
//! progress and a way to stop, and reports. The flow itself is
//! [`BulkDelete`]; this view runs the walks it asks for and draws it.

use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, IntoElement,
    ParentElement as _, Render, Styled as _, Task, Window,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        notification::Notification,
        progress::Progress,
        spinner::Spinner,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use redcrab_core::{
    Client, DeleteProgress,
    bulk::{BulkDelete, Ending, Phase, Refresh, Stop, Target},
    progress::Stopper,
};

pub enum BulkEvent {
    /// The flow is over; how the key list should catch up, if anything
    /// changed.
    Finished(Option<Refresh>),
}

pub struct BulkDeleteView {
    /// Where the keys are deleted, whatever database the key list moves
    /// to meanwhile.
    client: Client,
    flow: BulkDelete,
    /// Where the keys are: "cache · 10.0.0.5:6379 · db0".
    location: String,
    /// [`BulkEvent::Finished`] went out.
    reported: bool,
    /// The dialog is up. The view outlives it when the user closes it
    /// while a delete is stopping; the outcome then comes as a
    /// notification.
    open: bool,
    /// Asks the delete to stop and say how far it got.
    stopper: Option<Stopper>,
    /// The count or the delete; dropping it abandons the walk.
    _walk: Option<Task<()>>,
}

impl EventEmitter<BulkEvent> for BulkDeleteView {}

impl BulkDeleteView {
    /// Opens the dialog for `target` and starts counting.
    pub fn open(client: Client, target: Target, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let config = client.config();
        let location = format!("{} · {} · db{}", config.name, config.address(), config.db);
        let view = cx.new(|_| Self {
            client,
            flow: BulkDelete::new(target),
            location,
            reported: false,
            open: true,
            stopper: None,
            _walk: None,
        });
        view.update(cx, |this, cx| this.count(window, cx));
        let dialog_view = view.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let title = dialog_view.read(cx).flow.title();
            let ok = dialog_view.clone();
            let cancel = dialog_view.clone();
            dialog
                .title(title)
                .w(px(500.))
                .overlay_closable(false)
                // Enter only closes a dialog that is done: deleting takes a
                // click on the button that says how many keys go.
                .on_ok(move |_, _, cx| {
                    ok.update(cx, |this, _| {
                        let over = this.flow.is_over();
                        this.open &= !over;
                        over
                    })
                })
                // Escape and the close button stop whatever runs, and close.
                .on_cancel(move |_, window, cx| {
                    cancel.update(cx, |this, cx| this.dismiss(window, cx));
                    true
                })
                .child(dialog_view.clone())
        });
        view
    }

    /// The connection the keys are deleted on.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Whether the flow is over and reported: nothing more comes of it.
    pub fn is_done(&self) -> bool {
        self.reported
    }

    fn count(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut events = self.client.count_matching(self.flow.pattern().to_vec());
        self._walk = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        if this.flow.on_count(event) {
                            this.changed(window, cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                // The count keeps only its latest state while this waits.
                cx.background_executor()
                    .timer(Duration::from_millis(66))
                    .await;
            }
        }));
    }

    fn confirm(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.flow.confirm() {
            return;
        }
        match self.client.delete_matching(self.flow.pattern().to_vec()) {
            Ok(mut events) => {
                self.stopper = Some(events.stopper());
                // Read to the end, also after a stop: the walk's last word
                // is how many keys really went.
                self._walk = Some(cx.spawn_in(window, async move |this, cx| {
                    while let Some(event) = events.next().await {
                        let alive = this
                            .update_in(cx, |this, window, cx| {
                                if this.flow.on_delete(event) {
                                    this.changed(window, cx);
                                }
                            })
                            .is_ok();
                        if !alive {
                            break;
                        }
                        cx.background_executor()
                            .timer(Duration::from_millis(66))
                            .await;
                    }
                }));
            }
            // The menus offer no delete on a read-only connection; this is
            // the core's guard answering anyway.
            Err(err) => {
                self.flow.on_delete(DeleteProgress::Failed {
                    stats: Default::default(),
                    error: err.to_string(),
                });
            }
        }
        self.changed(window, cx);
    }

    /// Stops the count, or asks the delete to stop: it goes on until the
    /// round trip it has in flight is answered, then says how far it got.
    fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.flow.stop() {
            Stop::Nothing => return,
            Stop::Cancelled => self._walk = None,
            Stop::Ask => {
                if let Some(stopper) = &self.stopper {
                    stopper.stop();
                }
            }
        }
        self.changed(window, cx);
    }

    /// The dialog is going away (Escape, the close or Cancel button): stop
    /// whatever runs. A delete still says how far it got, in a
    /// notification.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.stop(window, cx);
    }

    /// Redraws, and once the flow is over tells the key list how to catch
    /// up. A delete that went through needs no more reading: the dialog
    /// closes and a notification says what happened, as it does when the
    /// dialog was closed before the delete ended.
    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        if !self.flow.is_over() || self.reported {
            return;
        }
        self.reported = true;
        self.stopper = None;
        cx.emit(BulkEvent::Finished(self.flow.refresh()));
        let Phase::Finished { stats, ending } = self.flow.phase() else {
            return;
        };
        let message = self.flow.message();
        if matches!(ending, Ending::Completed(_)) && stats.failed == 0 {
            window.push_notification(Notification::success(message), cx);
            if self.open {
                self.open = false;
                window.close_dialog(cx);
            }
        } else if !self.open {
            let note = match ending {
                Ending::Failed(_) => Notification::error(message),
                _ => Notification::info(message),
            };
            window.push_notification(note, cx);
        }
    }

    fn buttons(&self, cx: &Context<Self>) -> Vec<Button> {
        let close = || {
            Button::new("bulk-close")
                .label("Close")
                .primary()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open = false;
                    window.close_dialog(cx);
                }))
        };
        let cancel = || {
            Button::new("bulk-cancel")
                .label("Cancel")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.dismiss(window, cx);
                    window.close_dialog(cx);
                }))
        };
        match self.flow.phase() {
            Phase::Counting { .. } => vec![cancel()],
            Phase::Confirm { .. } => vec![
                cancel(),
                Button::new("bulk-confirm")
                    .danger()
                    .label(self.flow.confirm_label().unwrap_or_default())
                    .icon(gpui_kit::assets::IconName::Trash)
                    .on_click(cx.listener(Self::confirm)),
            ],
            Phase::Deleting(_) => vec![
                Button::new("bulk-stop")
                    .label("Stop")
                    .on_click(cx.listener(|this, _, window, cx| this.stop(window, cx))),
            ],
            Phase::Stopping(_) => vec![
                Button::new("bulk-stop")
                    .label("Stopping…")
                    .loading(true)
                    .disabled(true),
            ],
            _ => vec![close()],
        }
    }
}

impl Render for BulkDeleteView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let failed = matches!(
            self.flow.phase(),
            Phase::CountFailed(_)
                | Phase::Finished {
                    ending: Ending::Failed(_),
                    ..
                }
        );
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(self.location.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .when(self.flow.is_running(), |this| {
                        this.child(Spinner::new().small())
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .when(failed, |this| this.text_color(theme.danger))
                            .child(self.flow.message()),
                    ),
            )
            .when_some(self.flow.confirm_note(), |this, note| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(note),
                )
            })
            .when_some(self.flow.progress(), |this, progress| {
                this.child(Progress::new("bulk-progress").value(progress * 100.))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .children(self.buttons(cx)),
            )
    }
}
