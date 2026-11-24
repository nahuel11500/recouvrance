//! Window chrome: context picker page, context strip, command palette, zoom.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::search_index::{Entry, SEARCH_KINDS, on_cluster};
use super::{AppView, Route, section_icon, set_zoom, zoom};
use crate::k8s;
use crate::k8s::Kind;
use crate::ui::palette::{Item as PaletteItem, Palette, PaletteEvent, Target};
use crate::ui::settings_view::SettingsView;

impl AppView {
    pub(super) fn zoom_by(&mut self, delta: Option<f32>, window: &mut Window, cx: &mut Context<Self>) {
        let z = delta.map_or(1.0, |d| zoom(cx) + d);
        set_zoom(z, window, cx);
        // Column widths scale too; they are computed when the table refreshes.
        self.table.update(cx, component::table::TableState::refresh);
    }

    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Commands first (pages, keys, toggles), then each Settings section and saved view.
        let mut items: Vec<PaletteItem> = super::commands::palette_items().collect();
        for (section, name, icon) in SettingsView::sections() {
            items.push(PaletteItem::new(
                format!("Settings → {name}"),
                String::new(),
                *icon,
                Target::Settings(*section),
            ));
        }
        for v in self.favorites.read(cx).views() {
            let detail = format!("saved view · {}", v.kind.split('.').next().unwrap_or_default());
            items.push(PaletteItem::new(v.name.clone(), detail, Lucide::Bookmark, Target::View(v.clone())));
        }
        // The types the sidebar lists, not the noise discovery keeps for lookups.
        for k in self.kinds.iter().filter(|k| k8s::in_sidebar(k)) {
            items.push(PaletteItem::new(
                k.label.clone(),
                format!("{} · {}", k.kubectl_name(), k.section),
                section_icon(k.section),
                Target::Kind(k.clone()),
            ));
        }
        for c in &self.all_contexts {
            items.push(PaletteItem::new(c.clone(), "switch context".into(), Lucide::Ship, Target::Context(c.clone())));
        }
        // Each context set with a context left in the kubeconfig: picking it connects them all.
        for set in crate::settings::context_sets() {
            let contexts = set.available(&self.all_contexts);
            if contexts.is_empty() {
                continue;
            }
            let detail = format!("context set · {}", contexts.join(", "));
            let mut item = PaletteItem::new(set.name, detail, Lucide::Layers, Target::ContextSet(contexts));
            item.also = "set workspace group connect";
            items.push(item);
        }
        // With several clusters, objects say which one they are on.
        let names: std::rc::Rc<Vec<String>> = std::rc::Rc::new(self.clusters.iter().map(|(c, _)| c.clone()).collect());
        // Every favorite, on any context: picking one opens it where it points (see
        // `open_favorite`); `favorite` finds them all.
        let favorites = self.favorites.read(cx).all();
        let standings = self.favorite_standings(&favorites, cx);
        for (f, standing) in favorites.into_iter().zip(standings) {
            let detail = match standing.note() {
                Some(note) => format!("★ {} · {note}", f.what()),
                None => format!("★ {}", f.what()),
            };
            let mut item = PaletteItem::new(f.title(), detail, Lucide::Star, Target::Favorite(f));
            item.also = "favorite favourite starred";
            items.push(item);
        }
        // Objects of the current table.
        if let Some(kind) = &self.kind {
            let t = self.table.read(cx).delegate();
            for ix in 0..t.rows_count_visible().min(5000) {
                let (Some(row), Some(ctx)) = (t.row(ix), t.row_context(ix)) else { continue };
                items.push(PaletteItem::new(
                    row.name.clone(),
                    format!(
                        "{} · {}{}",
                        kind.label,
                        row.namespace.clone().unwrap_or_default(),
                        on_cluster(&names, ctx)
                    ),
                    section_icon(kind.section),
                    Target::Object {
                        ctx,
                        kind: kind.kubectl_name(),
                        name: row.name.clone(),
                        namespace: row.namespace.clone(),
                    },
                ));
            }
        }
        items.extend(self.search_index.entries().iter().map(|e| e.item(&names)));
        let palette = cx.new(|cx| Palette::new(items, window, cx));
        // (Re)build the cluster-wide name index in the background; results stream in.
        let kinds: Vec<Kind> = SEARCH_KINDS.iter().filter_map(|k| k8s::find_kind(&self.kinds, k).cloned()).collect();
        // None of the searched kinds served (or not discovered yet): nothing to index, and
        // no "indexing…" that nothing would clear.
        if self.search_index.is_stale() && !self.clusters.is_empty() && !kinds.is_empty() {
            self.search_index.restart(self.clusters.len() * kinds.len());
            let status = self.search_index.status();
            palette.update(cx, |p, cx| p.set_status(status, cx));
            for (ctx, (cluster, client)) in self.clusters.clone().into_iter().enumerate() {
                for kind in kinds.clone() {
                    let (client, palette, names) = (client.clone(), palette.downgrade(), names.clone());
                    let cluster = cluster.clone();
                    let task = cx.spawn(async move |this, cx| {
                        // A list that fails is said, not taken for an empty one.
                        let (found, failed) = match k8s::search_names(client, kind.clone()).await {
                            Ok(found) => (found, None),
                            Err(_) => (Vec::new(), Some(format!("{} on {cluster}", kind.kubectl_name()))),
                        };
                        let entries: Vec<Entry> = found
                            .into_iter()
                            .map(|(namespace, name)| Entry {
                                ctx,
                                kind: kind.kubectl_name(),
                                label: kind.label.clone(),
                                icon: section_icon(kind.section),
                                namespace,
                                name,
                            })
                            .collect();
                        let new_items: Vec<PaletteItem> = entries.iter().map(|e| e.item(&names)).collect();
                        let Ok(status) = this.update(cx, |this, _| this.search_index.add(entries, failed)) else {
                            return;
                        };
                        let _ = palette.update(cx, |p, cx| {
                            p.add_items(new_items, cx);
                            p.set_status(status, cx);
                        });
                    });
                    self.search_index.push_task(task);
                }
            }
        } else if let Some(status) = self.search_index.status() {
            palette.update(cx, |p, cx| p.set_status(Some(status), cx));
        }
        // Objects in the palette point at clusters by their place in the connected list: a
        // reconnect in between would open them on another cluster.
        let opened_on: Vec<String> = self.clusters.iter().map(|(c, _)| c.clone()).collect();
        self._palette_sub = Some(cx.subscribe_in(&palette, window, move |this, _, ev: &PaletteEvent, window, cx| {
            this.palette = None;
            window.focus(&this.focus, cx);
            if let PaletteEvent::Pick(target) = ev {
                match target.clone() {
                    Target::Kind(k) => this.select_kind(k, cx),
                    // Run as if its keys were pressed, with the window focused again.
                    Target::Action(action) => window.dispatch_action(action(), cx),
                    Target::Settings(section) => this.open_settings_section(section, window, cx),
                    Target::View(view) => this.apply_view(&view, window, cx),
                    Target::Context(c) => this.connect_many(vec![c], window, cx),
                    Target::ContextSet(contexts) => {
                        // As the picker does, so its ticks match what connects.
                        this.picked = contexts.iter().cloned().collect();
                        this.connect_many(contexts, window, cx);
                    }
                    Target::Favorite(f) => this.open_favorite(f, true, window, cx),
                    Target::Object { .. } if this.clusters.iter().map(|(c, _)| c).ne(opened_on.iter()) => {
                        window.push_notification(
                            Notification::warning(
                                "The connected clusters changed since the palette opened: search again",
                            ),
                            cx,
                        );
                    }
                    Target::Object { ctx, kind, name, namespace } => {
                        this.open_object_in(ctx, &kind, name, namespace, window, cx);
                    }
                }
            }
            cx.notify();
        }));
        self.palette = Some(palette);
        cx.notify();
    }

    /// A thin bar in the connected contexts' colors, so a colored (e.g. prod) cluster is obvious.
    pub(super) fn context_strip(&self) -> Option<AnyElement> {
        let colors: Vec<u32> = self.clusters.iter().filter_map(|c| crate::settings::context_color(&c.0)).collect();
        if colors.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .h(px(3.))
                .w_full()
                .children(colors.into_iter().map(|c| div().flex_1().h_full().bg(rgb(c))))
                .into_any_element(),
        )
    }
}

