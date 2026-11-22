//! The keyboard shortcuts: how each platform names a key, and the list shown on `?` and in
//! Settings → Shortcuts, both made from `commands::COMMANDS`. docs/features.md has the same table
//! (a test keeps them equal), so the panel and the docs can't disagree.

use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

use super::MAC;
use super::commands;

/// Single keys (`?`, `g`, `j`, `k`) only act outside text fields and terminals, where they
/// are typed instead.
pub(super) const OUTSIDE_TEXT: &str = "App && !Input && !Terminal";

/// The same for keys that act on the focused table.
pub(super) const TABLE_OUTSIDE_TEXT: &str = "DataTable && !Input";

/// `text` with each key in braces named as this platform does: `{secondary-k}` reads Ctrl+K on
/// Linux and ⌘K on macOS. A key is written as gpui binds it (`secondary` is ⌘ on macOS and Ctrl
/// elsewhere); `{alt-left|cmd-[}` is a different key on each, Linux's first; `{g p}` is two keys
/// pressed one after the other.
pub(crate) fn with_keys(text: &str) -> String {
    with_keys_on(text, MAC)
}

fn with_keys_on(text: &str, mac: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('{') {
        let Some((key, after)) = after.split_once('}') else { break };
        out.push_str(before);
        // `{g p}` is `g`, then `p`.
        let strokes: Vec<String> = key_for(key, mac).split(' ').map(|k| key_on(k, mac)).collect();
        out.push_str(&strokes.join(" then "));
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The keys a text names in braces, as gpui binds them on `mac` or Linux.
#[cfg(test)]
fn keys_in(text: &str, mac: bool) -> Vec<String> {
    let key = |k: &str| key_for(k, mac).replace("secondary-", if mac { "cmd-" } else { "ctrl-" });
    text.split('{').skip(1).filter_map(|part| part.split_once('}')).map(|(k, _)| key(k)).collect()
}

/// The actions bound to `keys` (keystrokes separated by spaces, as gpui binds them) on `mac` or
/// Linux, in any context.
#[cfg(test)]
fn actions_of(keys: &str, mac: bool) -> Vec<&'static str> {
    let strokes: Vec<Keystroke> = keys.split(' ').map(|k| Keystroke::parse(k).unwrap()).collect();
    let keymap = Keymap::new(super::key_bindings_on(mac));
    keymap
        .bindings()
        .filter(|b| b.keystrokes().len() == strokes.len())
        .filter(|b| b.keystrokes().iter().zip(&strokes).all(|(k, s)| k.inner() == s))
        .map(|b| b.action().name())
        .collect()
}

/// The platform's side of `linux|mac`.
fn key_for(key: &str, mac: bool) -> &str {
    match key.split_once('|') {
        Some((linux, macos)) => {
            if mac {
                macos
            } else {
                linux
            }
        }
        None => key,
    }
}

/// One keystroke as macOS (`mac`) or Linux names it: ⇧⌘N or Ctrl+Shift+N, ⌥← or Alt+←.
fn key_on(keystroke: &str, mac: bool) -> String {
    // The key follows the last `-`, unless it is `-` itself (`secondary--`).
    let (modifiers, key) = match keystroke.strip_suffix("--") {
        Some(modifiers) => (modifiers, "-"),
        None => keystroke.rsplit_once('-').filter(|(_, key)| !key.is_empty()).unwrap_or(("", keystroke)),
    };
    let has = |name: &str| modifiers.split('-').any(|m| m == name);
    let secondary = has("secondary");
    let (ctrl, alt, shift) = (has("ctrl") || (secondary && !mac), has("alt"), has("shift"));
    let cmd = has("cmd") || (secondary && mac);
    let name = match key {
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "escape" => "Esc".to_string(),
        "enter" if mac => "Return".to_string(),
        "pageup" => "PageUp".to_string(),
        "pagedown" => "PageDown".to_string(),
        // A letter alone stays as typed (`j`); with a modifier it reads as on the key (Ctrl+K).
        k if k.chars().count() == 1 && !(ctrl || alt || shift || cmd) => k.to_string(),
        k => match k.chars().next() {
            Some(first) => first.to_uppercase().chain(k[first.len_utf8()..].chars()).collect(),
            None => String::new(),
        },
    };
    let last = std::iter::once(name.as_str());
    if mac {
        // Apple's order: ⌃⌥⇧⌘, then the key, with nothing between.
        let symbols = [(ctrl, "⌃"), (alt, "⌥"), (shift, "⇧"), (cmd, "⌘")];
        symbols.into_iter().filter(|(on, _)| *on).map(|(_, s)| s).chain(last).collect()
    } else {
        let words = [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift"), (cmd, "Super")];
        words.into_iter().filter(|(on, _)| *on).map(|(_, w)| w).chain(last).collect::<Vec<_>>().join("+")
    }
}

/// Keys in a small box, named as this platform does (see [`with_keys`]).
pub(crate) fn key_cap(keys: &str, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .text_xs()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .font_family(theme.mono_font_family.clone())
        .whitespace_nowrap()
        .child(with_keys(keys))
}

/// Every command with keys: the keys, what it does and where. The `?` panel and Settings →
/// Shortcuts both show this.
pub(crate) fn shortcut_rows(cx: &App) -> AnyElement {
    let theme = cx.theme();
    v_flex()
        .children(commands::with_shortcut().map(|c| {
            h_flex()
                .gap_4()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(200.)).flex_shrink_0().child(key_cap(c.keys, cx)))
                .child(div().flex_1().min_w_0().text_sm().child(c.described().replace('`', "")))
                .child(
                    div()
                        .w(px(130.))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(c.scope.label()),
                )
        }))
        .into_any_element()
}

