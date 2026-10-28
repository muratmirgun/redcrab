use std::time::{Duration, Instant};

use gpui_kit::{
    App, AppContext as _, ClipboardItem, ContentMask, Context, Entity, Focusable as _, FontWeight,
    Hsla, InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TextAlign, TextRun, Window, canvas,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        scroll::ScrollableElement as _,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
};
use redcrab_core::{
    Client, ConsoleSession,
    console::{Edit, HistoryFile, Prompt, Suggestions},
    format,
};

use crate::{ClearConsole, CompleteNext, CompletePrev, HistoryNext, HistoryPrev, StopCommand};

/// The console's keys. Up, Down and Tab are the text input's own keys too
/// (cursor moves, indent), and the deepest context wins: these match in
/// the input itself, inside the console's wrapper, so they are as deep;
/// bound after the input's (after `gpui_kit::init`), they come first.
/// Inputs elsewhere keep their keys.
pub fn key_bindings() -> Vec<KeyBinding> {
    const INPUT: Option<&str> = Some("ConsoleInput > Input");
    vec![
        KeyBinding::new("up", HistoryPrev, INPUT),
        KeyBinding::new("down", HistoryNext, INPUT),
        KeyBinding::new("tab", CompleteNext, INPUT),
        KeyBinding::new("shift-tab", CompletePrev, INPUT),
        KeyBinding::new("cmd-k", ClearConsole, Some("Console")),
        KeyBinding::new("ctrl-l", ClearConsole, Some("Console")),
    ]
}

/// Replies longer than this are cut in the log.
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_ENTRIES: usize = 500;
/// Rows of the suggestion list shown at once.
const MAX_SUGGESTIONS: usize = 6;

struct Entry {
    command: SharedString,
    output: SharedString,
    error: bool,
    elapsed: Option<Duration>,
}

/// A `redis-cli` style prompt. It runs commands on a session of its own, so
/// SELECT or MULTI typed here never affect the rest of the app. History,
/// completion and hints come from [`Prompt`]; this view forwards keys and
/// draws.
pub struct Console {
    session: ConsoleSession,
    input: Entity<InputState>,
    prompt: Prompt,
    /// Where this connection's history is kept; `None` without a config
    /// directory (the history then lasts as long as the console).
    history_file: Option<HistoryFile>,
    entries: Vec<Entry>,
    /// Commands submitted and not answered yet: one running, the rest
    /// waiting behind it.
    running: usize,
    scroll: ScrollHandle,
    _subscription: Subscription,
    _load_history: Option<Task<()>>,
}

