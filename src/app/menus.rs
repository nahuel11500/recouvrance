//! The macOS menu bar, and the app's actions that need no window.

use gpui_kit::base::input;
use gpui_kit::*;

use super::{
    AppView, CloseTab, CloseWindow, FocusFilter, GoAssistant, GoBack, GoCluster, GoForward, GoHelm, GoProblems,
    GoSettings, GoWorkloads, HideApp, HideOtherApps, MAC, MinimizeWindow, NewTab, NewWindow, NextTab, OpenHelp,
    OpenPalette, PrevTab, Quit, RefreshPage, ReportIssue, Route, ShowAbout, ShowAllApps, ShowShortcuts,
    ToggleAssistant, ToggleSidebar, ZoomIn, ZoomOut, ZoomReset, ZoomWindow, open_main_window,
};

const HELP_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/docs/README.md");
const ISSUES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues");

/// Handle the actions that need no window and, on macOS, set the menu bar. Call it after
/// `bind_keys`: the menu bar shows the keys bound when it is set.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &NewWindow, cx| {
        open_main_window(cx);
    });
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOtherApps, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAllApps, cx| cx.unhide_other_apps());
    cx.on_action(|_: &OpenHelp, cx| {
        crate::forwards::open_web_link(HELP_URL, cx);
    });
    cx.on_action(|_: &ReportIssue, cx| {
        crate::forwards::open_web_link(ISSUES_URL, cx);
    });
    // A window handles these itself. With none open (macOS keeps running then), one opens.
    cx.on_action(|_: &ShowAbout, cx| in_new_window(cx, AppView::show_about));
    cx.on_action(|_: &GoSettings, cx| in_new_window(cx, |app, cx| app.navigate(Route::Settings, cx)));
    if MAC {
        cx.set_menus(app_menus());
    }
}

/// When no window is open, open one and show `page` there.
fn in_new_window(cx: &mut App, page: impl FnOnce(&mut AppView, &mut Context<AppView>)) {
    if cx.windows().is_empty()
        && let Some(app) = open_main_window(cx)
    {
        app.update(cx, page);
    }
}

/// The menu bar. gpui shows each item's key from the keymap: an action's first binding.
fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new("Recouvrance").items([
            MenuItem::action("About Recouvrance", ShowAbout),
            MenuItem::separator(),
            MenuItem::action("Settings…", GoSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Recouvrance", HideApp),
            MenuItem::action("Hide Others", HideOtherApps),
            MenuItem::action("Show All", ShowAllApps),
            MenuItem::separator(),
            MenuItem::action("Quit Recouvrance", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Window", NewWindow),
            MenuItem::action("New Tab", NewTab),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        // The text fields' own actions: they act on the field with focus, and Copy also copies
        // the text selected on a page.
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find", FocusFilter),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", OpenPalette),
            MenuItem::separator(),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Assistant", ToggleAssistant),
            MenuItem::separator(),
            MenuItem::action("Refresh", RefreshPage),
            MenuItem::separator(),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::action("Actual Size", ZoomReset),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Back", GoBack),
            MenuItem::action("Forward", GoForward),
            MenuItem::separator(),
            MenuItem::action("Problems", GoProblems),
            MenuItem::action("Cluster Overview", GoCluster),
            MenuItem::action("Workloads Overview", GoWorkloads),
            MenuItem::action("Helm Releases", GoHelm),
            MenuItem::action("Assistant", GoAssistant),
        ]),
        // gpui gives macOS the menu named "Window", which lists the open windows in it.
        Menu::new("Window").items([
            MenuItem::action("Minimize", MinimizeWindow),
            MenuItem::action("Zoom", ZoomWindow),
            MenuItem::separator(),
            MenuItem::action("Show Previous Tab", PrevTab),
            MenuItem::action("Show Next Tab", NextTab),
        ]),
        Menu::new("Help").items([
            MenuItem::action("Keyboard Shortcuts", ShowShortcuts),
            MenuItem::separator(),
            MenuItem::action("Recouvrance Help", OpenHelp),
            MenuItem::action("Report an Issue", ReportIssue),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use gpui_kit::base::input;
    use gpui_kit::{Action, KeyContext, Keymap, MenuItem, OsAction};

    use super::app_menus;

    /// Typing never runs a menu item: on macOS each key the menu bar shows takes ⌘ or ⌃ or is a
    /// function key. A chord (`g p`) shows no key.
    #[test]
    fn menu_keys_never_take_typing() {
        let keymap = Keymap::new(crate::app::key_bindings_on(true));
        // gpui-macos shows the first binding that applies in this context, else the first one.
        let mut context = KeyContext::new_with_defaults();
        for name in ["Workspace", "Pane", "Editor"] {
            context.add(name);
        }
        let contexts = [context];
        let mut shown = 0;
        for menu in app_menus() {
            for item in &menu.items {
                let MenuItem::Action { name, action, .. } = item else { continue };
                let bindings: Vec<_> = keymap.bindings_for_action(action.as_ref()).collect();
                let first = bindings.iter().find(|b| b.predicate().is_none_or(|p| p.eval(&contexts)));
                let Some(binding) = first.or(bindings.first()) else { continue };
                let [stroke] = binding.keystrokes() else { continue };
                let (m, key) = (stroke.modifiers(), stroke.key());
                let function_key =
                    key.strip_prefix('f').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
                assert!(m.platform || m.control || function_key, "{name} would show {key:?} and take it from typing");
                shown += 1;
            }
        }
        assert!(shown >= 20, "only {shown} menu items show a key");
    }

    /// The Edit menu sends the text fields' own actions, as the OS's own edit commands, so it
    /// acts on the field with focus; macOS lists the windows in the menu named "Window".
    #[test]
    fn the_edit_menu_edits_the_focused_field() {
        let menus = app_menus();
        let edit = menus.iter().find(|m| m.name == "Edit").expect("an Edit menu");
        let expected: [(&dyn Action, OsAction); 6] = [
            (&input::Undo, OsAction::Undo),
            (&input::Redo, OsAction::Redo),
            (&input::Cut, OsAction::Cut),
            (&input::Copy, OsAction::Copy),
            (&input::Paste, OsAction::Paste),
            (&input::SelectAll, OsAction::SelectAll),
        ];
        for (action, os) in expected {
            let found = edit.items.iter().any(|item| {
                matches!(item, MenuItem::Action { action: a, os_action: Some(o), .. } if a.partial_eq(action) && *o == os)
            });
            assert!(found, "Edit lacks {}", action.name());
        }
        assert!(menus.iter().any(|m| m.name == "Window"));
    }
}