/// How long an approval still waits, in its banner: "refused in 1:42". Rounded up, so it
/// never says 0:00 while the request still waits.
pub(super) fn refused_in(left: std::time::Duration) -> String {
    let secs = left.as_millis().div_ceil(1000);
    format!("refused in {}:{:02}", secs / 60, secs % 60)
}

impl AppView {
    pub(super) fn toggle_context(&mut self, context: String, cx: &mut Context<Self>) {
        if !self.picked.remove(&context) {
            self.picked.insert(context);
        }
        cx.notify();
    }

    pub(super) fn connect_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Keep the kubeconfig order so the primary context is predictable.
        let contexts: Vec<String> = self.all_contexts.iter().filter(|c| self.picked.contains(*c)).cloned().collect();
        self.connect_many(contexts, window, cx);
    }

    /// The context sets (saved from the top bar's picker), one button each that connects the
    /// set's contexts the kubeconfig still has. None when there is no set.
    fn context_set_buttons(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sets = crate::settings::context_sets();
        if sets.is_empty() {
            return None;
        }
        let buttons = sets.into_iter().enumerate().map(|(i, set)| {
            let contexts = set.available(&self.all_contexts);
            Button::new(("ctx-set-button", i))
                .outline()
                .icon(Lucide::Layers)
                .label(set.name)
                .tooltip(if contexts.is_empty() {
                    "None of its contexts is in the kubeconfig".to_string()
                } else {
                    contexts.join(", ")
                })
                .disabled(contexts.is_empty())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.picked = contexts.iter().cloned().collect();
                    this.connect_many(contexts.clone(), window, cx);
                }))
        });
        Some(h_flex().flex_wrap().gap_2().children(buttons).into_any_element())
    }

    pub(super) fn render_contexts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex().size_full().items_center().justify_center().child(
            v_flex()
                .w(px(900.))
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .child(crate::ui::logo(px(40.)))
                        .child(div().text_2xl().font_weight(FontWeight::BOLD).child("Recouvrance")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Select one or more contexts to get started"),
                )
                .children(self.context_set_buttons(cx))
                .child(h_flex().flex_wrap().gap_2().children(self.all_contexts.iter().enumerate().map(|(i, ctx)| {
                    let name = ctx.clone();
                    let risky = crate::settings::is_protected(ctx);
                    let accent: Hsla = crate::settings::context_color(ctx).map_or(theme.blue, |c| rgb(c).into());
                    let picked = self.picked.contains(ctx);
                    // A context of the set still connecting or that failed says so (and why).
                    let (state, state_color, why) = match self.conn.get(ctx) {
                        Some(super::conn::Conn::Connecting) => (Some("connecting…"), theme.muted_foreground, None),
                        Some(super::conn::Conn::Failed(why)) => {
                            (Some("can't connect"), theme.danger, Some(why.clone()))
                        }
                        None => (None, theme.muted_foreground, None),
                    };
                    // A shortened name shows in full on hover, above why it can't connect.
                    let short = k8s::short_names::short_name(ctx);
                    let tip = match (short != *ctx, why) {
                        (true, Some(why)) => Some(SharedString::from(format!("{ctx}\n{why}"))),
                        (true, None) => Some(SharedString::from(ctx.clone())),
                        (false, why) => why,
                    };
                    h_flex()
                        .id(("ctx-card", i))
                        .w(px(290.))
                        .gap_3()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(if picked { theme.blue } else { theme.border })
                        .when(picked, |d| d.bg(theme.blue.opacity(0.08)))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                            // Double click connects to just this one.
                            if ev.click_count() >= 2 {
                                this.picked = [name.clone()].into();
                                this.connect_picked(window, cx);
                            } else {
                                this.toggle_context(name.clone(), cx);
                            }
                        }))
                        .child(Icon::new(Lucide::Ship).size_6().text_color(accent))
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(short),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .text_xs()
                                        .child(
                                            div()
                                                .text_color(if risky { theme.warning } else { theme.muted_foreground })
                                                .child(if risky { "protected" } else { "context" }),
                                        )
                                        .when_some(state, |d, s| d.child(div().text_color(state_color).child(s))),
                                ),
                        )
                        .when_some(tip, |d, tip| {
                            d.tooltip(move |w, cx| component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
                        })
                })))
                .child(
                    h_flex()
                        .justify_center()
                        .gap_2()
                        .pt_4()
                        .child(
                            Button::new("add-kubeconfig")
                                .ghost()
                                .icon(Lucide::Plus)
                                .label("Add a kubeconfig")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings_view.update(cx, SettingsView::show_kubeconfig);
                                    this.navigate(Route::Settings, cx);
                                })),
                        )
                        .child(
                            Button::new("connect")
                                .primary()
                                .label(match self.picked.len() {
                                    0 => "Connect".to_string(),
                                    1 => "Connect →".to_string(),
                                    n => format!("Connect to {n} contexts →"),
                                })
                                .disabled(self.picked.is_empty())
                                .on_click(cx.listener(|this, _, window, cx| this.connect_picked(window, cx))),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::refused_in;

    /// The seconds a countdown text shows.
    fn shown(text: &str) -> u64 {
        let (min, sec) = text.strip_prefix("refused in ").unwrap().split_once(':').unwrap();
        assert_eq!(sec.len(), 2, "{text}");
        min.parse::<u64>().unwrap() * 60 + sec.parse::<u64>().unwrap()
    }

    #[test]
    fn the_countdown_reads_as_minutes_and_seconds() {
        assert_eq!(refused_in(Duration::from_secs(120)), "refused in 2:00");
        assert_eq!(refused_in(Duration::from_millis(101_200)), "refused in 1:42");
        assert_eq!(refused_in(Duration::from_secs(59)), "refused in 0:59");
        assert_eq!(refused_in(Duration::from_millis(300)), "refused in 0:01");
        assert_eq!(refused_in(Duration::ZERO), "refused in 0:00");
    }

    proptest! {
        /// The countdown shows the time left rounded up to the second: never 0:00 while
        /// the request waits, never more than a second over, and it never goes up as time
        /// passes.
        #[test]
        fn the_countdown_rounds_up_and_only_goes_down(a in 0u64..=180_000, b in 0u64..=180_000) {
            let (early, late) = (a.max(b), a.min(b));
            let s = shown(&refused_in(Duration::from_millis(early)));
            prop_assert!(s * 1000 >= early && s * 1000 < early + 1000);
            prop_assert!(shown(&refused_in(Duration::from_millis(late))) <= s);
            prop_assert_eq!(s == 0, early == 0);
        }
    }
}
