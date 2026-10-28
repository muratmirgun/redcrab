//! The memory analysis: which keys, types and key prefixes take the memory
//! of the server. It walks the keyspace when asked, never on its own, since
//! on a large server that takes a while; a sample is quicker.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use futures::StreamExt as _;
use gpui_kit::{
    Context, EventEmitter, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Task, Window,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        menu::{DropdownMenu as _, PopupMenuItem},
        progress::Progress,
        spinner::Spinner,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use redcrab_core::{
    AnalysisProgress, Client,
    analysis::{Analysis, BigKey, PrefixRow, Totals},
    format,
    progress::Stopper,
};

use crate::style;

/// The samples offered: every key, or one in ten, or one in a hundred.
const SAMPLES: [u32; 3] = [1, 10, 100];

fn sample_label(every: u32) -> String {
    match every {
        1 => "Every key".to_owned(),
        n => format!("1 key in {n}"),
    }
}

pub enum MemoryEvent {
    /// Show this key in the keys view.
    OpenKey(Vec<u8>),
}

pub struct MemoryView {
    client: Client,
    every: u32,
    analysis: Option<Arc<Mutex<Analysis>>>,
    progress: Option<AnalysisProgress>,
    stopper: Option<Stopper>,
    /// The prefix looked into; empty for the top.
    path: Vec<u8>,
    _walk: Option<Task<()>>,
}

impl EventEmitter<MemoryEvent> for MemoryView {}

impl MemoryView {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            every: 1,
            analysis: None,
            progress: None,
            stopper: None,
            path: Vec::new(),
            _walk: None,
        }
    }

    /// Another database: the analysis was of the one before, so it goes.
    pub fn set_client(&mut self, client: Client, cx: &mut Context<Self>) {
        self.client = client;
        self.stop();
        self.analysis = None;
        self.progress = None;
        self._walk = None;
        self.path.clear();
        cx.notify();
    }

    fn running(&self) -> bool {
        matches!(self.progress, Some(AnalysisProgress::Running { .. }))
    }

    pub fn analyze(&mut self, cx: &mut Context<Self>) {
        let (analysis, mut updates) = self.client.analyze_memory(self.every);
        self.stopper = Some(updates.stopper());
        self.analysis = Some(analysis);
        self.progress = Some(AnalysisProgress::Running {
            scanned: 0,
            measured: 0,
        });
        self.path.clear();
        self._walk = Some(cx.spawn(async move |this, cx| {
            while let Some(progress) = updates.next().await {
                let alive = this
                    .update(cx, |this, cx| {
                        this.progress = Some(progress);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                // The figures keep only their latest state meanwhile.
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
            }
        }));
        cx.notify();
    }

    fn stop(&mut self) {
        if let Some(stopper) = self.stopper.take() {
            stopper.stop();
        }
    }

    fn lock(&self) -> Option<MutexGuard<'_, Analysis>> {
        self.analysis
            .as_ref()
            .map(|a| a.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let running = self.running();
        let every = self.every;
        let view = cx.weak_entity();
        let status = match &self.progress {
            None => "Measures the keys with SCAN and MEMORY USAGE. On a large, busy \
                     server, a sample is quicker and gentler."
                .to_owned(),
            Some(AnalysisProgress::Running { scanned, measured }) => format!(
                "Scanned {} keys, measured {}…",
                format::count(*scanned),
                format::count(*measured)
            ),
            Some(AnalysisProgress::Done) => {
                let analysis = self.lock();
                let elapsed = analysis
                    .as_ref()
                    .and_then(|a| a.elapsed)
                    .map_or(String::new(), |d| format!(" in {:.1} s", d.as_secs_f64()));
                format!(
                    "Measured {} of {} keys{elapsed}.",
                    format::count(analysis.as_ref().map_or(0, |a| a.measured.keys)),
                    format::count(analysis.as_ref().map_or(0, |a| a.scanned)),
                )
            }
            Some(AnalysisProgress::Stopped) => "Stopped; what was measured is below.".to_owned(),
            Some(AnalysisProgress::Failed(err)) => format!("Stopped: {err}"),
        };
        let failed = matches!(self.progress, Some(AnalysisProgress::Failed(_)));
        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(if running {
                Button::new("memory-stop")
                    .small()
                    .label("Stop")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop();
                        cx.notify();
                    }))
            } else {
                Button::new("memory-analyze")
                    .small()
                    .primary()
                    .label(if self.analysis.is_some() {
                        "Analyze Again"
                    } else {
                        "Analyze"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.analyze(cx)))
            })
            .child(
                Button::new("memory-sample")
                    .small()
                    .ghost()
                    .label(sample_label(every))
                    .dropdown_caret(true)
                    .disabled(running)
                    .tooltip("Which keys are measured; the totals of a sample are estimates")
                    .dropdown_menu(move |menu, _, _| {
                        SAMPLES.iter().fold(menu, |menu, &n| {
                            let view = view.clone();
                            menu.item(
                                PopupMenuItem::new(sample_label(n))
                                    .checked(n == every)
                                    .on_click(move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.every = n;
                                            cx.notify();
                                        })
                                        .ok();
                                    }),
                            )
                        })
                    }),
            )
            .when(running, |this| this.child(Spinner::new().small()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(if failed {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(status),
            )
    }

    fn render_report(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let Some(analysis) = self.lock() else {
            return div().into_any_element();
        };
        let every = analysis.every();
        let sampled = analysis.is_sampled();
        let estimate = if sampled { "≈ " } else { "" };
        let whole = analysis.measured.scaled(every);
        let share = |totals: Totals| {
            if analysis.measured.bytes == 0 {
                0.
            } else {
                totals.bytes as f32 / analysis.measured.bytes as f32
            }
        };
        let tiles = h_flex()
            .gap_3()
            .child(figure(
                "Memory",
                format!("{estimate}{}", format::size(whole.bytes)),
                if sampled {
                    format!(
                        "{} measured, times {every}",
                        format::size(analysis.measured.bytes)
                    )
                } else {
                    "What MEMORY USAGE reports, keys and values".to_owned()
                },
                &theme,
            ))
            .child(figure(
                "Keys",
                format!("{estimate}{}", format::count(whole.keys)),
                format!("{} scanned", format::count(analysis.scanned)),
                &theme,
            ))
            .child(figure(
                "Without a TTL",
                format!(
                    "{estimate}{}",
                    format::size(analysis.without_ttl.scaled(every).bytes)
                ),
                format!(
                    "{:.0}% of the memory, {} keys",
                    share(analysis.without_ttl) * 100.,
                    format::count(analysis.without_ttl.scaled(every).keys)
                ),
                &theme,
            ));

        let types = analysis.by_type();
        let by_type =
            section("By type", &theme).children(types.into_iter().map(|(kind, totals)| {
                bar_row(
                    style::type_badge(&kind, cx).into_any_element(),
                    totals.scaled(every),
                    share(totals),
                    estimate,
                    theme.chart_2,
                    &theme,
                )
            }));

        let prefixes = analysis.prefixes(&self.path);
        let separator = self.client.config().separator_byte();
        let crumbs = crumbs(&self.path, separator);
        let view = cx.weak_entity();
        let by_prefix = section("By key prefix", &theme)
            .child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .children(crumbs.into_iter().enumerate().map(|(ix, (label, path))| {
                        let view = view.clone();
                        h_flex()
                            .gap_1()
                            .when(ix > 0, |this| {
                                this.child(div().text_color(theme.muted_foreground).child("›"))
                            })
                            .child(
                                div()
                                    .id(("crumb", ix))
                                    .cursor_pointer()
                                    .text_color(theme.link)
                                    .child(label)
                                    .on_click(move |_, _, cx| {
                                        let path = path.clone();
                                        view.update(cx, |this, cx| {
                                            this.path = path;
                                            cx.notify();
                                        })
                                        .ok();
                                    }),
                            )
                    })),
            )
            .when(prefixes.is_empty(), |this| {
                this.child(div().text_sm().text_color(theme.muted_foreground).child(
                    if self.path.is_empty() {
                        format!(
                            "No key has a prefix: none holds the separator {:?}.",
                            char::from(separator)
                        )
                    } else {
                        "Nothing under this prefix.".to_owned()
                    },
                ))
            })
            .children(prefixes.into_iter().take(100).enumerate().map(|(ix, row)| {
                prefix_row(ix, row, share, every, estimate, &theme, cx.weak_entity())
            }));

        let biggest = analysis.biggest();
        let view = cx.weak_entity();
        let biggest = section("Biggest keys", &theme).children(
            biggest.into_iter().enumerate().map(|(ix, key)| {
                let view = view.clone();
                big_key_row(ix, key, &theme, cx, move |key, cx| {
                    view.update(cx, |_, cx| cx.emit(MemoryEvent::OpenKey(key)))
                        .ok();
                })
            }),
        );

        v_flex()
            .id("memory-report")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_4()
            .child(tiles)
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(by_type))
                    .child(div().flex_1().min_w_0().child(by_prefix)),
            )
            .child(biggest)
            .into_any_element()
    }
}

