//! The user's preferences ([`redcrab_core::settings`]): applied to the app,
//! saved on every change, and the dialog that changes them (⌘,).

use std::path::PathBuf;

use gpui_kit::{
    App, Global, ParentElement as _, SharedString, Styled as _, Window,
    component::{
        Theme, ThemeMode, WindowExt as _,
        setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsView},
    },
    div, px,
};
use redcrab_core::{
    format,
    settings::{Appearance, KEY_LIMITS, SCAN_COUNTS, Settings, TextSize},
};

use crate::update::Updater;

/// The settings in effect, and where they are saved.
struct AppSettings {
    settings: Settings,
    file: Option<PathBuf>,
}

impl Global for AppSettings {}

/// Loads the saved settings and applies them.
pub fn init(cx: &mut App) {
    let file = Settings::path();
    let settings = file
        .as_deref()
        .map_or_else(Settings::default, Settings::load_from);
    init_with(settings, file, cx);
}

pub(crate) fn init_with(settings: Settings, file: Option<PathBuf>, cx: &mut App) {
    cx.set_global(AppSettings { settings, file });
    apply(cx);
}

/// The settings in effect; the defaults before [`init`].
pub fn get(cx: &App) -> Settings {
    cx.try_global::<AppSettings>()
        .map(|global| global.settings.clone())
        .unwrap_or_default()
}

/// Changes the settings, then saves and applies them if that changed
/// anything.
pub fn change(cx: &mut App, edit: impl FnOnce(&mut Settings)) {
    if !cx.has_global::<AppSettings>() {
        init_with(Settings::default(), None, cx);
    }
    let global = cx.global_mut::<AppSettings>();
    let before = global.settings.clone();
    edit(&mut global.settings);
    if global.settings == before {
        return;
    }
    if let Some(file) = &global.file
        && let Err(err) = global.settings.save_to(file)
    {
        tracing::warn!("could not save the settings: {err:#}");
    }
    apply(cx);
    cx.refresh_windows();
}

