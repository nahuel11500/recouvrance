//! Settings → Kubeconfig: the files and folders contexts come from, adding more
//! (picked, typed or pasted) and removing them.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::k8s::{Pasted, Source, SourceKind};

type Signature = (Vec<(std::path::PathBuf, Option<std::time::SystemTime>, u64)>, Vec<String>, Vec<String>);

/// A kubeconfig being pasted, before it is saved.
struct Paste {
    editor: Entity<EditorState>,
    name: Entity<InputState>,
    checked: Result<Pasted, String>,
    /// Last name proposed; replaced as the text changes until the user types their own.
    suggested: String,
}

pub struct KubeconfigSources {
    sources: Vec<Source>,
    signature: Option<Signature>,
    path: Option<Entity<InputState>>,
    paste: Option<Paste>,
    /// A pasted kubeconfig waiting for a second click to be deleted.
    confirm_delete: Option<String>,
    problems: Vec<String>,
    _task: Option<Task<()>>,
    /// Enter in the path field, and changes to the paste: replaced when either opens again.
    _path_sub: Option<Subscription>,
    _paste_sub: Option<Subscription>,
}

impl KubeconfigSources {
    pub fn new(_: &mut Window, _: &mut Context<Self>) -> Self {
        Self {
            sources: Vec::new(),
            signature: None,
            path: None,
            paste: None,
            confirm_delete: None,
            problems: Vec::new(),
            _task: None,
            _path_sub: None,
            _paste_sub: None,
        }
    }