impl Render for MemoryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running();
        let progress = match self.progress {
            Some(AnalysisProgress::Running { scanned, .. }) => Some(scanned),
            _ => None,
        };
        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .when_some(progress.filter(|_| running), |this, _| {
                // How far is unknown until the walk ends: an indeterminate bar.
                this.child(Progress::new("memory-progress").loading(true).h(px(2.)))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .when(self.analysis.is_some(), |this| {
                        this.child(self.render_report(cx))
                    }),
            )
    }
}

fn figure(
    title: &'static str,
    value: String,
    note: String,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    v_flex()
        .flex_1()
        .p_3()
        .gap_1()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(title),
        )
        .child(
            div()
                .text_xl()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(value),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(note),
        )
}

fn section(title: &'static str, theme: &gpui_kit::component::Theme) -> gpui_kit::Div {
    v_flex().gap_1().child(
        div()
            .pb_1()
            .text_sm()
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(theme.foreground)
            .child(title),
    )
}

/// A label, the keys and bytes of something, and a bar of its share.
fn bar_row(
    label: gpui_kit::AnyElement,
    totals: Totals,
    share: f32,
    estimate: &'static str,
    color: Hsla,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    v_flex()
        .gap_0p5()
        .child(
            h_flex()
                .gap_2()
                .text_sm()
                .child(div().flex_1().min_w_0().overflow_hidden().child(label))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("{estimate}{} keys", format::count(totals.keys))),
                )
                .child(
                    div()
                        .w(px(90.))
                        .text_right()
                        .child(format!("{estimate}{}", format::size(totals.bytes))),
                )
                .child(
                    div()
                        .w(px(44.))
                        .text_right()
                        .text_color(theme.muted_foreground)
                        .child(format!("{:.0}%", share * 100.)),
                ),
        )
        .child(
            div()
                .h(px(4.))
                .w_full()
                .rounded_full()
                .bg(theme.muted)
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(color)
                        .w(gpui_kit::relative(share.clamp(0., 1.))),
                ),
        )
}

