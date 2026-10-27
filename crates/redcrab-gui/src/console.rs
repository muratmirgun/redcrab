use std::time::{Duration, Instant};

use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window,
    component::{
        ActiveTheme as _, Icon, Sizable as _, h_flex,
        input::{Input, InputEvent, InputState},
        scroll::ScrollableElement as _,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
};
use redcrab_core::{Client, format};

/// Replies longer than this are cut in the log.
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_ENTRIES: usize = 500;

struct Entry {
    command: SharedString,
    output: SharedString,
    error: bool,
    elapsed: Option<Duration>,
}

/// A `redis-cli` style prompt.
pub struct Console {
    client: Client,
    input: Entity<InputState>,
    entries: Vec<Entry>,
    running: usize,
    scroll: ScrollHandle,
    _subscription: Subscription,
}

impl Console {
    pub fn new(client: Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Run a command, e.g. INFO memory"));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit(window, cx);
                }
            });
        Self {
            client,
            input,
            entries: Vec::new(),
            running: 0,
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.input.read(cx).value().trim().to_string();
        if line.is_empty() {
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if line.eq_ignore_ascii_case("clear") {
            self.entries.clear();
            cx.notify();
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
        let request = self.client.exec(args);
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

impl Render for Console {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        v_flex()
            .size_full()
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
                                    "Commands run on this connection. Type clear to empty the log.",
                                ))
                            })
                            .children(self.entries.iter().map(|entry| {
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
                                                    div().text_color(theme.muted_foreground).child(
                                                        format!(
                                                            "{:.2} ms",
                                                            elapsed.as_secs_f64() * 1000.0
                                                        ),
                                                    ),
                                                )
                                            }),
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
                            })),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
            .child(
                div().p_2().border_t_1().border_color(theme.border).child(
                    Input::new(&self.input)
                        .small()
                        .prefix(
                            Icon::new(gpui_kit::assets::IconName::Terminal)
                                .small()
                                .text_color(theme.muted_foreground),
                        )
                        .when(self.running > 0, |input| {
                            input.suffix(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("running…"),
                            )
                        }),
                ),
            )
    }
}