/// Sets the theme and the text size the settings ask for.
fn apply(cx: &mut App) {
    let settings = get(cx);
    match settings.appearance {
        Appearance::System => Theme::sync_system_appearance(None, cx),
        Appearance::Light => Theme::change(ThemeMode::Light, None, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
    let size = settings.text_size.pixels();
    Theme::update(cx, |theme| {
        theme.font_size = px(size);
        // Values and the console are monospaced, a little smaller.
        theme.mono_font_size = px(size - 3.);
    });
}

/// Follows the system into light or dark, when the settings say to.
pub fn system_appearance_changed(window: &mut Window, cx: &mut App) {
    if get(cx).appearance == Appearance::System {
        Theme::sync_system_appearance(Some(window), cx);
    }
}

/// Opens the settings in the window in front.
pub fn open_in_active_window(cx: &mut App) {
    let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    handle
        .update(cx, |_, window, cx| {
            // Not over another dialog, the settings included.
            if !window.has_active_dialog(cx) {
                open(window, cx);
            }
        })
        .ok();
}

/// Opens the settings dialog in `window`.
pub fn open(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, cx| {
        dialog.title("Settings").w(px(760.)).child(
            div().h(px(440.)).child(
                SettingsView::new("settings")
                    .sidebar_width(px(180.))
                    .pages(pages(cx)),
            ),
        )
    });
}

fn pages(cx: &App) -> Vec<SettingPage> {
    let mut general = SettingPage::new("General").group(
        SettingGroup::new().title("Appearance").items([
            SettingItem::new(
                "Theme",
                choice(
                    &Appearance::ALL,
                    Appearance::label,
                    |s| s.appearance,
                    |s, v| s.appearance = v,
                ),
            )
            .description("System follows macOS, Windows or your desktop as they change."),
            SettingItem::new(
                "Text size",
                choice(
                    &TextSize::ALL,
                    TextSize::label,
                    |s| s.text_size,
                    |s, v| s.text_size = v,
                ),
            ),
        ]),
    );
    if let Some(updater) = Updater::global(cx) {
        let set = updater.clone();
        general = general.group(
            SettingGroup::new().title("Updates").item(
                SettingItem::new(
                    "Check for updates automatically",
                    SettingField::switch(
                        move |cx| updater.read(cx).automatic(),
                        move |on, cx| set.update(cx, |updater, cx| updater.set_automatic(on, cx)),
                    ),
                )
                .description("On start and once a day. redcrab asks before it installs anything."),
            ),
        );
    }
    let keys = SettingPage::new("Keys").group(
        SettingGroup::new().title("Key list").items([
            SettingItem::new(
                "Load at most",
                choice(
                    &KEY_LIMITS,
                    |n| format!("{} keys", format::count(n as u64)),
                    |s| s.key_limit,
                    |s, v| s.key_limit = v,
                ),
            )
            .description(
                "A scan stops there and says so; a filter finds keys past it. \
                 Each key takes about 60 bytes. Applies from the next refresh.",
            ),
            SettingItem::new(
                "Keys per SCAN",
                choice(
                    &SCAN_COUNTS,
                    |n| format::count(n as u64),
                    |s| s.scan_count,
                    |s, v| s.scan_count = v,
                ),
            )
            .description(
                "The COUNT of each SCAN round trip. Smaller keeps each one short on a \
                 busy server; larger loads faster. Applies from the next refresh.",
            ),
        ]),
    );
    vec![general, keys]
}

/// A dropdown over `options`, each saved as its position in the list and
/// shown as `label` says.
fn choice<T: Copy + PartialEq + 'static, L: Into<SharedString>>(
    options: &'static [T],
    label: fn(T) -> L,
    read: fn(&Settings) -> T,
    write: fn(&mut Settings, T),
) -> SettingField<SharedString> {
    let items = options
        .iter()
        .enumerate()
        .map(|(ix, &option)| (SharedString::from(ix.to_string()), label(option).into()))
        .collect();
    SettingField::dropdown(
        items,
        move |cx| {
            let current = read(&get(cx));
            let ix = options.iter().position(|&o| o == current).unwrap_or(0);
            SharedString::from(ix.to_string())
        },
        move |ix, cx| {
            if let Some(&value) = ix.parse::<usize>().ok().and_then(|ix| options.get(ix)) {
                change(cx, |settings| write(settings, value));
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_kit::component::ActiveTheme as _;
    use redcrab_core::settings::{Appearance, Settings, TextSize};

    #[gpui::test]
    fn changes_apply_and_are_saved(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("redcrab-gui-settings-{}", std::process::id()));
        let file = dir.join("settings.json");
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::init_with(Settings::default(), Some(file.clone()), cx);
            super::change(cx, |s| {
                s.appearance = Appearance::Dark;
                s.text_size = TextSize::Large;
            });
            assert!(cx.theme().is_dark());
            assert_eq!(cx.theme().font_size, gpui_kit::px(18.));
            super::change(cx, |s| s.appearance = Appearance::Light);
            assert!(!cx.theme().is_dark());
            // The text size stays through a theme change.
            assert_eq!(cx.theme().font_size, gpui_kit::px(18.));
        });
        let saved = Settings::load_from(&file);
        assert_eq!(
            (saved.appearance, saved.text_size),
            (Appearance::Light, TextSize::Large)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    struct Host;

    impl gpui_kit::Render for Host {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            _: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            gpui_kit::div()
        }
    }

    #[gpui::test]
    fn the_dialog_draws_and_closes_with_escape(cx: &mut TestAppContext) {
        use gpui_kit::{AppContext as _, component::WindowExt as _};

        cx.update(|cx| {
            gpui_kit::init(cx);
            super::init_with(Settings::default(), None, cx);
        });
        let handle = cx.update(|cx| {
            gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| Host)
            })
            .unwrap()
            .0
        });
        cx.update_window(handle, |_, window, cx| super::open(window, cx))
            .unwrap();
        // A keystroke draws a frame first: the pages and fields render.
        cx.simulate_keystrokes(handle, "tab");
        let open = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        };
        assert!(open(cx));
        cx.simulate_keystrokes(handle, "escape");
        cx.run_until_parked();
        assert!(!open(cx));
    }

    #[gpui::test]
    fn an_unchanged_setting_is_not_saved(cx: &mut TestAppContext) {
        let dir =
            std::env::temp_dir().join(format!("redcrab-gui-settings-same-{}", std::process::id()));
        let file = dir.join("settings.json");
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::init_with(Settings::default(), Some(file.clone()), cx);
            super::change(cx, |s| s.appearance = Appearance::System);
        });
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