impl Console {
    pub fn new(client: &Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Run a command, e.g. INFO memory"));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    let (line, cursor) = line_and_cursor(input, cx);
                    this.prompt.edited(&line, cursor);
                    cx.notify();
                }
                InputEvent::Focus | InputEvent::Blur => cx.notify(),
            },
        );

        // The file is read off the UI thread; commands run before it is in
        // stay the newest.
        let history_file = HistoryFile::for_connection(&client.config().name);
        let load_history = history_file.clone().map(|file| {
            cx.spawn(async move |this, cx| {
                let loaded = cx
                    .background_executor()
                    .spawn(async move { file.load() })
                    .await;
                this.update(cx, |this, _| match loaded {
                    Ok(history) => this.prompt.history_mut().merge_older(history.iter()),
                    Err(err) => tracing::warn!("could not read the console history: {err:#}"),
                })
                .ok();
            })
        });

        Self {
            session: client.console(),
            input,
            prompt: Prompt::default(),
            history_file,
            entries: Vec::new(),
            running: 0,
            scroll: ScrollHandle::new(),
            _subscription: subscription,
            _load_history: load_history,
        }
    }

    /// Moves to another connection (another database of the server) with a
    /// new session; a command still running on the old one is abandoned.
    pub fn set_client(&mut self, client: &Client, cx: &mut Context<Self>) {
        self.session.cancel();
        self.session = client.console();
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.input.read(cx).value().trim().to_string();
        if line.is_empty() {
            return;
        }
        if self.prompt.submit(&line)
            && let Some(file) = &self.history_file
        {
            file.append_in_background(line.clone());
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if line.eq_ignore_ascii_case("clear") {
            self.clear(cx);
            return;
        }

        let args = match format::split_args(&line) {
            Ok(args) => args,
            Err(err) => {
                self.push(line.into(), err.into(), true, None, cx);
                return;
            }
        };
        self.running += 1;
        cx.notify();
        let started = Instant::now();
        let request = self.session.exec(args);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let elapsed = started.elapsed();
            this.update(cx, |this, cx| {
                this.running -= 1;
                let (output, error) = match result {
                    Ok(value) => (format::reply(&value), false),
                    Err(err) => (format!("(error) {err:#}"), true),
                };
                this.push(line.into(), clip(output).into(), error, Some(elapsed), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Abandons the running command and the ones waiting behind it; a
    /// `BLPOP key 0` would otherwise hold the console until disconnecting.
    fn stop(&mut self, cx: &mut Context<Self>) {
        if self.running > 0 {
            self.session.cancel();
            cx.notify();
        }
    }

    /// Escape: first closes the suggestion list, then stops a running
    /// command, else means something further up.
    fn on_stop(&mut self, _: &StopCommand, _: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.dismiss() {
            cx.notify();
            return;
        }
        if self.running == 0 {
            cx.propagate();
        }
        self.stop(cx);
    }

    fn on_history_prev(&mut self, _: &HistoryPrev, window: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = line_and_cursor(&self.input, cx);
        if let Some(line) = self.prompt.up(&line) {
            let cursor = line.len();
            self.show(Edit { text: line, cursor }, window, cx);
        }
    }

    fn on_history_next(&mut self, _: &HistoryNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(line) = self.prompt.down() {
            let cursor = line.len();
            self.show(Edit { text: line, cursor }, window, cx);
        }
    }

    fn on_complete_next(&mut self, _: &CompleteNext, window: &mut Window, cx: &mut Context<Self>) {
        self.complete(true, window, cx);
    }

    fn on_complete_prev(&mut self, _: &CompletePrev, window: &mut Window, cx: &mut Context<Self>) {
        self.complete(false, window, cx);
    }

    fn complete(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (line, cursor) = line_and_cursor(&self.input, cx);
        if let Some(edit) = self.prompt.tab(&line, cursor, forward) {
            self.show(edit, window, cx);
        }
    }

    fn pick(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = line_and_cursor(&self.input, cx);
        if let Some(edit) = self.prompt.pick(&line, ix) {
            self.show(edit, window, cx);
        }
        self.focus(window, cx);
    }

    /// Puts a line the prompt chose (history, completion) into the input.
    /// Setting a value emits no change event, so the prompt does not take it
    /// for typing.
    fn show(&mut self, edit: Edit, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(edit.text, window, cx);
            input.set_selected_range(edit.cursor..edit.cursor, cx);
        });
        cx.notify();
    }

    fn on_clear(&mut self, _: &ClearConsole, _: &mut Window, cx: &mut Context<Self>) {
        self.clear(cx);
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        cx.notify();
    }

    fn push(
        &mut self,
        command: SharedString,
        output: SharedString,
        error: bool,
        elapsed: Option<Duration>,
        cx: &mut Context<Self>,
    ) {
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            command,
            output,
            error,
            elapsed,
        });
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn render_entry(&self, ix: usize, entry: &Entry, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_color(theme.primary).child("›"))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(entry.command.clone()),
                    )
                    .when_some(entry.elapsed, |this, elapsed| {
                        this.child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(format!("{:.2} ms", elapsed.as_secs_f64() * 1000.0)),
                        )
                    })
                    .child(
                        Button::new(("copy-output", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip("Copy output")
                            .on_click({
                                let output = entry.output.clone();
                                move |_, _, cx: &mut App| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        output.to_string(),
                                    ));
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .pl_4()
                    .whitespace_normal()
                    .text_color(if entry.error {
                        theme.danger
                    } else {
                        theme.foreground
                    })
                    .child(entry.output.clone()),
            )
    }

    /// The list under the input: names and summaries, the Tab choice
    /// highlighted, a press picks one.
    fn render_suggestions(
        &self,
        suggestions: Suggestions<'_>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let count = suggestions.candidates.len();
        let start = suggestions
            .selected
            .map_or(0, |ix| ix.saturating_sub(MAX_SUGGESTIONS - 1));
        let end = (start + MAX_SUGGESTIONS).min(count);
        let more = count - (end - start);
        v_flex()
            .mt_1()
            .py_0p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_xs()
            .children(suggestions.candidates[start..end].iter().enumerate().map(
                |(offset, candidate)| {
                    let ix = start + offset;
                    let selected = suggestions.selected == Some(ix);
                    h_flex()
                        .id(("suggestion", ix))
                        .px_2()
                        .py_0p5()
                        .gap_3()
                        .cursor_pointer()
                        .when(selected, |this| this.bg(theme.list_active))
                        .hover(|this| this.bg(theme.list_hover))
                        // On press, not on click: the workspace takes focus
                        // on press, which would hide the list before the
                        // release; preventing that keeps the input focused.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.pick(ix, window, cx);
                            }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(candidate.name),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(candidate.summary.to_string()),
                        )
                },
            ))
            .when(more > 0, |this| {
                this.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .text_color(theme.muted_foreground)
                        .child(format!("{more} more · Tab cycles")),
                )
            })
    }
}

