//! The Map tab of every detail page (`crate::map`): a pod's or workload's tree, then what the
//! object is linked to. Read when the tab opens and worked out off the UI thread; drawing only
//! draws.

use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::DetailPanel;
use crate::map::{self, Item, Lists, Section};
use crate::overview::Ctx;
use crate::ui::selectable::selectable;

/// What the tab read when it was last opened, and the relations worked out from it.
#[derive(Default)]
pub(super) struct MapState {
    /// The tab was opened: its relations follow the page's reloads.
    opened: bool,
    lists: Option<Arc<Lists>>,
    loading: bool,
    load: crate::latest::Latest,
    /// The relations, from `lists` and the page's `Related` as last loaded.
    sections: Option<Arc<Vec<Section>>>,
    /// `sections` are being worked out again.
    computing: bool,
    compute: crate::latest::Latest,
}

impl MapState {
    /// Not read yet, nor being read.
    pub(super) fn unread(&self) -> bool {
        self.lists.is_none() && !self.loading
    }
}

impl DetailPanel {
    /// Read what the Map needs, on the object as last read: each time the tab is opened.
    pub(super) fn load_map(&mut self, cx: &mut Context<Self>) {
        let Some(obj) = self.object.clone() else { return };
        let work = map::load(self.client.clone(), self.obj.kind.clone(), obj, self.kinds.clone());
        self.map.opened = true;
        self.map.loading = true;
        self.map.load.spawn(cx, work, |this, lists, cx| {
            this.map.lists = Some(Arc::new(lists));
            this.map.loading = false;
            this.recompute_map(cx);
            cx.notify();
        });
    }

    /// Work the relations out again, off the UI thread, once the tab has read its lists and
    /// the page has loaded what it holds: when either arrives.
    pub(super) fn recompute_map(&mut self, cx: &mut Context<Self>) {
        if !self.map.opened {
            return;
        }
        let (Some(obj), Some(known), Some(lists)) =
            (self.object.clone(), self.related.map.clone(), self.map.lists.clone())
        else {
            return;
        };
        let (kind, kinds) = (self.obj.kind.clone(), self.kinds.clone());
        let work = cx.background_executor().spawn(async move { map::sections(&kind, &obj, &known, &lists, &kinds) });
        self.map.computing = true;
        self.map.compute.spawn(cx, work, |this, sections, cx| {
            this.map.sections = Some(Arc::new(sections));
            this.map.computing = false;
            cx.notify();
        });
    }

