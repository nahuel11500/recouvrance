//! The Map tab of every detail page (`crate::map`): a pod's or workload's tree, then what the
//! object is linked to, read when the tab opens.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::DetailPanel;
use crate::map::{self, Item, Lists};
use crate::overview::Ctx;
use crate::ui::selectable::selectable;

/// What the tab read when it was last opened.
#[derive(Default)]
pub(super) struct MapState {
    lists: Option<Lists>,
    loading: bool,
    load: crate::latest::Latest,
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
        self.map.loading = true;
        self.map.load.spawn(cx, work, |this, lists, cx| {
            this.map.lists = Some(lists);
            this.map.loading = false;
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
        let sections = map::sections(kind, obj, &self.related, self.map.lists.as_ref());
        let note = |text: String, color: Hsla| div().py_1().text_xs().text_color(color).child(selectable(text));
        if self.map.loading {
            col = col.child(note("Reading what links to it…".into(), muted));
        }
        if let Some(lists) = &self.map.lists
            && !lists.errors.is_empty()
        {
            col = col.child(note(format!("Not fully known: {}", lists.errors.join(" · ")), theme.danger));
        }
        for section in &sections {
            col = col.child(
                div()
                    .pt_4()
                    .pb_1()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(selectable(section.rel.label().to_string())),
            );
            for item in &section.items {
                col = col.child(item_row(&ctx, item, 0, cx));
                col = col.children(item.under.iter().map(|p| item_row(&ctx, p, 1, cx)));
            }
            if section.more > 0 {
                col = col.child(note(format!("and {} more", section.more), muted).pl(px(22.)));
            }
        }
        if sections.is_empty() && !self.map.loading {
            let what = format!("{} {}", kind.ar.kind, self.obj.name);
            let said = if tree {
                format!("Nothing else found that links to {what}: no owner, PDB, NetworkPolicy, HPA or Ingress.")
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
fn item_row(ctx: &Ctx, item: &Item, depth: usize, cx: &App) -> AnyElement {
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
        .child(ctx.link_in(shown, &item.link_kind(), &item.name, item.namespace.clone(), cx))
        .when_some(item.note.clone(), |d, n| d.child(div().text_xs().text_color(note_color).child(selectable(n))))
        .into_any_element()
}
