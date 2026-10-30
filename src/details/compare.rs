//! Compare tab: the same object in another cluster, or another object here.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{DetailPanel, error_box};
use crate::history::DiffLine;
use crate::k8s;
use crate::k8s::ObjectRef;
use crate::ui::selectable::selectable;

impl DetailPanel {
    /// Compare with the same object in another cluster.
    pub fn compare_with_cluster(&mut self, context: String, cx: &mut Context<Self>) {
        let Some((_, client)) = self.clusters.iter().find(|(c, _)| *c == context).cloned() else { return };
        self.compare.target_ctx = Some(context);
        self.compare.target_name = None;
        self.compare.other = None;
        let obj = self.obj.clone();
        self.compare._compare = Some(cx.spawn(async move |this, cx| {
            let r = k8s::get_object(client, obj).await.map_err(|e| k8s::short_error(&e));
            let _ = this.update(cx, |this, cx| {
                this.compare.other = Some(r);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Compare with another object of the same kind (same namespace).
    pub(super) fn compare_with_object(&mut self, name: String, cx: &mut Context<Self>) {
        self.compare.target_ctx = None;
        self.compare.target_name = Some(name.clone());
        self.compare.other = None;
        let obj = ObjectRef { name, uid: String::new(), ..self.obj.clone() };
        let client = self.client.clone();
        self.compare._compare = Some(cx.spawn(async move |this, cx| {
            let r = k8s::get_object(client, obj).await.map_err(|e| k8s::short_error(&e));
            let _ = this.update(cx, |this, cx| {
                this.compare.other = Some(r);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn load_compare_candidates(&mut self, cx: &mut Context<Self>) {
        // Listed once per page, even when nothing else of this kind is here.
        if self.compare.candidates_loaded {
            return;
        }
        self.compare.candidates_loaded = true;
        let (client, ar, ns, me) =
            (self.client.clone(), self.obj.kind.ar.clone(), self.obj.namespace.clone(), self.obj.name.clone());
        self.compare._candidates = Some(cx.spawn(async move |this, cx| {
            let result = k8s::list_names(client, ar, ns).await.map(|names| {
                let mut list: Vec<String> =
                    names.into_iter().map(|(_, name)| name).filter(|name| *name != me).collect();
                list.sort();
                list.truncate(200);
                list
            });
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(list) => this.compare.candidates = list,
                    Err(e) => this.compare.candidates_error = Some(k8s::short_error(&e)),
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn render_compare(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.load_compare_candidates(cx);
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let others: Vec<String> = self.clusters.iter().map(|(c, _)| c.clone()).filter(|c| *c != self.context).collect();
        let pick_bar = v_flex()
            .gap_2()
            .when(!others.is_empty(), |d| {
                d.child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .child(div().text_xs().text_color(muted).child("Same object in"))
                        .children(others.iter().enumerate().map(|(i, c)| {
                            let ctx = c.clone();
                            Button::new(("cmp-ctx", i))
                                .xsmall()
                                .label(c.clone())
                                .when(self.compare.target_ctx.as_deref() == Some(c), ButtonVariants::primary)
                                .when(self.compare.target_ctx.as_deref() != Some(c), Button::outline)
                                .on_click(cx.listener(move |this, _, _, cx| this.compare_with_cluster(ctx.clone(), cx)))
                        })),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(div().text_xs().text_color(muted).child(format!("Another {} here", self.obj.kind.ar.kind)))
                    .children(self.compare.candidates.iter().take(40).enumerate().map(|(i, n)| {
                        let name = n.clone();
                        Button::new(("cmp-obj", i))
                            .xsmall()
                            .label(n.clone())
                            .when(self.compare.target_name.as_deref() == Some(n), ButtonVariants::primary)
                            .when(self.compare.target_name.as_deref() != Some(n), ButtonVariants::ghost)
                            .on_click(cx.listener(move |this, _, _, cx| this.compare_with_object(name.clone(), cx)))
                    }))
                    .when_some(self.compare.candidates_error.clone(), |d, e| {
                        d.child(div().text_xs().text_color(theme.danger).child(selectable(e)))
                    }),
            );
        let body: AnyElement = match (&self.object, &self.compare.other) {
            (_, None) if self.compare.target_ctx.is_none() && self.compare.target_name.is_none() => {
                div().text_sm().text_color(muted).child(selectable("Pick what to compare with.")).into_any_element()
            }
            (_, None) => div().text_sm().text_color(muted).child(selectable("Loading…")).into_any_element(),
            (_, Some(Err(e))) => error_box(e.clone().into(), cx).into_any_element(),
            (None, _) => div().into_any_element(),
            (Some(mine), Some(Ok(theirs))) => {
                let mask = self.obj.kind.is("", "Secret") && !self.reveal_secrets;
                let (a, b) = (comparable_yaml(theirs, mask), comparable_yaml(mine, mask));
                let left = self.compare.target_ctx.clone().or(self.compare.target_name.clone()).unwrap_or_default();
                let lines = crate::history::diff_lines(&a, &b);
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(selectable(format!("− {left}    + {} (this)", self.context))))
                    .child(div().text_xs().text_color(muted).child(selectable(if mask {
                        "Ignoring uid, resourceVersion, timestamps, managedFields and status. \
                                 Secret values are masked: reveal them to compare values."
                    } else {
                        "Ignoring uid, resourceVersion, timestamps, managedFields and status."
                    })))
                    .child(if lines.is_empty() {
                        div().text_sm().text_color(theme.success).child(selectable("Identical.")).into_any_element()
                    } else {
                        diff_block(&lines, cx)
                    })
                    .into_any_element()
            }
        };
        v_flex()
            .id("compare")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_3()
            .child(pick_bar)
            .child(body)
            .into_any_element()
    }
}

/// YAML without fields that always differ between two copies of an object.
/// `mask` hides Secret values (still showing which keys differ, by a changed mark).
pub(super) fn comparable_yaml(v: &serde_json::Value, mask: bool) -> String {
    let mut v = v.clone();
    if mask {
        for field in ["data", "stringData"] {
            if let Some(data) = v.get_mut(field).and_then(|d| d.as_object_mut()) {
                for val in data.values_mut() {
                    *val = serde_json::Value::String("••••••".into());
                }
            }
        }
    }
    if let Some(m) = v.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        for k in [
            "uid",
            "resourceVersion",
            "creationTimestamp",
            "generation",
            "managedFields",
            "selfLink",
            "ownerReferences",
        ] {
            m.remove(k);
        }
        // Holds a full copy of the object (Secret values included) and differs anyway.
        if let Some(a) = m.get_mut("annotations").and_then(|a| a.as_object_mut()) {
            a.remove("kubectl.kubernetes.io/last-applied-configuration");
            a.remove("deployment.kubernetes.io/revision");
        }
    }
    if let Some(o) = v.as_object_mut() {
        o.remove("status");
    }
    crate::history::drop_nulls(&mut v);
    crate::yaml::to_string(&v).unwrap_or_default()
}

/// A diff as the Compare and History tabs show it: removed lines in red, added ones in
/// green, a mark where unchanged lines are left out.
pub(crate) fn diff_block(lines: &[DiffLine], cx: &App) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    v_flex()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .py_1()
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .children(lines.iter().map(|line| {
            let (sign, bg, fg, text) = match line {
                DiffLine::Gap => return div().text_color(muted).child("⋯").into_any_element(),
                DiffLine::Removed(t) => ("-", Some(theme.danger.opacity(0.12)), theme.danger, t),
                DiffLine::Added(t) => ("+", Some(theme.success.opacity(0.12)), theme.success, t),
                DiffLine::Same(t) => (" ", None, muted, t),
            };
            h_flex()
                .px_2()
                .when_some(bg, Styled::bg)
                .text_color(fg)
                .child(div().w(px(16.)).child(sign))
                .child(div().whitespace_nowrap().child(selectable(text.clone())))
                .into_any_element()
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn comparable_yaml_masks_secrets() {
        let secret = json!({
            "kind": "Secret",
            "metadata": {"name": "s", "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"config\":\"ZXhhbXBsZQ==\"}}"
            }},
            "data": {"config": "ZXhhbXBsZQ=="},
            "stringData": {"note": "plain"},
        });
        let masked = super::comparable_yaml(&secret, true);
        assert!(!masked.contains("ZXhhbXBsZQ") && !masked.contains("plain"), "{masked}");
        assert!(masked.contains("config:") && masked.contains("note:"));
        let shown = super::comparable_yaml(&secret, false);
        assert!(shown.contains("ZXhhbXBsZQ==") && !shown.contains("last-applied"));
    }
}
