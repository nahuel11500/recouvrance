//! The list of keyboard shortcuts shown on `?`. docs/features.md has the same table (a test
//! keeps them equal), so the panel and the docs can't disagree.

use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

/// Single keys (`?`, `g`, `j`, `k`) only act outside text fields and terminals, where they
/// are typed instead.
pub(super) const OUTSIDE_TEXT: &str = "App && !Input && !Terminal";

/// The same for keys that act on the focused table.
pub(super) const TABLE_OUTSIDE_TEXT: &str = "DataTable && !Input";

/// Keys, then what they do: one row of the panel and of the table in docs/features.md.
pub(super) const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+K", "Command palette: resources, pages, contexts, objects"),
    ("Ctrl+Shift+N", "New window"),
    ("Ctrl+T / Ctrl+W / Ctrl+Tab", "New tab / close tab / next tab"),
    ("Ctrl+B / F5", "Hide the sidebar / refresh (in a terminal, F5 goes to the program)"),
    ("Alt+← / Alt+→", "Back / forward (Esc also goes back from a detail page)"),
    ("Ctrl+= / Ctrl+- / Ctrl+0", "Zoom in / zoom out / reset the zoom"),
    (
        "Ctrl+F or /",
        "Filter the current table: `web` · `!Running` · `app=web` · `tier!=db` · `l:app` · `ns:team-*` · \
         `/^api-\\d+/` · `status:Running` · `restarts>3` · `cpu>200m` (all terms must match)",
    ),
    ("j / k / Enter", "Move the selection in a table / open the selected row"),
    (
        "g then p / o / w / h / i / s",
        "Go to Problems / Cluster overview / Workloads overview / Helm / Assistant / Settings",
    ),
    ("?", "List the keyboard shortcuts"),
    ("Right-click a row", "Open, logs, shell, YAML, events, copy name, favorite"),
];

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
                            .child(*keys),
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

#[cfg(test)]
mod tests {
    use gpui_kit::{KeyContext, Keymap, Keystroke};

    use super::SHORTCUTS;

    /// The rows of the shortcut table in docs/features.md.
    fn docs_rows() -> Vec<(String, String)> {
        let docs = include_str!("../../docs/features.md");
        docs.lines()
            .skip_while(|l| l.trim() != "| Shortcut | Action |")
            .skip(2)
            .take_while(|l| l.starts_with('|'))
            .map(|line| {
                let inner = line.trim().trim_start_matches("| ").trim_end_matches(" |");
                let (keys, action) = inner.split_once(" | ").expect("a row has two cells");
                (keys.to_string(), action.to_string())
            })
            .collect()
    }

    #[test]
    fn the_docs_list_every_shortcut_of_the_panel() {
        let docs = docs_rows();
        let code: Vec<(String, String)> = SHORTCUTS.iter().map(|(k, a)| (k.to_string(), a.to_string())).collect();
        for row in &code {
            assert!(docs.contains(row), "docs/features.md lacks the shortcut {row:?}");
        }
        assert_eq!(docs, code, "docs/features.md and the `?` panel list different shortcuts");
    }

    fn bindings(keys: &str, contexts: &[&str]) -> (usize, bool) {
        let keymap = Keymap::new(crate::app::key_bindings());
        let input: Vec<Keystroke> = keys.split(' ').map(|k| Keystroke::parse(k).unwrap()).collect();
        let contexts: Vec<KeyContext> = contexts.iter().map(|c| KeyContext::parse(c).unwrap()).collect();
        let (found, pending) = keymap.bindings_for_input(&input, &contexts);
        (found.len(), pending)
    }

    #[test]
    fn single_keys_act_outside_text_fields_only() {
        for keys in ["?", "g p", "g o", "g w", "g h", "g i", "g s"] {
            assert_eq!(bindings(keys, &["App"]).0, 1, "{keys} on a page");
            assert_eq!(bindings(keys, &["App", "DataTable"]).0, 1, "{keys} on a table");
            assert_eq!(bindings(keys, &["App", "Input"]), (0, false), "{keys} in a filter box");
            assert_eq!(bindings(keys, &["App", "Input mode=editor"]), (0, false), "{keys} in an editor");
            assert_eq!(bindings(keys, &["App", "Terminal"]), (0, false), "{keys} in a terminal");
        }
        // `g` alone waits for the page's letter, only outside text fields.
        assert_eq!(bindings("g", &["App"]), (0, true));
        assert_eq!(bindings("g", &["App", "Input"]), (0, false));
        for keys in ["j", "k"] {
            assert_eq!(bindings(keys, &["App", "DataTable"]).0, 1, "{keys} on a table");
            assert_eq!(bindings(keys, &["App"]).0, 0, "{keys} with no table focused");
            assert_eq!(bindings(keys, &["App", "DataTable", "Input"]), (0, false), "{keys} in a cell's input");
        }
    }
}
