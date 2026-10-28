//! Updates: looks for a new release on start and once a day (unless the
//! user turned that off), offers it in a dialog, and on the user's word
//! downloads it, installs it and restarts. The work itself is
//! [`redcrab_core::update`]; this decides when it runs and draws it.
//!
//! Only builds made by the packaging scripts look for updates
//! ([`update::enabled`]); in any other build [`Updater::global`] is `None`
//! and nothing here shows.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use futures::StreamExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, Global, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window,
    component::{
        ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex,
        notification::Notification,
        progress::Progress,
        spinner::Spinner,
        text::TextView,
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};
use redcrab_core::{
    format,
    progress::Stopper,
    update::{self, Install, Release, Settings, UpdateProgress},
};

/// How long after start the first check waits, so it does not compete
/// with opening the window and connecting.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(5);
/// How often the app looks whether a daily check is due. Checking the
/// clock, not sleeping a day, keeps a laptop that slept on schedule.
const LOOK_EVERY: Duration = Duration::from_secs(60 * 60);

struct GlobalUpdater(Entity<Updater>);

impl Global for GlobalUpdater {}

/// Where an update stands.
#[derive(Clone)]
pub enum State {
    Idle,
    Checking,
    /// A newer release, offered and not yet taken or skipped.
    Available(Release),
    Downloading {
        release: Release,
        done: u64,
        total: u64,
        stopper: Stopper,
    },
    Installing(Release),
    /// The update failed; `installed` when only the restart did, and the
    /// new version waits for the next start.
    Failed {
        release: Release,
        error: String,
        installed: bool,
    },
}

impl State {
    /// The release this is about, if any.
    pub fn version(&self) -> Option<&update::Version> {
        match self {
            Self::Available(release)
            | Self::Downloading { release, .. }
            | Self::Installing(release)
            | Self::Failed { release, .. } => Some(&release.version),
            Self::Idle | Self::Checking => None,
        }
    }
}

/// The app's one updater, shared by its windows.
pub struct Updater {
    settings: Settings,
    /// Where `settings` are saved (`update.json`).
    settings_file: Option<PathBuf>,
    install: Install,
    state: State,
    _schedule: Task<()>,
    _work: Option<Task<()>>,
}

/// Starts the updater, in a build that updates itself.
pub fn init(cx: &mut App) {
    if !update::enabled() {
        return;
    }
    let install = Install::detect();
    tracing::info!("updates: {install:?}");
    let leftovers = install.clone();
    cx.background_executor()
        .spawn(async move { leftovers.clean_up() })
        .detach();
    let updater = cx.new(|cx| Updater {
        settings: Settings::load(),
        settings_file: Settings::path(),
        install,
        state: State::Idle,
        _schedule: Updater::schedule(cx),
        _work: None,
    });
    cx.set_global(GlobalUpdater(updater));
}

impl Updater {
    /// The updater, in a build that updates itself.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalUpdater>()
            .map(|global| global.0.clone())
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn automatic(&self) -> bool {
        self.settings.automatic
    }

    /// What the title bar's update button says, and whether it spins:
    /// `None` hides it.
    pub fn button(&self) -> Option<(String, bool)> {
        match &self.state {
            State::Available(release) => Some((format!("Update to {}", release.version), false)),
            State::Downloading { done, total, .. } => Some((
                format!(
                    "Updating {}%",
                    (done * 100).checked_div(*total).unwrap_or(0)
                ),
                true,
            )),
            State::Installing(_) => Some(("Installing…".to_owned(), true)),
            State::Failed {
                installed: false, ..
            } => Some(("Update failed".to_owned(), false)),
            State::Failed {
                installed: true, ..
            } => Some(("Restart to update".to_owned(), false)),
            State::Idle | State::Checking => None,
        }
    }

    /// Whether a check or an update runs, so another cannot start.
    pub fn busy(&self) -> bool {
        matches!(
            self.state,
            State::Checking | State::Downloading { .. } | State::Installing(_)
        )
    }