/// The input's text and cursor (a byte offset).
fn line_and_cursor(input: &Entity<InputState>, cx: &App) -> (String, usize) {
    let input = input.read(cx);
    (input.value().to_string(), input.cursor())
}

fn clip(mut output: String) -> String {
    if output.len() > MAX_OUTPUT {
        let total = output.len();
        let mut end = MAX_OUTPUT;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push_str(&format!(
            "\n… cut, {} of {} shown",
            format::size(end as u64),
            format::size(total as u64)
        ));
    }
    output
}

/// Paints `hint` greyed out right after the input's text, where its caret
/// would go next, clipped to the input. It reads where the text ends from
/// the input's layout of this very frame, so it must be painted after the
/// input (a later sibling).
fn paint_hint(input: Entity<InputState>, hint: SharedString, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, window: &mut Window, cx: &mut App| {
            let state = input.read(cx);
            let end = state.text().len();
            let Some(at) = state.range_to_bounds(&(end..end)) else {
                return;
            };
            let clip = state.input_bounds();
            let style = window.text_style();
            let font_size = style.font_size.to_pixels(window.rem_size());
            let run = TextRun {
                len: hint.len(),
                font: style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(hint, font_size, &[run], None);
            window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                let _ = line.paint(at.origin, at.size.height, TextAlign::Left, None, window, cx);
            });
        },
    )
    .absolute()
    .size_full()
}

impl Render for Console {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let status = match self.running {
            0 => None,
            1 => Some("running…".to_string()),
            n => Some(format!("running… {} queued", n - 1)),
        };
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let hint = focused
            .then(|| {
                let (line, cursor) = line_and_cursor(&self.input, cx);
                self.prompt
                    .hint(&line, cursor)
                    .map(|hint| SharedString::from(hint.to_string()))
            })
            .flatten();
        let suggestions = self
            .prompt
            .suggestions()
            .filter(|_| focused)
            .map(|suggestions| self.render_suggestions(suggestions, cx));

