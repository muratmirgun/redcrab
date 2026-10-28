//! The command palette (⌘K): every command of the window in one searchable
//! list, with its shortcut when it has one. [`crate::app::RedcrabApp`]
//! decides which commands the window has; this shows them and runs the one
//! picked, after the palette closed, so that it acts on the window as it
//! was before the palette opened.

use std::rc::Rc;

use gpui_kit::{
    Action, App, AppContext as _, AsKeystroke as _, FocusHandle, Keystroke, ParentElement as _,
    SharedString, Styled as _, Window,
    component::{
        ActiveTheme as _, Icon, IndexPath, Sizable as _, WindowExt as _,
        command::{Command, CommandGroup, CommandItem, CommandState},
        h_flex,
        kbd::Kbd,
    },
    div,
    prelude::FluentBuilder as _,
    px,
};

pub type Run = Rc<dyn Fn(&mut Window, &mut App)>;

/// One command: what the palette shows and what it runs.
pub struct PaletteCommand {
    group: &'static str,
    label: SharedString,
    icon: Option<gpui_kit::assets::IconName>,
    keywords: &'static [&'static str],
    keystroke: Option<Keystroke>,
    run: Run,
}

impl PaletteCommand {
    pub fn new(
        group: &'static str,
        label: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            group,
            label: label.into(),
            icon: None,
            keywords: &[],
            keystroke: None,
            run: Rc::new(run),
        }
    }

    /// Dispatches `action` where `target` is, as its shortcut would there,
    /// whatever has focus; and shows that shortcut.
    pub fn action(
        group: &'static str,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
        target: FocusHandle,
        window: &Window,
    ) -> Self {
        let keystroke = window
            .highest_precedence_binding_for_action_in(action.as_ref(), &target)
            .and_then(|binding| {
                binding
                    .keystrokes()
                    .first()
                    .map(|key| key.as_keystroke().clone())
            });
        Self {
            keystroke,
            ..Self::new(group, label, move |window, cx| {
                target.dispatch_action(action.as_ref(), window, cx)
            })
        }
    }

    /// Dispatches `action` to the app, as its shortcut would anywhere;
    /// for the app-wide actions (quit, new window).
    pub fn app_action(
        group: &'static str,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
        window: &Window,
    ) -> Self {
        let keystroke = window
            .highest_precedence_binding_for_action(action.as_ref())
            .and_then(|binding| {
                binding
                    .keystrokes()
                    .first()
                    .map(|key| key.as_keystroke().clone())
            });
        Self {
            keystroke,
            ..Self::new(group, label, move |window, cx| {
                window.dispatch_action(action.boxed_clone(), cx)
            })
        }
    }

    pub fn icon(mut self, icon: gpui_kit::assets::IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Other words the search finds it by.
    pub fn keywords(mut self, keywords: &'static [&'static str]) -> Self {
        self.keywords = keywords;
        self
    }

    fn item(&self) -> CommandItem {
        let label = self.label.clone();
        let icon = self.icon;
        let keystroke = self.keystroke.clone();
        CommandItem::new()
            .label(self.label.clone())
            .keywords(self.keywords.iter().copied())
            .child(move |_, cx| {
                let muted = cx.theme().muted_foreground;
                h_flex()
                    .flex_1()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(16.)).when_some(icon, |this, icon| {
                        this.child(Icon::new(icon).small().text_color(muted))
                    }))
                    .child(label.clone())
                    .child(div().flex_1())
                    .when_some(keystroke.clone(), |this, key| this.child(Kbd::new(key)))
            })
    }
}

/// Opens the palette over `window` with `commands`, grouped in the order
/// their groups first appear.
pub fn open(commands: Vec<PaletteCommand>, window: &mut Window, cx: &mut App) {
    let state = cx.new(|cx| CommandState::new(window, cx));
    let mut groups: Vec<(&'static str, Vec<usize>)> = Vec::new();
    for (ix, command) in commands.iter().enumerate() {
        match groups.iter_mut().find(|(group, _)| *group == command.group) {
            Some((_, members)) => members.push(ix),
            None => groups.push((command.group, vec![ix])),
        }
    }
    let commands: Rc<Vec<PaletteCommand>> = Rc::new(commands);
    let groups = Rc::new(groups);
    let focus = state.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let commands = commands.clone();
        let groups_for_confirm = groups.clone();
        let palette = groups.iter().fold(
            Command::new(&state)
                .bordered(false)
                .max_h(px(380.))
                .placeholder("Type a command…"),
            |palette, (group, members)| {
                palette.group(
                    CommandGroup::new()
                        .label(*group)
                        .items(members.iter().map(|&ix| commands[ix].item())),
                )
            },
        );
        let confirmed = commands.clone();
        let palette = palette
            .on_confirm(move |path: IndexPath, window, cx| {
                let Some(&ix) = groups_for_confirm
                    .get(path.section)
                    .and_then(|(_, members)| members.get(path.row))
                else {
                    return;
                };
                let run = confirmed[ix].run.clone();
                window.close_dialog(cx);
                // After the dialog is gone and focus is back where it was.
                window.defer(cx, move |window, cx| run(window, cx));
            })
            .on_cancel(|window, cx| window.close_dialog(cx));
        dialog.w(px(560.)).p_0().close_button(false).child(palette)
    });
    // Once the dialog took focus for itself.
    window.defer(cx, move |window, cx| {
        focus.update(cx, |state, cx| state.focus(window, cx));
    });
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::TestAppContext;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Context, IntoElement, Render, Window, WindowOptions,
        component::WindowExt as _, div,
    };

    use super::{PaletteCommand, open};

    struct Host;

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn a_picked_command_runs_once_the_palette_is_gone(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let handle: AnyWindowHandle = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Host))
                .unwrap()
                .0
        });
        let ran = Rc::new(RefCell::new(Vec::new()));
        let commands = ["Refresh keys", "Toggle console", "New window"]
            .into_iter()
            .enumerate()
            .map(|(ix, label)| {
                let ran = ran.clone();
                let group = if ix < 2 { "Keys" } else { "redcrab" };
                PaletteCommand::new(group, label, move |window, cx| {
                    // The palette is closed by the time a command runs.
                    ran.borrow_mut().push((label, window.has_active_dialog(cx)));
                })
            })
            .collect();
        cx.update_window(handle, |_, window, cx| open(commands, window, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(
            cx.update_window(handle, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        );
        // Typing narrows the list; Enter runs the first match.
        cx.simulate_input(handle, "console");
        cx.simulate_keystrokes(handle, "enter");
        cx.run_until_parked();
        assert_eq!(*ran.borrow(), [("Toggle console", false)]);
        assert!(
            !cx.update_window(handle, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        );
    }
}