fn prefix_row(
    ix: usize,
    row: PrefixRow,
    share: impl Fn(Totals) -> f32,
    every: u32,
    estimate: &'static str,
    theme: &gpui_kit::component::Theme,
    view: gpui_kit::WeakEntity<MemoryView>,
) -> impl IntoElement {
    let fraction = share(row.totals);
    let totals = row.totals.scaled(every);
    let label = match &row.prefix {
        Some(prefix) => div()
            .truncate()
            .font_family(theme.mono_font_family.clone())
            .child(format::bytes_inline(prefix).into_owned()),
        None => div()
            .text_color(theme.muted_foreground)
            .child("(other prefixes, summed)"),
    };
    let nested = row.nested;
    div()
        .id(("prefix", ix))
        .rounded_md()
        .px_1()
        .when(nested, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(theme.accent.opacity(0.4)))
                .on_click(move |_, _, cx| {
                    if let Some(prefix) = row.prefix.clone() {
                        view.update(cx, |this, cx| {
                            this.path = prefix;
                            cx.notify();
                        })
                        .ok();
                    }
                })
        })
        .child(bar_row(
            label.into_any_element(),
            totals,
            fraction,
            estimate,
            theme.chart_1,
            theme,
        ))
}

fn big_key_row(
    ix: usize,
    key: BigKey,
    theme: &gpui_kit::component::Theme,
    cx: &gpui_kit::App,
    open: impl Fn(Vec<u8>, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    let ttl = key
        .ttl_ms
        .and_then(|ms| format::ttl(ms as i64))
        .unwrap_or_else(|| "no TTL".to_owned());
    let name = key.key.clone();
    h_flex()
        .id(("big", ix))
        .gap_2()
        .px_1()
        .py_0p5()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.bg(theme.accent.opacity(0.4)))
        .on_click(move |_, _, cx| open(name.clone(), cx))
        .child(style::type_badge(&key.kind, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.mono_font_family.clone())
                .child(format::bytes_inline(&key.key).into_owned()),
        )
        .child(div().text_color(theme.muted_foreground).child(ttl))
        .child(div().w(px(90.)).text_right().child(format::size(key.bytes)))
}

