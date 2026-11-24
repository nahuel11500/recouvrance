//! Keyboard focus and screen-reader names for the rows and cards the app draws itself.
//!
//! The toolkit's buttons, checkboxes and inputs already take focus with Tab and tell
//! screen readers what they are. A row drawn from a `div` does neither until it gets
//! [`Control::control`]. gpui builds the accessibility tree only while a screen reader
//! asks for it, so the names cost nothing otherwise.

use gpui_kit::{Hsla, InteractiveElement, MouseButton, Role, SharedString, StatefulInteractiveElement, Styled as _};

/// A clickable row or card that the keyboard and screen readers can use.
pub trait Control: StatefulInteractiveElement + Sized {
    /// Make this a tab stop that screen readers announce as `role`, named `name`.
    ///
    /// Enter and Space click it once it has focus (gpui sends them to the focused
    /// element's click handlers). Focus reached with the keyboard tints it with `ring`.
    /// A mouse press leaves no focus behind, as before: the window then gives focus back
    /// to the list or itself, so the table's keys still work after a click in the sidebar.
    fn control(self, role: Role, name: impl Into<SharedString>, ring: Hsla) -> Self {
        self.role(role).aria_label(name).tab_index(0).focus_visible(move |s| s.bg(ring.opacity(0.3))).on_mouse_down(
            MouseButton::Left,
            |_, window, cx| {
                // Before, the press focused the window's own handle; a row that kept focus
                // would keep the table's keys away from the table.
                window.prevent_default();
                window.blur(cx);
            },
        )
    }
}

impl<E: StatefulInteractiveElement + Sized> Control for E {}

/// The source with comments, and the text of string and char literals, blanked out, so
/// that brackets and words in them are not read as code.
#[cfg(test)]
fn code_only(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == 'r'
            && matches!(next, Some('"' | '#'))
            && !chars[..i].last().is_some_and(|p| p.is_alphanumeric() || *p == '_')
        {
            // A raw string: r"…" or r#"…"#.
            let mut j = i + 1;
            let mut hashes = 0;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                j += 1;
                while j < chars.len() && !(chars[j] == '"' && (1..=hashes).all(|h| chars.get(j + h) == Some(&'#'))) {
                    j += 1;
                }
                out.push_str("\"\"");
                i = j + 1 + hashes;
            } else {
                out.push(c);
                i += 1;
            }
        } else if c == '"' {
            i += 1;
            out.push('"');
            while i < chars.len() && chars[i] != '"' {
                let step = if chars[i] == '\\' { 2 } else { 1 };
                // Lines kept, so the line numbers found stay right.
                if chars.get(i + step - 1) == Some(&'\n') {
                    out.push('\n');
                }
                i += step;
            }
            out.push('"');
            i += 1;
        } else if c == '\'' && next == Some('\\') {
            // '\n', '\'', '\u{…}': the quote, the backslash and the escaped character.
            i += 3;
            while i < chars.len() && chars[i] != '\'' {
                i += 1;
            }
            out.push_str("' '");
            i += 1;
        } else if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            out.push_str("' '");
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Where the brackets opened at `open` close, or the end.
#[cfg(test)]
fn closing(code: &[char], open: usize) -> usize {
    let mut depth = 0;
    for (i, c) in code.iter().enumerate().skip(open) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    code.len()
}

