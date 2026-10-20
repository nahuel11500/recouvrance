//! Rules as code: the rules.yaml editor.

#[allow(unused_imports)]
use super::*;

impl AgentsView {
    /// The file's text, or what it would contain (from the current agents).
    pub(super) fn rules_text(&self) -> String {
        std::fs::read_to_string(crate::mcp::rules_file::path()).unwrap_or_else(|_| {
            crate::mcp::rules_file::render(&self.hub.config().agents).unwrap_or_else(|e| format!("# {e:#}\n"))
        })
    }

    pub(super) fn toggle_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.code.take().is_some() {
            cx.notify();
            return;
        }
        let text = self.rules_text();
        let editor = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).language("yaml").line_number(true).searchable(true);
            s.set_value(text.clone(), window, cx);
            s
        });
        self.code = Some((editor, text));
        self.code_message = None;
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn demo_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_code(window, cx);
    }

    /// Validate and save the editor's text to rules.yaml.
    pub(super) fn save_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, _)) = &self.code else { return };
        let text = editor.read(cx).value().to_string();
        match self.hub.save_rules_text(&text) {
            Ok(()) => {
                self.code = Some((editor.clone(), text));
                self.code_message =
                    Some((true, format!("Saved {} and applied", crate::mcp::rules_file::path().display())));
                if let Some(id) = self.selected.clone() {
                    self.select(id, window, cx);
                }
            }
            Err(e) => self.code_message = Some((false, format!("{e:#}"))),
        }
        cx.notify();
    }

    /// Pick up changes made on disk while the editor is open and untouched.
    pub fn refresh_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, loaded)) = &self.code else { return };
        if editor.read(cx).value().as_ref() != loaded.as_str() {
            return;
        }
        let text = self.rules_text();
        if text != *loaded {
            let e = editor.clone();
            e.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
            self.code = Some((e, text));
        }
    }

    pub(super) fn render_code(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((editor, loaded)) = &self.code else { return div().into_any_element() };
        let theme = cx.theme();
        let exists = self.hub.rules_file_active();
        let dirty = editor.read(cx).value().as_ref() != loaded.as_str();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_4()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child("Rules as code"))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(crate::mcp::rules_file::path().display().to_string()))
                    .child(div().flex_1())
                    .when(!exists, |d| d.child(div().text_sm().text_color(theme.muted_foreground).child("Not created yet: saving creates it")))
                    .child(
                        Button::new("rules-save")
                            .small()
                            .primary()
                            .icon(Lucide::Save)
                            .label(if exists { "Save" } else { "Create rules.yaml" })
                            .disabled(exists && !dirty)
                            .on_click(cx.listener(|this, _, window, cx| this.save_code(window, cx))),
                    ),
            )
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "One line per rule: VERBS RESOURCES[/NAMES] [-n NAMESPACES] [--context CONTEXTS] [until 2026-11-01]. \
                 Saved files are checked first; edits in your own editor are picked up too. The visual editor writes here as well (comments are not kept).",
            ))
            .when_some(self.code_message.clone().or_else(|| self.hub.rules_error().map(|e| (false, e))), |d, (ok, m)| {
                d.child(div().text_sm().text_color(if ok { theme.success } else { theme.danger }).child(m))
            })
            .child(div().flex_1().min_h_0().border_1().border_color(theme.border).rounded_md().child(CodeEditor::new(editor).h_full()))
            .into_any_element()
    }
}