/// "All keys › user › user:sessions", each with the prefix it goes to.
fn crumbs(path: &[u8], separator: u8) -> Vec<(String, Vec<u8>)> {
    let mut crumbs = vec![("All keys".to_owned(), Vec::new())];
    if path.is_empty() {
        return crumbs;
    }
    let mut prefix = Vec::new();
    for segment in path.split(|&b| b == separator) {
        if !prefix.is_empty() {
            prefix.push(separator);
        }
        prefix.extend_from_slice(segment);
        crumbs.push((format::bytes_inline(segment).into_owned(), prefix.clone()));
    }
    crumbs
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::TestAppContext;
    use gpui_kit::{AppContext as _, WindowOptions};
    use redcrab_core::{AnalysisProgress, Client, ConnectionConfig};

    use super::{MemoryView, crumbs};

    /// Against the test environment (`REDCRAB_IT=1`): an analysis of the
    /// standalone server's database 0 runs to its end, and the report draws
    /// at the top and inside a prefix.
    #[gpui::test]
    fn an_analysis_runs_and_its_report_draws(cx: &mut TestAppContext) {
        if std::env::var("REDCRAB_IT").as_deref() != Ok("1") {
            return;
        }
        let config = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: 6400,
            ..ConnectionConfig::default()
        };
        let client = futures::executor::block_on(Client::connect(config)).unwrap();
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| MemoryView::new(client))
            })
            .unwrap()
        });
        view.update(cx, |view, cx| view.analyze(cx));
        for _ in 0..1000 {
            cx.executor().advance_clock(Duration::from_millis(300));
            cx.run_until_parked();
            if view.read_with(cx, |view, _| !view.running()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        view.read_with(cx, |view, _| {
            assert_eq!(view.progress, Some(AnalysisProgress::Done));
        });
        // A keystroke draws a frame first.
        cx.simulate_keystrokes(handle, "shift");
        let first = view.read_with(cx, |view, _| {
            view.lock()
                .unwrap()
                .prefixes(b"")
                .into_iter()
                .find_map(|row| row.prefix.filter(|_| row.nested))
        });
        if let Some(prefix) = first {
            view.update(cx, |view, cx| {
                view.path = prefix;
                cx.notify();
            });
            cx.simulate_keystrokes(handle, "shift");
        }
    }

    #[test]
    fn crumbs_lead_back_up_the_path() {
        assert_eq!(crumbs(b"", b':'), [("All keys".to_owned(), vec![])]);
        assert_eq!(
            crumbs(b"user:sessions", b':'),
            [
                ("All keys".to_owned(), vec![]),
                ("user".to_owned(), b"user".to_vec()),
                ("sessions".to_owned(), b"user:sessions".to_vec()),
            ]
        );
    }
}