/// Each `Button::new(…)` call with the methods chained on it, and the line it starts on.
#[cfg(test)]
fn button_chains(code: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = code.chars().collect();
    let needle: Vec<char> = concat!("Button", "::new(").chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        let word_start = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        if !(word_start && chars[i..i + needle.len()] == needle[..]) {
            i += 1;
            continue;
        }
        let mut end = closing(&chars, i + needle.len() - 1);
        loop {
            let mut j = end;
            while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            if chars.get(j) != Some(&'.') || chars.get(j + 1) == Some(&'.') {
                break;
            }
            j += 1;
            while chars.get(j).is_some_and(|c| c.is_alphanumeric() || *c == '_' || c.is_whitespace()) {
                j += 1;
            }
            if chars.get(j) != Some(&'(') {
                break;
            }
            end = closing(&chars, j);
        }
        let line = chars[..i].iter().filter(|c| **c == '\n').count() + 1;
        found.push((line, chars[i..end].iter().collect()));
        i = end;
    }
    found
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{button_chains, code_only};

    /// A button that shows only an icon says what it does in words to screen readers: the
    /// toolkit names a button after its label, and a tooltip doesn't count.
    #[test]
    fn every_icon_only_button_has_a_name() {
        fn walk(dir: &Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let code = code_only(&std::fs::read_to_string(&path).unwrap());
                    for (line, chain) in button_chains(&code) {
                        let has = |method: &str| chain.contains(&format!(".{method}("));
                        let named = ["label", "child", "children", "accessibility_label"].iter().any(|m| has(m));
                        if has("icon") && !named {
                            found.push(format!("{}:{line}", path.display()));
                        }
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
        assert!(found.is_empty(), "icon-only buttons with no accessibility_label: {found:#?}");
    }

    /// The scan reads chains across lines and comments, and leaves brackets in text alone.
    #[test]
    fn the_scan_reads_whole_chains() {
        let src = concat!(
            "let a = Button",
            "::new(\"x\")\n",
            "    .icon(I::X)\n",
            "    // a comment (with a bracket\n",
            "    .tooltip(\"Refresh (F5\")\n",
            "    .on_click(|_, _, _| {});\n",
            "let b = Button",
            "::new(('y', 1)).icon(I::Y).accessibility_label(\"Y\");\n",
            "let c = IconButton",
            "::new(\"z\").icon(I::Z);\n",
            "let d = r#\"Button",
            "::new(\"q\").icon(I::Q)\"#;\n",
            "let e = ('\\'', \"a\\\"b (\", '(');\n",
            "let g = \"two\nlines\";\n",
            "let f = Button",
            "::new(\"f\").icon(I::F).label(\"F\");\n",
        );
        let chains = button_chains(&code_only(src));
        assert_eq!(chains.len(), 3, "{chains:?}");
        assert_eq!(chains[0].0, 1);
        assert!(chains[0].1.contains(".on_click("), "{}", chains[0].1);
        assert_eq!(chains[1].0, 6);
        assert!(chains[1].1.contains(".accessibility_label("), "{}", chains[1].1);
        assert_eq!(chains[2].0, 12);
        assert!(chains[2].1.contains(".label("), "{}", chains[2].1);
    }
}

#[cfg(all(test, feature = "dev"))]
mod view_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    // Not `gpui_kit::*`: its `test` macro would shadow the `#[test]` that `#[gpui_kit::test]` expands to.
    use gpui_kit::{
        AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent, KeyUpEvent,
        Keystroke, Modifiers, ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px, red,
    };

    use super::Control as _;

    /// Two rows, and the clicks each took, in a view that holds focus as the app's does.
    struct Rows {
        clicks: Rc<RefCell<Vec<&'static str>>>,
        focus: FocusHandle,
    }

    impl Render for Rows {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let row = |name: &'static str| {
                let clicks = self.clicks.clone();
                div()
                    .id(name)
                    .debug_selector(move || name.into())
                    .w(px(80.))
                    .h(px(20.))
                    .control(Role::Link, name, red())
                    .on_click(move |_, _, _| clicks.borrow_mut().push(name))
            };
            div().id("rows").size_full().track_focus(&self.focus).child(row("first")).child(row("second"))
        }
    }

    /// The rows in a window, with the view's own handle focused.
    fn rows(cx: &mut TestAppContext) -> (Rc<RefCell<Vec<&'static str>>>, &mut VisualTestContext) {
        cx.update(gpui_kit::init);
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let shared = clicks.clone();
        let mut focus = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| Rows { clicks: shared, focus: cx.focus_handle() });
            focus = Some(view.read(cx).focus.clone());
            gpui_kit::base::Root::new(view, window, cx)
        });
        let focus = focus.expect("the window was built");
        cx.update(|window, cx| {
            window.focus(&focus, cx);
            drop(window.draw(cx));
        });
        (clicks, cx)
    }

    fn press(cx: &mut VisualTestContext, key: &str) {
        let keystroke = Keystroke::parse(key).unwrap();
        cx.simulate_event(KeyDownEvent { keystroke: keystroke.clone(), is_held: false, prefer_character_input: false });
        cx.simulate_event(KeyUpEvent { keystroke });
        cx.update(|window, cx| drop(window.draw(cx)));
    }

    /// Tab goes from row to row in the order they are drawn, and Enter or Space clicks the
    /// one with focus.
    #[gpui_kit::test]
    fn tab_reaches_each_row_and_enter_clicks_it(cx: &mut TestAppContext) {
        let (clicks, cx) = rows(cx);
        press(cx, "tab");
        press(cx, "enter");
        assert_eq!(*clicks.borrow(), ["first"]);
        press(cx, "tab");
        press(cx, "space");
        assert_eq!(*clicks.borrow(), ["first", "second"]);
        press(cx, "shift-tab");
        press(cx, "enter");
        assert_eq!(*clicks.borrow(), ["first", "second", "first"]);
    }

    /// A mouse click still clicks, and leaves no row with focus (nor the view's handle: the
    /// app gives focus back to the list itself), so the table's keys keep working after a
    /// click in the sidebar.
    #[gpui_kit::test]
    fn a_mouse_click_leaves_focus_to_the_window(cx: &mut TestAppContext) {
        let (clicks, cx) = rows(cx);
        let at = cx.debug_bounds("second").expect("drawn").center();
        cx.simulate_click(at, Modifiers::default());
        assert_eq!(*clicks.borrow(), ["second"]);
        assert!(cx.update(|window, cx| window.focused(cx)).is_none(), "the row kept focus after a click");
    }
}
