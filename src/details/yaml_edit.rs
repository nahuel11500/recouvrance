//! The YAML editor while the object changes on the cluster: a warning over the editor once
//! the live object is newer than the version the edit started from, with what changed, and
//! the edit moved onto the latest version or dropped.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{DetailPanel, diff_block, has_masked_values};
use crate::k8s;
use crate::k8s::edit;
use crate::mcp::preview::{self, Section};

impl DetailPanel {
    /// Whether the object changed on the cluster since the YAML being edited was read. An
    /// editor nobody edited follows the object, so only an edit can fall behind.
    pub(super) fn changed_meanwhile(&self) -> bool {
        let base = self.yaml_base.as_ref().and_then(edit::resource_version);
        let live = self.object.as_ref().and_then(edit::resource_version);
        matches!((base, live), (Some(base), Some(live)) if base != live)
    }

    /// "Deployment shop/web changed on the cluster since you started editing (by
    /// kube-controller-manager, 12s ago)."
    pub(super) fn changed_meanwhile_text(&self) -> String {
        let by = self
            .object
            .as_ref()
            .and_then(edit::last_writer)
            .map_or_default(|(manager, at)| format!(" (by {manager}, {} ago)", k8s::age(at)));
        format!("{} {} changed on the cluster since you started editing{by}.", self.obj.kind.ar.kind, self.obj.name)
    }

    /// What changed on the cluster since the edit started (Secret values hidden).
    pub(super) fn meanwhile_section(&self) -> Option<Section> {
        let (base, live) = (self.yaml_base.as_ref()?, self.object.as_ref()?);
        Some(preview::object_section("Since you started editing", Some(base), live, self.obj.kind.is("", "Secret")))
    }

    fn toggle_meanwhile(&mut self, cx: &mut Context<Self>) {
        self.yaml_compare = if self.yaml_compare.is_some() { None } else { self.meanwhile_section() };
        cx.notify();
    }

    /// Move the edit onto the live object: the fields it changed keep their edited value,
    /// the rest takes what the cluster has now. Save then sends it as of that version.
    pub(super) fn keep_edits_on_latest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(base), Some(live)) = (self.yaml_base.clone(), self.object.clone()) else { return };
        let text = self.yaml.read(cx).value().to_string();
        let edited: serde_json::Value = match crate::yaml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("Invalid YAML: {}", crate::yaml::message(&e));
                window.push_notification(Notification::error(msg), cx);
                return;
            }
        };
        // Masks would be moved over as values.
        if has_masked_values(&edited) {
            window.push_notification(Notification::error("Reveal secret values before editing a Secret."), cx);
            return;
        }
        let merged = edit::rebase(&base, &edited, &live);
        let mut shown_live = live.clone();
        if !self.show_managed
            && let Some(m) = shown_live.get_mut("metadata").and_then(|m| m.as_object_mut())
        {
            m.remove("managedFields");
        }
        let secret = self.obj.kind.is("", "Secret");
        match (
            k8s::to_yaml(&shown_live, secret, self.reveal_secrets),
            k8s::to_yaml(&merged, secret, self.reveal_secrets),
        ) {
            (Ok(live_text), Ok(text)) => {
                // The live object's text is what the editor was "filled from" now: the merged
                // edit counts as an edit, and the live refresh leaves it alone.
                self.yaml_text = live_text;
                self.yaml_base = Some(live);
                self.yaml_compare = None;
                self.yaml.update(cx, |s, cx| s.set_value(text, window, cx));
            }
            (Err(e), _) | (_, Err(e)) => {
                window.push_notification(Notification::error(format!("Could not merge the edit: {e:#}")), cx);
            }
        }
        cx.notify();
    }

    /// The warning over the editor while the edit is behind the cluster: what changed (on a
    /// click), keep the edit on the latest version, or drop it.
    pub(super) fn render_changed_meanwhile(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.changed_meanwhile() {
            return None;
        }
        let (warning, mono) = (cx.theme().warning, cx.theme().mono_font_family.clone());
        let compare = self.yaml_compare.as_ref();
        Some(
            v_flex()
                .gap_1()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(warning.opacity(0.15))
                .border_1()
                .border_color(warning)
                .text_sm()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(format!("{} Saving now would be refused.", self.changed_meanwhile_text())),
                        )
                        .child(
                            Button::new("meanwhile-compare")
                                .xsmall()
                                .ghost()
                                .label(if compare.is_some() { "Hide changes" } else { "Compare" })
                                .tooltip("What changed on the cluster since you started editing")
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_meanwhile(cx))),
                        )
                        .child(
                            Button::new("meanwhile-keep")
                                .xsmall()
                                .primary()
                                .label("Keep my edits on the latest")
                                .tooltip("Your edits, made on the object as it is now; Save shows the diff first")
                                .on_click(cx.listener(|this, _, window, cx| this.keep_edits_on_latest(window, cx))),
                        )
                        .child(
                            Button::new("meanwhile-reload")
                                .xsmall()
                                .ghost()
                                .label("Reload")
                                .tooltip("Drop your edits and show the object as it is now")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.refresh_yaml(window, cx);
                                    cx.notify();
                                })),
                        ),
                )
                .when_some(compare, |d, s| {
                    d.child(
                        v_flex()
                            .id("meanwhile-diff")
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .gap_0p5()
                            .font_family(mono.clone())
                            .text_xs()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(s.title.clone()))
                            .children(s.lines.iter().map(|line| div().pl_2().child(line.clone())))
                            .when(!s.diff.is_empty(), |d| d.child(diff_block(&s.diff, cx))),
                    )
                })
                .into_any_element(),
        )
    }
}

/// What a refused save says: the object moved on, and where to go from there.
pub(super) fn refused_as_stale(kind: &str, name: &str) -> String {
    format!(
        "{kind} {name} changed on the cluster since you started editing, so nothing was saved. \
         Compare, keep your edits on the latest version or reload, above the editor."
    )
}