    /// Read the sources again when a file or the settings changed (cheap otherwise).
    fn refresh(&mut self) {
        let st = crate::settings::get();
        let sig = (crate::k8s::kubeconfig_signature(), st.kubeconfigs.clone(), st.kubeconfig_folders);
        if self.signature.as_ref() != Some(&sig) {
            self.sources = crate::k8s::sources();
            self.signature = Some(sig);
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.signature = None;
        cx.refresh_windows();
    }

    fn add(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        self.problems = crate::k8s::add_paths(paths);
        self.changed(cx);
    }

    fn pick(&mut self, directories: bool, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
            multiple: !directories,
            prompt: Some(if directories { "Watch folder" } else { "Add kubeconfig" }.into()),
        });
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let _ = this.update(cx, |this, cx| this.add(&paths, cx));
        }));
    }

    fn open_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("~/clusters/team.yaml or a folder, Enter to add"));
        self._path_sub = Some(cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.add_typed(cx);
            }
        }));
        input.update(cx, |s, cx| s.focus(window, cx));
        self.path = Some(input);
        cx.notify();
    }

    fn add_typed(&mut self, cx: &mut Context<Self>) {
        let Some(input) = &self.path else { return };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.add(&[crate::settings::expand(&text)], cx);
        if self.problems.is_empty() {
            self.path = None;
        }
    }

    fn open_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx).language("yaml").line_number(true).placeholder("Paste a kubeconfig here")
        });
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("File name"));
        self._paste_sub = Some(cx.subscribe_in(&editor, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                let text = state.read(cx).value().to_string();
                this.check_paste(&text, window, cx);
            }
        }));
        editor.update(cx, |s, cx| s.focus(window, cx));
        self.paste = Some(Paste { editor, name, checked: Err(String::new()), suggested: String::new() });
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn demo_paste(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_paste(window, cx);
        if let Some(p) = &self.paste {
            p.editor.update(cx, |s, cx| s.set_value(text.to_string(), window, cx));
        }
        self.check_paste(text, window, cx);
    }

    fn check_paste(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = &mut self.paste else { return };
        p.checked = crate::k8s::inspect_pasted(text).map_err(|e| format!("{e:#}"));
        if let Ok(ok) = &p.checked {
            let current = p.name.read(cx).value().to_string();
            if current.is_empty() || current == p.suggested {
                let name = crate::k8s::safe_file_part(&ok.suggested_name);
                p.name.update(cx, |s, cx| s.set_value(name.clone(), window, cx));
                p.suggested = name;
            }
        }
        cx.notify();
    }

    fn save_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = &self.paste else { return };
        let text = p.editor.read(cx).value().to_string();
        let name = p.name.read(cx).value().to_string();
        let n = p.checked.as_ref().map_or(0, |c| c.contexts.len());
        match crate::k8s::save_pasted(&text, &name) {
            Ok(path) => {
                let msg = format!("Saved {} ({n} context{})", crate::k8s::tilde(&path), if n == 1 { "" } else { "s" });
                window.push_notification(Notification::success(msg), cx);
                self.paste = None;
                self.changed(cx);
            }
            Err(e) => window.push_notification(Notification::error(format!("Not saved: {e:#}")), cx),
        }
    }

    fn remove(&mut self, src: Source, window: &mut Window, cx: &mut Context<Self>) {
        if src.kind == SourceKind::Added && self.confirm_delete.as_deref() != Some(src.path.as_str()) {
            self.confirm_delete = Some(src.path);
            cx.notify();
            return;
        }
        self.confirm_delete = None;
        if let Err(e) = crate::k8s::remove_source(&src) {
            window.push_notification(Notification::error(format!("{e:#}")), cx);
        }
        self.changed(cx);
    }

    fn render_source(&self, i: usize, src: &Source, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let icon = match src.kind {
            SourceKind::Folder => Lucide::Folder,
            SourceKind::Added => Lucide::FileKey,
            _ => Lucide::File,
        };
        let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
        let mut facts = Vec::new();
        if src.error.is_none() && (src.kind == SourceKind::Folder || src.files != 1) {
            facts.push(plural(src.files, "file"));
        }
        if src.files > 0 && src.error.is_none() {
            facts.push(plural(src.contexts, "context"));
        }
        facts.push(
            match src.kind {
                SourceKind::Default => "default",
                SourceKind::File => "added file",
                SourceKind::Folder => "watched folder",
                SourceKind::Added => "pasted",
            }
            .into(),
        );
        let confirming = self.confirm_delete.as_deref() == Some(src.path.as_str());
        let s = src.clone();
        h_flex()
            .gap_3()
            .px_3()
            .py_2()
            .when(i > 0, |d| d.border_t_1().border_color(theme.border))
            .child(Icon::new(icon).size_4().text_color(if src.error.is_some() { theme.danger } else { muted }))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .font_family(theme.mono_font_family.clone())
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(if src.kind == SourceKind::Added {
                                // Always in our own folder: the name says enough.
                                src.path.rsplit('/').next().unwrap_or_default().to_string()
                            } else {
                                src.path.clone()
                            }),
                    )
                    .child(div().text_xs().text_color(muted).child(facts.join(" · ")))
                    .when_some(src.error.clone(), |d, e| d.child(div().text_xs().text_color(theme.danger).child(e))),
            )
            .when(src.kind != SourceKind::Default, |d| {
                d.child(if confirming {
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(("kc-del", i))
                                .xsmall()
                                .danger()
                                .label("Delete file")
                                .on_click(cx.listener(move |this, _, window, cx| this.remove(s.clone(), window, cx))),
                        )
                        .child(Button::new(("kc-keep", i)).xsmall().ghost().label("Keep").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.confirm_delete = None;
                                cx.notify();
                            },
                        )))
                        .into_any_element()
                } else {
                    Button::new(("kc-rm", i))
                        .xsmall()
                        .ghost()
                        .icon(Lucide::X)
                        .tooltip(if src.kind == SourceKind::Added {
                            "Delete this pasted kubeconfig"
                        } else {
                            "Stop using"
                        })
                        .on_click(cx.listener(move |this, _, window, cx| this.remove(s.clone(), window, cx)))
                        .into_any_element()
                })
            })
    }

    fn render_paste(&self, p: &Paste, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let check: AnyElement = match &p.checked {
            Err(e) if e.is_empty() => div().into_any_element(),
            Err(e) => div().text_xs().text_color(theme.danger).child(e.clone()).into_any_element(),
            Ok(c) => v_flex()
                .gap_1()
                .text_xs()
                .child(h_flex().gap_1().child(Icon::new(Lucide::Check).size_3().text_color(theme.success)).child(
                    format!(
                        "{}: {}",
                        if c.contexts.len() == 1 { "Context" } else { "Contexts" },
                        c.contexts.join(", ")
                    ),
                ))
                .when(!c.taken.is_empty(), |d| {
                    let file = crate::k8s::pasted_stem(&p.name.read(cx).value());
                    let renamed: Vec<String> = c.taken.iter().map(|t| format!("{t} as {t}@{file}")).collect();
                    d.child(
                        div()
                            .text_color(muted)
                            .child(format!("Names already in use, shown as: {}", renamed.join(", "))),
                    )
                })
                .children(c.warnings.iter().map(|w| {
                    h_flex()
                        .gap_1()
                        .items_start()
                        .child(Icon::new(Lucide::TriangleAlert).size_3().text_color(theme.warning))
                        .child(div().flex_1().min_w_0().child(w.clone()))
                }))
                .into_any_element(),
        };
        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Paste a kubeconfig"))
            .child(div().text_xs().text_color(muted).child(format!(
                "Saved as it is in {}, readable only by you, and merged with the other sources.",
                crate::k8s::tilde(&crate::k8s::added_dir())
            )))
            .child(
                div()
                    .h(px(260.))
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .child(Editor::new(&p.editor).h_full()),
            )
            .child(check)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(260.)).child(Input::new(&p.name).small()))
                    .child(div().text_xs().text_color(muted).child(".yaml"))
                    .child(div().flex_1())
                    .child(Button::new("kc-paste-cancel").small().ghost().label("Cancel").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.paste = None;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("kc-paste-save")
                            .small()
                            .primary()
                            .label("Save")
                            .disabled(p.checked.is_err())
                            .on_click(cx.listener(|this, _, window, cx| this.save_paste(window, cx))),
                    ),
            )
    }
}

