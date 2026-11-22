//! The keyboard shortcuts: how each platform names a key, and the list shown on `?`.
//! docs/features.md has the same table (a test keeps them equal), so the panel and the docs
//! can't disagree.

use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

use super::MAC;

/// Single keys (`?`, `g`, `j`, `k`) only act outside text fields and terminals, where they
/// are typed instead.
pub(super) const OUTSIDE_TEXT: &str = "App && !Input && !Terminal";

/// The same for keys that act on the focused table.
pub(super) const TABLE_OUTSIDE_TEXT: &str = "DataTable && !Input";

/// Keys, then what they do: one row of the panel and of the table in docs/features.md. A key in
/// braces is named as each platform does (see [`with_keys`]).
pub(super) const SHORTCUTS: &[(&str, &str)] = &[
    ("{secondary-k}", "Command palette: resources, pages, contexts, objects"),
    ("{secondary-shift-n}", "New window"),
    ("{secondary-t} / {secondary-w} / {ctrl-tab}", "New tab / close tab / next tab"),
    ("{secondary-b} / {f5}", "Hide the sidebar / refresh (in a terminal, F5 goes to the program)"),
    ("{alt-left|cmd-[} / {alt-right|cmd-]}", "Back / forward (Esc also goes back from a detail page)"),
    ("{secondary-=} / {secondary--} / {secondary-0}", "Zoom in / zoom out / reset the zoom"),
    (
        "{secondary-f} or {/}",
        "Filter the current table: `web` · `!Running` · `app=web` · `tier!=db` · `l:app` · `ns:team-*` · \
         `/^api-\\d+/` · `status:Running` · `restarts>3` · `cpu>200m` (all terms must match)",
    ),
    ("{j} / {k} / {enter}", "Move the selection in a table / open the selected row"),
    (
        "g then p / o / w / h / i / s",
        "Go to Problems / Cluster overview / Workloads overview / Helm / Assistant / Settings",
    ),
    ("{secondary-j}", "Show or hide the assistant beside the page: it is told what is on screen"),
    ("{?}", "List the keyboard shortcuts"),
    ("Right-click a row", "Open, logs, shell, YAML, events, copy name, favorite"),
];

/// `text` with each key in braces named as this platform does: `{secondary-k}` reads Ctrl+K on
/// Linux and ⌘K on macOS. A key is written as gpui binds it (`secondary` is ⌘ on macOS and Ctrl
/// elsewhere); `{alt-left|cmd-[}` is a different key on each, Linux's first.
pub(crate) fn with_keys(text: &str) -> String {
    with_keys_on(text, MAC)
}

fn with_keys_on(text: &str, mac: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('{') {
        let Some((key, after)) = after.split_once('}') else { break };
        out.push_str(before);
        out.push_str(&key_on(key_for(key, mac), mac));
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

/// The keys a text names in braces that no binding takes, on Linux or on macOS.
#[cfg(test)]
pub(crate) fn unbound_keys(text: &str) -> Vec<String> {
    let mut unbound = Vec::new();
    for mac in [false, true] {
        let keymap = Keymap::new(super::key_bindings_on(mac));
        for key in keys_in(text, mac) {
            let stroke = Keystroke::parse(&key).unwrap();
            if !keymap.bindings().any(|b| matches!(b.keystrokes(), [k] if *k.inner() == stroke)) {
                unbound.push(format!("{key} (macOS: {mac})"));
            }
        }
    }
    unbound
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

/// Open the list of shortcuts.
pub(super) fn open_panel(window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, |alert, _, cx| {
        let theme = cx.theme();
        let rows = SHORTCUTS.iter().map(|(keys, action)| {
            h_flex()
                .gap_4()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div().w(px(200.)).flex_shrink_0().child(
                        div()
                            .text_xs()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme.border)
                            .font_family(theme.mono_font_family.clone())
                            .child(with_keys(keys)),
                    ),
                )
                .child(div().flex_1().min_w_0().text_sm().child(action.replace('`', "")))
        });
        alert
            .title("Keyboard shortcuts")
            .description("Single keys (?, g, j, k) work when no text field or terminal has focus.")
            .width(px(720.))
            .child(v_flex().children(rows))
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

    use super::{SHORTCUTS, describe, keys_in, unbound_keys, with_keys_on};
    use crate::app::{self as app, key_bindings_on};

    /// The rows of the shortcut table in docs/features.md: the keys on Linux, on macOS, then
    /// what they do.
    fn docs_rows() -> Vec<(String, String, String)> {
        let docs = include_str!("../../docs/features.md");
        docs.lines()
            .skip_while(|l| l.trim() != "| Linux | macOS | Action |")
            .skip(2)
            .take_while(|l| l.starts_with('|'))
            .map(|line| {
                let inner = line.trim().trim_start_matches("| ").trim_end_matches(" |");
                let mut cells = inner.splitn(3, " | ").map(String::from);
                let mut cell = || cells.next().expect("a row has three cells");
                (cell(), cell(), cell())
            })
            .collect()
    }

    #[test]
    fn the_docs_list_every_shortcut_of_the_panel() {
        let docs = docs_rows();
        let code: Vec<(String, String, String)> = SHORTCUTS
            .iter()
            .map(|&(keys, action)| (with_keys_on(keys, false), with_keys_on(keys, true), action.to_string()))
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
    /// on Linux and on macOS.
    #[test]
    fn the_panel_names_only_bound_keys() {
        let named: usize = SHORTCUTS.iter().map(|(keys, _)| keys_in(keys, false).len()).sum();
        assert!(named > 15, "the list names {named} keys in braces");
        for (keys, action) in SHORTCUTS {
            assert_eq!(unbound_keys(keys), Vec::<String>::new(), "{action}");
        }
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