    fn schedule(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK_AFTER).await;
            loop {
                let alive = this
                    .update(cx, |this, cx| {
                        if !this.busy() && this.settings.due(SystemTime::now()) {
                            this.check(None, cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(LOOK_EVERY).await;
            }
        })
    }

    /// Looks for a new release. A check the user asked for from `window`
    /// says what it found there, also when there is nothing new; an
    /// automatic one stays quiet unless it finds a release the user did
    /// not skip.
    pub fn check(&mut self, window: Option<AnyWindowHandle>, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let asked = window.is_some();
        // Leaves an offered release in the title bar if this finds nothing.
        let before = std::mem::replace(&mut self.state, State::Checking);
        self.settings.checked(SystemTime::now());
        self.save();
        cx.notify();
        let found = update::check();
        self._work = Some(cx.spawn(async move |this, cx| {
            let found = found.await;
            this.update(cx, |this, cx| {
                this.state = before;
                match found {
                    Ok(Some(release)) if asked || this.settings.offers(&release) => {
                        tracing::info!("redcrab {} is available", release.version);
                        this.state = State::Available(release);
                        this.offer(window, cx);
                    }
                    Ok(Some(release)) => {
                        tracing::info!("redcrab {} is available, but skipped", release.version);
                    }
                    Ok(_) => {
                        if asked {
                            notify(
                                window,
                                Notification::success(format!(
                                    "redcrab {} is the latest version",
                                    update::current_version()
                                )),
                                cx,
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!("{err:#}");
                        if asked {
                            notify(
                                window,
                                Notification::error(format!("{err:#}"))
                                    .title("Could not look for updates"),
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Opens the update dialog in `window`, or in the active window. A
    /// window busy with a dialog of its own is not interrupted: the title
    /// bar's update button stays to open it later.
    pub fn offer(&mut self, window: Option<AnyWindowHandle>, cx: &mut Context<Self>) {
        let asked = window.is_some();
        let Some(handle) = window
            .or_else(|| cx.active_window())
            .or_else(|| cx.windows().first().copied())
        else {
            return;
        };
        let updater = cx.entity();
        handle
            .update(cx, |_, window, cx| {
                if asked || !window.has_active_dialog(cx) {
                    UpdateDialog::open(updater, window, cx);
                }
            })
            .ok();
    }

    pub fn set_automatic(&mut self, automatic: bool, cx: &mut Context<Self>) {
        self.settings.automatic = automatic;
        self.save();
        cx.notify();
    }

    /// Not offered again; a newer release is.
    fn skip(&mut self, cx: &mut Context<Self>) {
        if let State::Available(release) = &self.state {
            self.settings.skipped = Some(release.version.clone());
            self.save();
            self.state = State::Idle;
            cx.notify();
        }
    }

    /// Downloads and installs the offered release, then restarts into it.
    fn install(&mut self, cx: &mut Context<Self>) {
        let State::Available(release) = &self.state else {
            return;
        };
        let release = release.clone();
        let mut updates = update::start(release.clone(), self.install.clone());
        self.state = State::Downloading {
            release: release.clone(),
            done: 0,
            total: 0,
            stopper: updates.stopper(),
        };
        cx.notify();
        self._work = Some(cx.spawn(async move |this, cx| {
            while let Some(progress) = updates.next().await {
                let alive = this
                    .update(cx, |this, cx| this.progress(&release, progress, cx))
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

    fn progress(&mut self, release: &Release, progress: UpdateProgress, cx: &mut Context<Self>) {
        let release = release.clone();
        match progress {
            UpdateProgress::Downloading { done, total } => {
                if let State::Downloading {
                    done: d, total: t, ..
                } = &mut self.state
                {
                    (*d, *t) = (done, total);
                }
            }
            UpdateProgress::Installing => {
                tracing::info!("installing redcrab {}", release.version);
                self.state = State::Installing(release);
            }
            UpdateProgress::Installed(installed) => match installed.relaunch() {
                Ok(()) => {
                    tracing::info!("restarting into redcrab {}", installed.version());
                    cx.quit();
                }
                Err(err) => {
                    self.state = State::Failed {
                        release,
                        error: format!("{err:#}"),
                        installed: true,
                    }
                }
            },
            UpdateProgress::Failed(error) => {
                self.state = State::Failed {
                    release,
                    error,
                    installed: false,
                }
            }
            UpdateProgress::Stopped => self.state = State::Available(release),
        }
        cx.notify();
    }

    /// Stops a download; an install goes on.
    fn stop(&mut self) {
        if let State::Downloading { stopper, .. } = &self.state {
            stopper.stop();
        }
    }

    /// A failed update goes back to being offered; one that is installed
    /// and only waits for a restart is done.
    fn dismiss_failure(&mut self, cx: &mut Context<Self>) {
        if let State::Failed {
            release, installed, ..
        } = &self.state
        {
            self.state = if *installed {
                State::Idle
            } else {
                State::Available(release.clone())
            };
            cx.notify();
        }
    }

    fn save(&self) {
        let Some(file) = &self.settings_file else {
            return;
        };
        if let Err(err) = self.settings.save_to(file) {
            tracing::warn!("could not save the update settings: {err:#}");
        }
    }
}

fn notify(window: Option<AnyWindowHandle>, note: Notification, cx: &mut App) {
    let Some(handle) = window.or_else(|| cx.active_window()) else {
        return;
    };
    handle
        .update(cx, |_, window, cx| window.push_notification(note, cx))
        .ok();
}

/// The update dialog: the release and its notes, then the download's
/// progress, or what went wrong.
struct UpdateDialog {
    updater: Entity<Updater>,
    _observe: Subscription,
}

impl UpdateDialog {
    fn open(updater: Entity<Updater>, window: &mut Window, cx: &mut App) {
        let view = cx.new(|cx| Self {
            _observe: cx.observe(&updater, |_, _, cx| cx.notify()),
            updater,
        });
        window.open_dialog(cx, move |dialog, _, cx| {
            let updater = view.read(cx).updater.clone();
            let title = match updater.read(cx).state().version() {
                Some(version) => format!("redcrab {version} is available"),
                None => "Updates".to_owned(),
            };
            dialog
                .title(title)
                .w(px(560.))
                .overlay_closable(false)
                // Escape and the close button stop a download; once the
                // update installs, the dialog stays until it restarts.
                .on_cancel(move |_, _, cx| {
                    updater.update(cx, |updater, cx| match updater.state {
                        State::Installing(_) => false,
                        _ => {
                            updater.stop();
                            updater.dismiss_failure(cx);
                            true
                        }
                    })
                })
                .child(view.clone())
        });
    }

    fn buttons(&self, cx: &mut Context<Self>) -> Vec<Button> {
        let updater = self.updater.read(cx);
        let close = |label: &'static str| {
            Button::new("update-close")
                .label(label)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.updater
                        .update(cx, |updater, cx| updater.dismiss_failure(cx));
                    window.close_dialog(cx);
                }))
        };
        let download = |page: String| {
            Button::new("update-download")
                .label("Download")
                .icon(gpui_kit::assets::IconName::ExternalLink)
                .on_click(move |_, window, cx| {
                    cx.open_url(&page);
                    window.close_dialog(cx);
                })
        };
        match &updater.state {
            State::Available(release) => match release.installable(&updater.install) {
                Ok(()) => vec![
                    Button::new("update-skip")
                        .ghost()
                        .label("Skip This Version")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.updater.update(cx, |updater, cx| updater.skip(cx));
                            window.close_dialog(cx);
                        })),
                    close("Later"),
                    Button::new("update-install")
                        .primary()
                        .label("Install and Restart")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.updater.update(cx, |updater, cx| updater.install(cx));
                        })),
                ],
                Err(_) => vec![close("Later"), download(release.page.clone()).primary()],
            },
            State::Downloading { .. } => {
                vec![
                    Button::new("update-stop")
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.updater.update(cx, |updater, _| updater.stop());
                        })),
                ]
            }
            State::Installing(_) => vec![
                Button::new("update-installing")
                    .label("Installing…")
                    .loading(true)
                    .disabled(true),
            ],
            State::Failed {
                installed: true, ..
            } => vec![close("Close").primary()],
            State::Failed { release, .. } => {
                vec![close("Close"), download(release.page.clone()).primary()]
            }
            State::Idle | State::Checking => vec![close("Close")],
        }
    }
}

impl Render for UpdateDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let buttons = self.buttons(cx);
        let theme = cx.theme();
        let updater = self.updater.read(cx);
        let current = update::current_version();
        let muted = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let body = v_flex().gap_3();
        let body = match &updater.state {
            State::Available(release) => body
                .child(div().text_sm().child(format!(
                    "You have redcrab {current}. Open connections close when it restarts."
                )))
                .when(!release.notes.trim().is_empty(), |this| {
                    this.child(
                        div()
                            .id("update-notes")
                            .max_h(px(260.))
                            .overflow_y_scroll()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.muted)
                            .text_sm()
                            .child(TextView::markdown(
                                "update-notes-text",
                                release.notes.clone(),
                            )),
                    )
                })
                .when_some(release.installable(&updater.install).err(), |this, why| {
                    this.child(muted(format!("{why}. Download it from its page instead.")))
                })
                .child(
                    Checkbox::new("update-automatic")
                        .small()
                        .label("Check for updates automatically")
                        .checked(updater.settings.automatic)
                        .on_click({
                            let updater = self.updater.clone();
                            move |checked, _, cx| {
                                updater
                                    .update(cx, |updater, cx| updater.set_automatic(*checked, cx));
                            }
                        }),
                ),
            State::Downloading { done, total, .. } => body
                .child(div().text_sm().child(format!(
                    "Downloading… {} of {}",
                    format::size(*done),
                    format::size(*total)
                )))
                .child(Progress::new("update-progress").value(if *total == 0 {
                    0.
                } else {
                    *done as f32 / *total as f32 * 100.
                })),
            State::Installing(release) => {
                body.child(h_flex().gap_2().child(Spinner::new().small()).child(
                    div().text_sm().child(format!(
                        "Installing redcrab {}; it restarts when done.",
                        release.version
                    )),
                ))
            }
            State::Failed {
                release,
                error,
                installed: true,
            } => body
                .child(div().text_sm().child(format!(
                    "redcrab {} is installed, but did not start: {error}",
                    release.version
                )))
                .child(muted(
                    "Quit redcrab and open it again to use the new version.".to_owned(),
                )),
            State::Failed { error, .. } => body
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(format!("The update did not go through: {error}")),
                )
                .child(muted(format!(
                    "redcrab {current} is unchanged. The release page has the download."
                ))),
            State::Idle | State::Checking => body.child(
                h_flex()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(div().text_sm().child("Looking for updates…")),
            ),
        };
        body.child(h_flex().justify_end().gap_2().pt_1().children(buttons))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };

    use gpui::TestAppContext;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, Point, Render,
        Task, Window, WindowBounds, WindowOptions, component::WindowExt as _, div, px, size,
    };
    use redcrab_core::update::{Install, Release, Settings, Version};