impl Render for KubeconfigSources {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh();
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let sources = self.sources.clone();
        let total: usize = sources.iter().filter(|s| s.error.is_none()).map(|s| s.contexts).sum();
        let rows: Vec<AnyElement> =
            sources.iter().enumerate().map(|(i, s)| self.render_source(i, s, cx).into_any_element()).collect();
        let paste = self.paste.as_ref().map(|p| self.render_paste(p, cx).into_any_element());
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("Kubeconfig sources"))
                    .child(div().text_sm().text_color(muted).child(format!(
                        "Where contexts come from, merged in this order ({total} contexts). Changes to these files are picked up within seconds."
                    ))),
            )
            .child(
                v_flex()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .children(rows),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(Button::new("kc-files").small().outline().icon(Lucide::FilePlus).label("Add files").on_click(
                        cx.listener(|this, _, window, cx| this.pick(false, window, cx)),
                    ))
                    .child(Button::new("kc-folder").small().outline().icon(Lucide::FolderPlus).label("Watch folder").on_click(
                        cx.listener(|this, _, window, cx| this.pick(true, window, cx)),
                    ))
                    .child(
                        Button::new("kc-path")
                            .small()
                            .outline()
                            .icon(Lucide::TextCursorInput)
                            .label("Enter path")
                            .on_click(cx.listener(|this, _, window, cx| this.open_path(window, cx))),
                    )
                    .child(
                        Button::new("kc-paste")
                            .small()
                            .outline()
                            .icon(Lucide::ClipboardPaste)
                            .label("Paste kubeconfig")
                            .disabled(self.paste.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.open_paste(window, cx))),
                    ),
            )
            .when_some(self.path.clone(), |d, input| {
                d.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&input).small()))
                        .child(Button::new("kc-path-add").small().primary().label("Add").on_click(cx.listener(
                            |this, _, _, cx| this.add_typed(cx),
                        )))
                        .child(Button::new("kc-path-cancel").small().ghost().label("Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.path = None;
                                this.problems.clear();
                                cx.notify();
                            },
                        ))),
                )
            })
            .children(self.problems.iter().map(|p| div().text_xs().text_color(theme.danger).child(p.clone())))
            .children(paste)
            .child(
                v_flex()
                    .pt_3()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("How sources merge"))
                    .child(div().text_xs().text_color(muted).child(
                        "Every source is read and merged, so a file may hold only a cluster or a user that another file's context uses. \
                         When two files use the same context, cluster or user name, the later one gets @<file name> appended so both \
                         stay usable. Recouvrance never changes your kubeconfig files; KUBECONFIG replaces ~/.kube/config as the default.",
                    )),
            )
    }
}
