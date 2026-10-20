//! A side panel next to a YAML editor that explains the field under the cursor,
//! like `kubectl explain`, from the cluster's own OpenAPI schema (CRDs included).

use gpui_kit::component::input::EditorState;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::schema::{Explained, Schema};

pub struct FieldDocs {
    context: String,
    client: Client,
    editor: Entity<EditorState>,
    kind: Option<(String, String)>,
    schema: Option<Result<Schema, String>>,
    path: Vec<String>,
    _task: Option<Task<()>>,
    _sub: Subscription,
}

impl FieldDocs {
    pub fn new(context: String, client: Client, editor: Entity<EditorState>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&editor, |this, _, cx| this.recompute(cx));
        let mut this =
            Self { context, client, editor, kind: None, schema: None, path: Vec::new(), _task: None, _sub: sub };
        this.recompute(cx);
        this
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        let (text, line) = {
            let e = self.editor.read(cx);
            (e.value().to_string(), e.cursor_position().line as usize)
        };
        let path = crate::schema::yaml_path_at(&text, line);
        let kind = crate::schema::doc_kind_at(&text, line);
        if kind != self.kind {
            self.kind = kind.clone();
            self.schema = None;
            if let Some((api_version, kind)) = kind {
                let (ctx, client) = (self.context.clone(), self.client.clone());
                self._task = Some(cx.spawn(async move |this, cx| {
                    let r = crate::schema::document(&ctx, client, &api_version)
                        .await
                        .map_err(|e| format!("{e:#}"))
                        .and_then(|doc| {
                            Schema::find(doc, &api_version, &kind)
                                .ok_or_else(|| format!("No schema for {api_version} {kind}"))
                        });
                    let _ = this.update(cx, |this, cx| {
                        this.schema = Some(r);
                        cx.notify();
                    });
                }));
            }
        }
        if path != self.path {
            self.path = path;
        }
        cx.notify();
    }

    fn explained(&self) -> Option<Explained> {
        let schema = self.schema.as_ref()?.as_ref().ok()?;
        // Unknown keys (typos, map entries): explain the deepest known parent.
        (0..=self.path.len()).rev().find_map(|n| schema.explain(&self.path[..n]))
    }
}

impl Render for FieldDocs {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let body: AnyElement = match (&self.kind, &self.schema) {
            (None, _) => {
                div().text_sm().text_color(muted).child("Add apiVersion and kind to see field docs.").into_any_element()
            }
            (Some(_), None) => div().text_sm().text_color(muted).child("Loading the schema…").into_any_element(),
            (Some(_), Some(Err(e))) => div().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
            (Some((_, kind)), Some(Ok(_))) => match self.explained() {
                None => div().into_any_element(),
                Some(x) => {
                    let unknown = x.path.len() < self.path.len();
                    v_flex()
                        .gap_2()
                        .child(div().text_xs().text_color(muted).child(
                            std::iter::once(kind.clone()).chain(x.path.iter().cloned()).collect::<Vec<_>>().join(" › "),
                        ))
                        .when(unknown, |d| {
                            d.child(div().text_xs().text_color(theme.warning).child(format!(
                                "`{}` is not in the schema (typo, or a map key).",
                                self.path.last().cloned().unwrap_or_default()
                            )))
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child(x.field.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(theme.secondary)
                                        .child(x.field.type_name.clone()),
                                )
                                .when(x.field.required, |d| {
                                    d.child(div().text_xs().text_color(theme.danger).child("required"))
                                }),
                        )
                        .child(div().text_sm().child(x.field.description.clone()))
                        .when(!x.field.enum_values.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(format!("One of: {}", x.field.enum_values.join(", "))),
                            )
                        })
                        .when(!x.children.is_empty(), |d| {
                            d.child(div().pt_2().text_xs().text_color(muted).child("FIELDS")).children(
                                x.children.iter().map(|f| {
                                    let first = f.description.split(". ").next().unwrap_or_default().to_string();
                                    v_flex()
                                        .py_1()
                                        .border_b_1()
                                        .border_color(theme.border)
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .text_sm()
                                                .child(
                                                    div()
                                                        .font_family(theme.mono_font_family.clone())
                                                        .child(f.name.clone()),
                                                )
                                                .child(div().text_xs().text_color(muted).child(f.type_name.clone()))
                                                .when(f.required, |d| {
                                                    d.child(div().text_xs().text_color(theme.danger).child("required"))
                                                }),
                                        )
                                        .when(!first.is_empty(), |d| {
                                            d.child(div().text_xs().text_color(muted).child(first))
                                        })
                                }),
                            )
                        })
                        .into_any_element()
                }
            },
        };
        v_flex()
            .id("field-docs")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(div().text_xs().text_color(muted).child("FIELD DOCS · kubectl explain"))
            .child(body)
    }
}