/// Open the list of shortcuts.
pub(super) fn open_panel(window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, |alert, _, cx| {
        alert
            .title("Keyboard shortcuts")
            .description(with_keys(
                "Single keys (?, g, j, k) work when no text field or terminal has focus. Every command, with a \
                 key or without, is in the command palette ({secondary-k}).",
            ))
            .width(px(820.))
            .child(shortcut_rows(cx))
            .ok_text("Close")
    });
}

/// A binding as `ctrl-cmd-alt-shift-key` (⌘ where Ctrl would be, so a Linux binding becomes its
/// macOS one by a replace), its action and its context.
#[cfg(test)]
fn describe(binding: &KeyBinding) -> (String, &'static str, Option<String>) {
    let stroke = |k: &KeybindingKeystroke| {
        let m = k.modifiers();
        let names = [(m.control, "ctrl-"), (m.platform, "cmd-"), (m.alt, "alt-"), (m.shift, "shift-")];
        let mods: String = names.into_iter().filter(|(on, _)| *on).map(|(_, n)| n).collect();
        mods + k.key()
    };
    let keys = binding.keystrokes().iter().map(stroke).collect::<Vec<_>>().join(" ");
    (keys, binding.action().name(), binding.predicate().as_deref().map(ToString::to_string))
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Action, KeyBinding, KeyContext, Keymap, Keystroke};
    use proptest::prelude::*;

    use super::{actions_of, describe, keys_in, with_keys_on};
    use crate::app::commands::{COMMANDS, Scope, with_shortcut};
    use crate::app::{self as app, key_bindings_on};

    /// The rows of the shortcut table in docs/features.md: the keys on Linux, on macOS, what
    /// they do, then where.
    fn docs_rows() -> Vec<[String; 4]> {
        let docs = include_str!("../../docs/features.md");
        docs.lines()
            .skip_while(|l| l.trim() != "| Linux | macOS | Command | Where |")
            .skip(2)
            .take_while(|l| l.starts_with('|'))
            .map(|line| {
                let inner = line.trim().trim_start_matches("| ").trim_end_matches(" |");
                let mut cells = inner.splitn(4, " | ").map(String::from);
                std::array::from_fn(|_| cells.next().expect("a row has four cells"))
            })
            .collect()
    }

    #[test]
    fn the_docs_list_every_shortcut_of_the_panel() {
        let docs = docs_rows();
        let code: Vec<[String; 4]> = with_shortcut()
            .map(|c| {
                [with_keys_on(c.keys, false), with_keys_on(c.keys, true), c.described(), c.scope.label().to_string()]
            })
            .collect();
        for row in &code {
            assert!(docs.contains(row), "docs/features.md lacks the shortcut {row:?}");
        }
        assert_eq!(docs, code, "docs/features.md and the `?` panel list different shortcuts");
    }

    #[test]
    fn keys_read_as_each_platform_names_them() {
        for (text, linux, mac) in [
            ("{secondary-shift-n}", "Ctrl+Shift+N", "⇧⌘N"),
            ("{ctrl-tab}", "Ctrl+Tab", "⌃Tab"),
            ("{secondary--} / {secondary-=}", "Ctrl+- / Ctrl+=", "⌘- / ⌘="),
            ("{alt-left|cmd-[}", "Alt+←", "⌘["),
            ("{j} or {enter}", "j or Enter", "j or Return"),
            ("Close tab ({secondary-w})", "Close tab (Ctrl+W)", "Close tab (⌘W)"),
            ("{cmd-alt-h}", "Alt+Super+H", "⌥⌘H"),
            ("{g p}", "g then p", "g then p"),
            // A brace left open is text.
            ("a { b", "a { b", "a { b"),
        ] {
            assert_eq!(with_keys_on(text, false), linux, "{text} on Linux");
            assert_eq!(with_keys_on(text, true), mac, "{text} on macOS");
        }
    }

    proptest! {
        /// Text without braces names no key: it comes back as it was, on both platforms.
        #[test]
        fn text_without_braces_is_kept(text in "[^{}]*", mac: bool) {
            prop_assert_eq!(with_keys_on(&text, mac), text);
        }
    }

    /// The panel and the docs never name a key that does nothing: each key in braces is bound,
    /// on Linux and on macOS, and to the command's own action when it has one. A shell's paste
    /// is the terminal's own key, not a binding.
    #[test]
    fn the_panel_names_only_bound_keys() {
        let named: usize = COMMANDS.iter().map(|c| keys_in(c.keys, false).len()).sum();
        assert!(named > 25, "the list names {named} keys in braces");
        for c in COMMANDS.iter().filter(|c| c.scope != Scope::Shell) {
            for mac in [false, true] {
                for keys in keys_in(c.keys, mac) {
                    let actions = actions_of(&keys, mac);
                    assert!(!actions.is_empty(), "{}: {keys} isn't bound (macOS: {mac})", c.name);
                    if let Some(action) = c.action {
                        let name = action().name();
                        assert!(
                            actions.contains(&name),
                            "{}: {keys} runs {actions:?}, not {name} (macOS: {mac})",
                            c.name
                        );
                    }
                }
            }
        }
    }

    /// Every key the window binds is in the list, so `?` and Settings → Shortcuts show them all;
    /// only the keys every Mac app has are left to its menu bar.
    #[test]
    fn every_bound_action_is_listed() {
        let menu_bar_only = ["Quit", "CloseWindow", "MinimizeWindow", "HideApp", "HideOtherApps"];
        for mac in [false, true] {
            let listed: Vec<&str> =
                COMMANDS.iter().flat_map(|c| keys_in(c.keys, mac)).flat_map(|k| actions_of(&k, mac)).collect();
            for binding in key_bindings_on(mac) {
                let name = binding.action().name();
                let short = name.rsplit("::").next().unwrap_or(name);
                if short == "NoAction" || menu_bar_only.contains(&short) {
                    continue;
                }
                assert!(listed.contains(&name), "{name} is bound but no command lists it (macOS: {mac})");
            }
        }
    }

    /// What the palette lists does something when picked, every command is in a list, and
    /// each is listed once.
    #[test]
    fn palette_commands_run_an_action() {
        for c in COMMANDS {
            assert!(!c.palette || c.action.is_some(), "{} is in the palette but runs nothing", c.name);
            assert!(c.palette || !c.keys.is_empty(), "{} is in no list", c.name);
        }
        let mut names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COMMANDS.len(), "two commands share a name");
    }

    /// Linux keeps the keys it had.
    #[test]
    fn linux_keeps_its_keys() {
        use app::{
            CloseTab, EscapeBack, FocusFilter, GoAssistant, GoBack, GoCluster, GoForward, GoHelm, GoProblems,
            GoSettings, GoWorkloads, NewTab, NewWindow, NextTab, OpenPalette, OpenSelected, PrevTab, RefreshPage,
            ShowShortcuts, ToggleAssistant, ToggleSidebar, ZoomIn, ZoomOut, ZoomReset,
        };
        use gpui_kit::NoAction;
        use gpui_kit::base::actions::{SelectDown, SelectUp};
        let before = [
            KeyBinding::new("ctrl-k", OpenPalette, None),
            KeyBinding::new("ctrl-shift-n", NewWindow, None),
            KeyBinding::new("ctrl-t", NewTab, None),
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PrevTab, None),
            KeyBinding::new("ctrl-pageup", PrevTab, None),
            KeyBinding::new("ctrl-b", ToggleSidebar, None),
            KeyBinding::new("ctrl-j", ToggleAssistant, None),
            KeyBinding::new("f5", RefreshPage, None),
            KeyBinding::new("ctrl-r", RefreshPage, None),
            KeyBinding::new("ctrl-=", ZoomIn, None),
            KeyBinding::new("ctrl-+", ZoomIn, None),
            KeyBinding::new("ctrl-shift-=", ZoomIn, None),
            KeyBinding::new("ctrl--", ZoomOut, None),
            KeyBinding::new("ctrl-0", ZoomReset, None),
            KeyBinding::new("alt-left", GoBack, None),
            KeyBinding::new("alt-right", GoForward, None),
            KeyBinding::new("ctrl-f", FocusFilter, Some("App")),
            KeyBinding::new("escape", EscapeBack, Some("App")),
            KeyBinding::new("/", FocusFilter, Some("DataTable")),
            KeyBinding::new("enter", OpenSelected, Some("DataTable")),
            KeyBinding::new("f5", NoAction, Some("Terminal")),
            KeyBinding::new("?", ShowShortcuts, Some(super::OUTSIDE_TEXT)),
            KeyBinding::new("g p", GoProblems, Some(super::OUTSIDE_TEXT)),
            KeyBinding::new("g o", GoCluster, Some(super::OUTSIDE_TEXT)),
            KeyBinding::new("g w", GoWorkloads, Some(super::OUTSIDE_TEXT)),
            KeyBinding::new("g h", GoHelm, Some(super::OUTSIDE_TEXT)),
            KeyBinding::new("g i", GoAssistant, Some(super::OUTSIDE_TEXT)),
            KeyBinding::new("g s", GoSettings, Some(super::OUTSIDE_TEXT)),
            KeyBinding::new("j", SelectDown, Some(super::TABLE_OUTSIDE_TEXT)),
            KeyBinding::new("k", SelectUp, Some(super::TABLE_OUTSIDE_TEXT)),
        ];
        let sorted = |bindings: &[KeyBinding]| {
            let mut all: Vec<_> = bindings.iter().map(describe).collect();
            all.sort();
            all
        };
        assert_eq!(sorted(&key_bindings_on(false)), sorted(&before));
    }

    /// Ctrl+Tab and Ctrl+Page Up/Down switch tabs with Ctrl on both platforms, as in browsers.
    fn switches_tabs(keys: &str) -> bool {
        ["ctrl-tab", "ctrl-shift-tab", "ctrl-pageup", "ctrl-pagedown"].contains(&keys)
    }

    /// Every key the app takes with Ctrl on Linux takes ⌘ on macOS (Back and Forward: ⌘[ and ⌘]),
    /// and macOS takes no other Ctrl key, so in a terminal they reach the shell. Linux takes no
    /// ⌘ (Super) key: its desktop uses them.
    #[test]
    fn macos_takes_cmd_where_linux_takes_ctrl() {
        let mac: Vec<_> = key_bindings_on(true).iter().map(describe).collect();
        for binding in key_bindings_on(false) {
            let (keys, action, context) = describe(&binding);
            assert!(!keys.contains("cmd-"), "Linux binds {keys}");
            let on_mac = match keys.as_str() {
                "alt-left" => "cmd-[".to_string(),
                "alt-right" => "cmd-]".to_string(),
                k if switches_tabs(k) => k.to_string(),
                k => k.replace("ctrl-", "cmd-"),
            };
            let twin = (on_mac.clone(), action, context);
            assert!(mac.contains(&twin), "{keys} ({action}) is {on_mac} on macOS, which isn't bound");
        }
        for (keys, action, _) in &mac {
            assert!(!keys.contains("ctrl-") || switches_tabs(keys), "macOS binds {keys} ({action})");
            assert!(!keys.starts_with("alt-"), "macOS binds {keys} ({action}): it moves by word in a shell");
        }
    }

    /// macOS has the keys every Mac app has, wherever focus is; Linux has none of them.
    #[test]
    fn macos_quits_closes_and_opens_settings_like_mac_apps() {
        let action_for = |mac: bool, keys: &str| {
            let keymap = Keymap::new(key_bindings_on(mac));
            let input = [Keystroke::parse(keys).unwrap()];
            let contexts = ["App", "Input"].map(|c| KeyContext::parse(c).unwrap());
            keymap.bindings_for_input(&input, &contexts).0.first().map(|b| b.action().boxed_clone())
        };
        let mac_keys: [(&str, &dyn Action); 8] = [
            ("cmd-q", &app::Quit),
            ("cmd-w", &app::CloseTab),
            ("cmd-shift-w", &app::CloseWindow),
            ("cmd-,", &app::GoSettings),
            ("cmd-m", &app::MinimizeWindow),
            ("cmd-h", &app::HideApp),
            ("cmd-k", &app::OpenPalette),
            ("cmd-/", &app::ShowShortcuts),
        ];
        for (keys, action) in mac_keys {
            let found = action_for(true, keys);
            assert!(found.is_some_and(|a| a.partial_eq(action)), "{keys} on macOS isn't {}", action.name());
            assert!(action_for(false, keys).is_none(), "{keys} is bound on Linux");
        }
    }

    fn bindings(keys: &str, contexts: &[&str], mac: bool) -> (usize, bool) {
        let keymap = Keymap::new(key_bindings_on(mac));
        let input: Vec<Keystroke> = keys.split(' ').map(|k| Keystroke::parse(k).unwrap()).collect();
        let contexts: Vec<KeyContext> = contexts.iter().map(|c| KeyContext::parse(c).unwrap()).collect();
        let (found, pending) = keymap.bindings_for_input(&input, &contexts);
        (found.len(), pending)
    }

    #[test]
    fn single_keys_act_outside_text_fields_only() {
        for mac in [false, true] {
            let found = |keys: &str, contexts: &[&str]| bindings(keys, contexts, mac);
            for keys in ["?", "g p", "g o", "g w", "g h", "g i", "g s"] {
                assert_eq!(found(keys, &["App"]).0, 1, "{keys} on a page");
                assert_eq!(found(keys, &["App", "DataTable"]).0, 1, "{keys} on a table");
                assert_eq!(found(keys, &["App", "Input"]), (0, false), "{keys} in a filter box");
                assert_eq!(found(keys, &["App", "Input mode=editor"]), (0, false), "{keys} in an editor");
                assert_eq!(found(keys, &["App", "Terminal"]), (0, false), "{keys} in a terminal");
            }
            // `g` alone waits for the page's letter, only outside text fields.
            assert_eq!(found("g", &["App"]), (0, true));
            assert_eq!(found("g", &["App", "Input"]), (0, false));
            for keys in ["j", "k"] {
                assert_eq!(found(keys, &["App", "DataTable"]).0, 1, "{keys} on a table");
                assert_eq!(found(keys, &["App"]).0, 0, "{keys} with no table focused");
                assert_eq!(found(keys, &["App", "DataTable", "Input"]), (0, false), "{keys} in a cell's input");
            }
        }
    }
}
