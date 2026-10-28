//! The app's keyboard shortcuts, with the platform's primary modifier: ⌘
//! on macOS, Ctrl on Linux and Windows. GPUI reads `cmd` as the Super /
//! Windows key off macOS, which the system keeps for itself there (Win+S
//! opens Windows Search, Win+R the Run box), so the bindings name the
//! modifier per platform, and so do the hints in tooltips.

use gpui_kit::KeyBinding;

use crate::{DeleteKey, FocusSearch, NewKey, NewWindow, Quit, Refresh, SaveValue, ToggleConsole};

/// A keystroke with the platform's primary modifier, as the `&'static
/// str` GPUI's bindings take: `primary!("s")` is `cmd-s` on macOS and
/// `ctrl-s` elsewhere.
#[cfg(target_os = "macos")]
macro_rules! primary {
    ($keys:literal) => {
        concat!("cmd-", $keys)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! primary {
    ($keys:literal) => {
        concat!("ctrl-", $keys)
    };
}
pub(crate) use primary;

/// How a shortcut of the primary modifier and `key` reads in a tooltip:
/// "⌘S" and "⇧⌘N" on macOS, "Ctrl+S" and "Ctrl+Shift+N" elsewhere.
pub fn hint(key: &str, shift: bool) -> String {
    if cfg!(target_os = "macos") {
        format!("{}⌘{key}", if shift { "⇧" } else { "" })
    } else {
        format!("Ctrl+{}{key}", if shift { "Shift+" } else { "" })
    }
}

/// The app-wide and workspace shortcuts. Inside a text input, the input's
/// own bindings for the same keys (delete a word, find) come first.
pub fn bindings() -> Vec<KeyBinding> {
    const WORKSPACE: Option<&str> = Some("Workspace");
    vec![
        KeyBinding::new(primary!("q"), Quit, None),
        KeyBinding::new(primary!("n"), NewWindow, None),
        KeyBinding::new(primary!("r"), Refresh, WORKSPACE),
        KeyBinding::new(primary!("f"), FocusSearch, WORKSPACE),
        KeyBinding::new(primary!("backspace"), DeleteKey, WORKSPACE),
        KeyBinding::new(primary!("j"), ToggleConsole, WORKSPACE),
        KeyBinding::new(primary!("s"), SaveValue, WORKSPACE),
        KeyBinding::new(primary!("shift-n"), NewKey, WORKSPACE),
    ]
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Action, KeyContext, Keymap, Keystroke};

    use super::*;

    fn bound(key: &str, contexts: &[&str]) -> Option<Box<dyn Action>> {
        // Parsing errors borrow the keystroke for good.
        let key: &'static str = Box::leak(key.to_string().into_boxed_str());
        let keymap = Keymap::new(bindings());
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
    fn shortcuts_use_the_platform_modifier() {
        let workspace = ["Workspace"];
        // macOS: ⌘. Elsewhere Ctrl, never the Super / Windows key.
        let (primary, other) = if cfg!(target_os = "macos") {
            ("cmd", "ctrl")
        } else {
            ("ctrl", "cmd")
        };
        for (key, saves) in [(primary, true), (other, false)] {
            assert_eq!(
                is::<SaveValue>(bound(&format!("{key}-s"), &workspace)),
                saves,
                "{key}-s"
            );
        }
        assert!(is::<Refresh>(bound(&format!("{primary}-r"), &workspace)));
        assert!(is::<NewKey>(bound(
            &format!("{primary}-shift-n"),
            &workspace
        )));
        assert!(is::<DeleteKey>(bound(
            &format!("{primary}-backspace"),
            &workspace
        )));
        assert!(is::<Quit>(bound(&format!("{primary}-q"), &[])));
        // Workspace keys only in the workspace.
        assert!(bound(&format!("{primary}-s"), &["ConnectView"]).is_none());

        assert_eq!(primary!("shift-n"), format!("{primary}-shift-n"));
        let parsed = Keystroke::parse(primary!("s")).unwrap();
        assert_eq!(
            (parsed.modifiers.platform, parsed.modifiers.control),
            (cfg!(target_os = "macos"), !cfg!(target_os = "macos"))
        );
    }

    #[test]
    fn hints_read_as_the_platform_writes_them() {
        if cfg!(target_os = "macos") {
            assert_eq!(hint("S", false), "⌘S");
            assert_eq!(hint("N", true), "⇧⌘N");
        } else {
            assert_eq!(hint("S", false), "Ctrl+S");
            assert_eq!(hint("N", true), "Ctrl+Shift+N");
        }
    }
}