    use super::{State, Updater};

    struct Host;

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A release whose files are on a port nothing listens on, so that an
    /// update to it fails at once.
    fn release() -> Release {
        let asset = |name: &str| {
            format!(
                r#"{{"name": "{name}", "size": 10, "browser_download_url": "http://127.0.0.1:1/{name}"}}"#
            )
        };
        let json = format!(
            r#"{{"tag_name": "v9.0.0", "body": "* Faster", "html_url": "https://example.invalid/v9.0.0",
                "assets": [{}, {}, {}]}}"#,
            asset("redcrab-9.0.0-macos-universal.dmg"),
            asset("SHA256SUMS"),
            asset("SHA256SUMS.sig"),
        );
        Release::from_github(json.as_bytes()).unwrap().unwrap()
    }

    fn mac_app() -> Install {
        Install::MacApp {
            bundle: "/Applications/redcrab.app".into(),
        }
    }

    fn settings_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("redcrab-gui-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("update.json")
    }

    /// A window, and an updater offering `release()` in it.
    fn offered(
        cx: &mut TestAppContext,
        install: Install,
        settings_file: &Path,
    ) -> (AnyWindowHandle, Entity<Updater>) {
        cx.update(gpui_kit::init);
        let updater = cx.new(|_| Updater {
            settings: Settings::default(),
            settings_file: Some(settings_file.to_owned()),
            install,
            state: State::Available(release()),
            _schedule: Task::ready(()),
            _work: None,
        });
        let handle = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(900.), px(700.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| Host))
                .unwrap()
                .0
        });
        updater.update(cx, |updater, cx| updater.offer(Some(handle), cx));
        cx.run_until_parked();
        (handle, updater)
    }

    fn dialog_open(cx: &mut TestAppContext, handle: AnyWindowHandle) -> bool {
        cx.update_window(handle, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    }

    #[gpui::test]
    fn an_offer_opens_the_dialog(cx: &mut TestAppContext) {
        let file = settings_file("offer");
        let (handle, updater) = offered(cx, mac_app(), &file);
        assert!(dialog_open(cx, handle));
        updater.read_with(cx, |updater, _| {
            assert_eq!(updater.state().version(), Some(&Version::new(9, 0, 0)));
            assert_eq!(updater.button(), Some(("Update to 9.0.0".into(), false)));
        });
    }

    #[gpui::test]
    fn an_automatic_offer_waits_for_a_window_busy_with_a_dialog(cx: &mut TestAppContext) {
        let file = settings_file("busy");
        let (handle, updater) = offered(cx, mac_app(), &file);
        cx.update_window(handle, |_, window, cx| window.close_dialog(cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Edit key"));
        })
        .unwrap();
        cx.run_until_parked();
        // The automatic offer leaves the window alone; one the user asks
        // for from it opens on top.
        updater.update(cx, |updater, cx| updater.offer(None, cx));
        cx.run_until_parked();
        let open = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| window.close_dialog(cx))
                .unwrap();
            cx.run_until_parked();
            dialog_open(cx, handle)
        };
        assert!(!open(cx), "only the user's dialog was open");
        cx.update_window(handle, |_, window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Edit key"));
        })
        .unwrap();
        updater.update(cx, |updater, cx| updater.offer(Some(handle), cx));
        cx.run_until_parked();
        assert!(open(cx), "the update dialog was on top of the user's");
    }

    #[gpui::test]
    fn a_skipped_release_is_remembered(cx: &mut TestAppContext) {
        let file = settings_file("skip");
        let (_, updater) = offered(cx, mac_app(), &file);
        updater.update(cx, |updater, cx| updater.skip(cx));
        updater.read_with(cx, |updater, _| {
            assert!(matches!(updater.state, State::Idle));
            assert_eq!(updater.button(), None);
        });
        assert_eq!(
            Settings::load_from(&file).skipped,
            Some(Version::new(9, 0, 0))
        );
    }

    #[gpui::test]
    fn the_daily_check_can_be_turned_off(cx: &mut TestAppContext) {
        let file = settings_file("automatic");
        let (_, updater) = offered(cx, mac_app(), &file);
        updater.update(cx, |updater, cx| updater.set_automatic(false, cx));
        updater.read_with(cx, |updater, _| assert!(!updater.automatic()));
        assert!(!Settings::load_from(&file).automatic);
    }

    #[gpui::test]
    fn a_copy_that_cannot_replace_itself_does_not_install(cx: &mut TestAppContext) {
        let file = settings_file("manual");
        let manual = Install::Manual {
            reason: "redcrab cannot write to /opt".into(),
        };
        let (_, updater) = offered(cx, manual, &file);
        updater.update(cx, |updater, cx| updater.install(cx));
        cx.run_until_parked();
        updater.read_with(cx, |updater, _| {
            assert!(matches!(
                &updater.state,
                State::Failed { installed: false, error, .. } if error.contains("cannot write to /opt")
            ));
        });
    }

    #[gpui::test]
    fn a_failed_update_says_so_and_is_offered_again(cx: &mut TestAppContext) {
        let file = settings_file("failed");
        let (_, updater) = offered(cx, mac_app(), &file);
        // The download runs on the I/O runtime's threads.
        cx.executor().allow_parking();
        updater.update(cx, |updater, cx| updater.install(cx));
        updater.read_with(cx, |updater, _| {
            assert!(matches!(updater.state, State::Downloading { .. }));
            assert!(updater.busy());
        });
        for _ in 0..500 {
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            if updater.read_with(cx, |updater, _| !updater.busy()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        updater.read_with(cx, |updater, _| {
            assert!(
                matches!(&updater.state, State::Failed { installed: false, error, .. } if error.contains("SHA256SUMS")),
            );
            assert_eq!(updater.button(), Some(("Update failed".into(), false)));
        });
        updater.update(cx, |updater, cx| updater.dismiss_failure(cx));
        updater.read_with(cx, |updater, _| {
            assert!(matches!(updater.state, State::Available(_)));
        });
    }
}