        v_flex()
            .size_full()
            .key_context("Console")
            .on_action(cx.listener(Self::on_stop))
            .on_action(cx.listener(Self::on_history_prev))
            .on_action(cx.listener(Self::on_history_next))
            .on_action(cx.listener(Self::on_complete_next))
            .on_action(cx.listener(Self::on_complete_prev))
            .on_action(cx.listener(Self::on_clear))
            .bg(theme.background)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("console-log")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .px_3()
                            .py_2()
                            .gap_2()
                            .font_family(mono.clone())
                            .text_xs()
                            .when(self.entries.is_empty(), |this| {
                                this.child(div().text_color(theme.muted_foreground).child(
                                    "Commands run on the console's own connection, so SELECT and MULTI stay here. ↑↓ history · Tab completes · ⌘K clears.",
                                ))
                            })
                            .children(
                                self.entries
                                    .iter()
                                    .enumerate()
                                    .map(|(ix, entry)| self.render_entry(ix, entry, cx)),
                            ),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        // Same font as the hint painted over it.
                        div()
                            .key_context("ConsoleInput")
                            .relative()
                            .font_family(mono)
                            .text_sm()
                            .child(
                                Input::new(&self.input)
                                    .small()
                                    .prefix(
                                        Icon::new(gpui_kit::assets::IconName::Terminal)
                                            .small()
                                            .text_color(theme.muted_foreground),
                                    )
                                    .when_some(status, |input, status| {
                                        input.suffix(
                                            h_flex()
                                                .gap_1()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(status)
                                                .child(
                                                    Button::new("stop")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("Stop")
                                                        .tooltip("Stop the running command (Esc)")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.stop(cx)
                                                        })),
                                                ),
                                        )
                                    }),
                            )
                            .when_some(hint, |this, hint| {
                                this.child(paint_hint(
                                    self.input.clone(),
                                    hint,
                                    theme.muted_foreground,
                                ))
                            }),
                    )
                    .children(suggestions),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Action, KeyContext, Keymap, Keystroke};

    use super::*;

    gpui_kit::actions!(test, [InputUp, InputDown, InputTab, InputOutdent, ListUp]);

    /// The bindings that win for `key` with `contexts` (outermost first) in
    /// focus, the input's own bound first as `gpui_kit::init` does.
    fn winner(key: &'static str, contexts: &[&str]) -> Option<Box<dyn Action>> {
        let mut bindings = vec![
            KeyBinding::new("up", InputUp, Some("Input")),
            KeyBinding::new("down", InputDown, Some("Input")),
            KeyBinding::new("tab", InputTab, Some("Input")),
            KeyBinding::new("shift-tab", InputOutdent, Some("Input")),
            KeyBinding::new("up", ListUp, Some("KeyList")),
        ];
        bindings.extend(key_bindings());
        let keymap = Keymap::new(bindings);
        let contexts: Vec<KeyContext> = contexts
            .iter()
            .map(|c| KeyContext::parse(c).unwrap())
            .collect();
        let (matched, _) = keymap.bindings_for_input(&[Keystroke::parse(key).unwrap()], &contexts);
        matched
            .first()
            .map(|binding| binding.action().boxed_clone())
    }

    fn is<A: Action + Default>(action: Option<Box<dyn Action>>) -> bool {
        action.is_some_and(|action| action.partial_eq(&A::default()))
    }

    #[test]
    fn console_keys_beat_the_inputs_own() {
        let console = ["Workspace", "Console", "ConsoleInput", "Input"];
        assert!(is::<HistoryPrev>(winner("up", &console)));
        assert!(is::<HistoryNext>(winner("down", &console)));
        assert!(is::<CompleteNext>(winner("tab", &console)));
        assert!(is::<CompletePrev>(winner("shift-tab", &console)));
        assert!(is::<ClearConsole>(winner("cmd-k", &console)));
    }

    #[test]
    fn other_inputs_keep_their_keys() {
        // The key browser's filter, and the console's log (no input).
        let search = ["Workspace", "KeyBrowser", "Input"];
        assert!(is::<InputUp>(winner("up", &search)));
        assert!(is::<InputTab>(winner("tab", &search)));
        assert!(is::<ListUp>(winner("up", &["Workspace", "KeyList"])));
        assert!(winner("up", &["Workspace", "Console"]).is_none());
        assert!(!is::<ClearConsole>(winner("cmd-k", &search)));
    }
}