    pub(super) fn render_map(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(obj) = &self.object else { return div().into_any_element() };
        let ctx = self.ctx(obj, cx);
        let kind = &self.obj.kind;
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let tree = map::has_tree(kind);
        let mut col = v_flex().pt_2();
        if tree {
            col = col.children(crate::overview::relation_rows(&ctx, cx));
        }
        let note = |text: String, color: Hsla| div().py_1().text_xs().text_color(color).child(selectable(text));
        let known = self.related.map.as_deref();
        // Running: the page's first load, the tab's reads, or the relations being worked out.
        let running = known.is_none() || self.map.lists.is_none() || self.map.loading || self.map.computing;
        if running {
            col = col.child(note("Reading what links to it…".into(), muted));
        }
        // What could not be read, by the page or by the tab: the Map isn't the whole story.
        let errors: Vec<String> = known
            .map_or_default(|k| k.errors.clone())
            .into_iter()
            .chain(self.map.lists.iter().flat_map(|l| l.errors.clone()))
            .collect();
        if !errors.is_empty() {
            col = col.child(note(format!("Not fully known: {}", errors.join(" · ")), theme.danger));
        }
        let partial = self.map.lists.as_ref().map(|l| l.partial.join(" · ")).filter(|p| !p.is_empty());
        let partial_shown = partial.is_some();
        col = col.when_some(partial, |d, p| d.child(note(format!("Read in part: {p}"), muted)));
        // The kinds served decide which operators' resources are read (a Secret's Certificates,
        // a custom resource's siblings, HTTPRoutes): unknown on a context not connected.
        if self.kinds.is_empty() && self.obj.namespace.is_some() && !map::only_owners(kind) {
            col = col.child(note(
                "This cluster's kinds aren't known yet: operators' resources are not looked for.".into(),
                muted,
            ));
        }
        let sections: &[Section] = self.map.sections.as_deref().map_or(&[], Vec::as_slice);
        for (s, section) in sections.iter().enumerate() {
            col = col.child(
                div()
                    .pt_4()
                    .pb_1()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(selectable(section.rel.label().to_string())),
            );
            for (i, item) in section.items.iter().enumerate() {
                let id = format!("map-{s}-{i}");
                col = col.child(item_row(&ctx, item, 0, &id, cx));
                col = col.children(
                    item.under.iter().enumerate().map(|(j, p)| item_row(&ctx, p, 1, &format!("{id}-{j}"), cx)),
                );
                if item.more_under > 0 {
                    col = col.child(note(format!("and {} more", item.more_under), muted).pl(px(44.)));
                }
            }
            if section.more > 0 {
                col = col.child(note(format!("and {} more", section.more), muted).pl(px(22.)));
            }
        }
        // "Nothing" only when everything was read and worked out, and nothing failed.
        if sections.is_empty() && !running && errors.is_empty() && !partial_shown {
            let what = format!("{} {}", kind.ar.kind, self.obj.name);
            let said = if tree {
                format!("Nothing else found that links to {what}: no owner, PDB, NetworkPolicy, HPA or Ingress.")
            } else if map::only_owners(kind) {
                format!("Only owners are known for this kind, and {what} has none.")
            } else {
                format!("{what} has no owner, and nothing found that it owns or that refers to it.")
            };
            col = col.child(div().pt_4().text_sm().text_color(muted).child(selectable(said)));
            let looked = self.map.lists.as_ref().map(|l| l.read.join(", ")).filter(|r| !r.is_empty());
            col = col.when_some(looked, |d, read| {
                let ns = self.obj.namespace.clone().unwrap_or_default();
                d.child(note(format!("Looked through the {read} of namespace {ns}."), muted))
            });
        }
        v_flex().id("map").size_full().overflow_y_scroll().px_2().pb_8().child(col).into_any_element()
    }
}

fn icon_of(kind: &str) -> Lucide {
    match kind {
        "Pod" => Lucide::Box,
        "Secret" => Lucide::KeyRound,
        "ConfigMap" => Lucide::FileText,
        "Service" | "Ingress" | "HTTPRoute" | "NetworkPolicy" => Lucide::Network,
        "PersistentVolumeClaim" | "PersistentVolume" | "StorageClass" => Lucide::Database,
        "Node" => Lucide::Server,
        "ReplicaSet" | "Job" => Lucide::Layers,
        "ServiceAccount" => Lucide::Users,
        _ => Lucide::Boxes,
    }
}

/// One object: its kind, a link to it (with its namespace when not the page's), and its note.
/// `id` is its place on the tab: one object can be linked twice (a pod behind two Services).
fn item_row(ctx: &Ctx, item: &Item, depth: usize, id: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let shown = match &item.namespace {
        Some(ns) if Some(ns) != ctx.ns().as_ref() => format!("{ns}/{}", item.name),
        _ => item.name.clone(),
    };
    let note_color = if item.kind == "Pod" {
        crate::overview::state_color(item.note.as_deref().unwrap_or_default(), cx)
    } else {
        theme.muted_foreground
    };
    h_flex()
        .pl(px(depth as f32 * 22.))
        .py(px(3.))
        .gap_2()
        .text_sm()
        .child(div().w(px(14.)).text_color(theme.border).child(if depth == 0 { "" } else { "└" }))
        .child(Icon::new(icon_of(&item.kind)).size_4().text_color(theme.muted_foreground))
        .child(
            div()
                .w(px(150.))
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(selectable(item.kind.clone())),
        )
        .child(ctx.link_with_id(id.to_string(), shown, &item.link_kind(), &item.name, item.namespace.clone(), cx))
        .when_some(item.note.clone(), |d, n| d.child(div().text_xs().text_color(note_color).child(selectable(n))))
        .into_any_element()
}
