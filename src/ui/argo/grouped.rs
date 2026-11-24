//! The Applications picked on the Argo CD page, and their grouped diff: what a sync of each
//! would change now, the same change in several of them shown once with the Applications it
//! is in (see `k8s::argo::grouped_diff`).
//!
//! Applications are picked with their checkbox, a group at a time (a service's heading, or an
//! app of apps and those under it), or all those the filter keeps. The picks are kept by
//! Argo CD, namespace and name, so a new read of the list keeps them; `ArgoView::selected_apps`
//! gives those still listed, for every action on the picked Applications.
//!
//! Diff selected reads each one's managed resources, at most a few at a time, off the UI
//! thread, each kept by its resourceVersion; the changes are regrouped as each read ends.
//! Leaving the diff drops the reads running.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dashboard::{lines_block, link};
use super::{ArgoView, GroupBy, Key, ancestors, key};
use crate::k8s::argo::grouped_diff::{self, AppDiff, Cross, MAX_APPS};
use crate::k8s::argo::{FleetApp, Source, api};
use crate::smartdiff::SmartDiff;
use crate::ui::selectable::selectable;

/// Changes listed, at most.
const GROUPS_SHOWN: usize = 100;
/// Applications named on a change's line, at most.
const APPS_NAMED: usize = 6;
/// Applications unfolded under a change, at most.
const APPS_SHOWN: usize = 20;
/// Resources of one Application unfolded under a change, at most.
const RESOURCES_SHOWN: usize = 10;
/// Applications named in the lists of those in sync, with only noise, at most.
const LISTED: usize = 40;

/// What a read of one Application gave.
type Read = Result<Arc<AppDiff>, String>;

/// The grouped diff open on the page.
pub(super) struct GroupedDiff {
    /// The Applications read, in the page's order, with their name as shown.
    picked: Vec<(Key, String)>,
    /// Picked past `MAX_APPS`: not read.
    left_out: usize,
    /// Each one's read, once it ended.
    results: Vec<Option<Read>>,
    done: usize,
    /// The changes across them, and the noise by rule: worked out again as reads end.
    groups: Arc<Vec<Cross>>,
    noise_groups: Arc<Vec<Cross>>,
    /// Noise shown.
    noise: bool,
    /// The changes unfolded, by their text.
    open: HashSet<String>,
    load: crate::latest::Latest,
    regroup: crate::latest::Latest,
}

/// Each Application's name as shown: its name, and where it is read when two picked have
/// the same name.
fn labels(apps: &[FleetApp]) -> Vec<String> {
    let mut count: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for a in apps {
        *count.entry(a.app.name.as_str()).or_default() += 1;
    }
    apps.iter()
        .map(|a| {
            if count[a.app.name.as_str()] > 1 {
                format!("{} · {} {}", a.app.name, a.source.label(), a.app.namespace)
            } else {
                a.app.name.clone()
            }
        })
        .collect()
}

/// The first few of `names`, then how many more.
fn some(names: impl Iterator<Item = String>, at_most: usize) -> String {
    let all: Vec<String> = names.collect();
    let mut out: Vec<String> = all.iter().take(at_most).cloned().collect();
    if all.len() > at_most {
        out.push(format!("{} more", all.len() - at_most));
    }
    out.join(", ")
}

impl ArgoView {
    // MARK: Picking

    /// The Applications picked that are still listed, in the order of the list by name: what
    /// an action on the picked Applications (Diff selected, and Sync selected next to it)
    /// works on.
    pub(super) fn selected_apps(&self) -> Vec<FleetApp> {
        let Some(rows) = &self.rows else { return Vec::new() };
        rows.flat.iter().map(|&i| &rows.fleet.apps[i]).filter(|a| self.selected.contains(&key(a))).cloned().collect()
    }

    /// How many of the Applications listed are picked.
    pub(super) fn recount_picked(&mut self) {
        self.picked =
            self.rows.as_ref().map_or(0, |r| r.fleet.apps.iter().filter(|a| self.selected.contains(&key(a))).count());
    }

    pub(super) fn toggle_pick(&mut self, k: Key, cx: &mut Context<Self>) {
        if !self.selected.remove(&k) {
            self.selected.insert(k);
        }
        self.recount_picked();
        cx.notify();
    }

    /// Pick the Applications at `ixs` (in the rows); when all are picked already, unpick them.
    fn flip_picks(&mut self, ixs: &[usize], cx: &mut Context<Self>) {
        let Some(rows) = &self.rows else { return };
        let keys: Vec<Key> = ixs.iter().filter_map(|&i| rows.fleet.apps.get(i)).map(key).collect();
        let all = keys.iter().all(|k| self.selected.contains(k));
        for k in keys {
            if all {
                self.selected.remove(&k);
            } else {
                self.selected.insert(k);
            }
        }
        self.recount_picked();
        cx.notify();
    }

    /// The Applications of the group the line `ix` heads that the filter keeps: under the
    /// app of apps (it too), or of the service at `ix`.
    pub(super) fn group_members(&self, ix: usize, service: bool) -> Vec<usize> {
        let Some(rows) = &self.rows else { return Vec::new() };
        let kept = |i: usize| self.keeps.get(i).copied().unwrap_or(false);
        if service {
            return rows
                .services
                .get(ix)
                .map_or_else(Vec::new, |(_, m)| m.iter().copied().filter(|&i| kept(i)).collect());
        }
        (0..rows.fleet.apps.len())
            .filter(|&i| kept(i) && (i == ix || ancestors(&rows.parents, i).contains(&ix)))
            .collect()
    }

    /// Pick the group (or unpick it when all of it is picked).
    pub(super) fn pick_group(&mut self, ix: usize, service: bool, cx: &mut Context<Self>) {
        let members = self.group_members(ix, service);
        self.flip_picks(&members, cx);
    }

    /// Whether every Application the filter keeps of the group is picked.
    pub(super) fn group_picked(&self, ix: usize, service: bool) -> bool {
        let Some(rows) = &self.rows else { return false };
        let members = self.group_members(ix, service);
        !members.is_empty() && members.iter().all(|&i| self.selected.contains(&key(&rows.fleet.apps[i])))
    }

    /// Pick every Application the filter keeps.
    fn pick_filtered(&mut self, cx: &mut Context<Self>) {
        let Some(rows) = &self.rows else { return };
        for (i, a) in rows.fleet.apps.iter().enumerate() {
            if self.keeps.get(i).copied().unwrap_or(false) {
                self.selected.insert(key(a));
            }
        }
        self.recount_picked();
        cx.notify();
    }

    fn clear_picks(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        self.recount_picked();
        cx.notify();
    }

    /// Under the toolbar: pick all filtered, and with Applications picked, how many, Clear and
    /// Diff selected (the place for Sync selected, next).
    pub(super) fn selection_bar(&self, cx: &mut Context<Self>) -> Div {
        let n = self.picked;
        let pick_group_hint = match self.group_by {
            GroupBy::Parent => "Tick an Application, or its menu's Pick the group.",
            GroupBy::Service => "Tick an Application, or a service's heading for all of it.",
            GroupBy::None => "Tick the Applications to diff together.",
        };
        h_flex()
            .gap_2()
            .px_2()
            .child(
                Button::new("argo-pick-filtered")
                    .small()
                    .ghost()
                    .icon(Lucide::ListChecks)
                    .label(format!("Pick all filtered ({})", self.kept))
                    .disabled(self.kept == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.pick_filtered(cx))),
            )
            .when(n == 0, |h| h.child(div().text_xs().text_color(cx.theme().muted_foreground).child(pick_group_hint)))
            .when(n > 0, |h| {
                h.child(div().text_sm().child(format!("{} picked", crate::plural::count(n, "Application"))))
                    .child(
                        Button::new("argo-pick-clear")
                            .small()
                            .ghost()
                            .icon(Lucide::X)
                            .label("Clear")
                            .on_click(cx.listener(|this, _, _, cx| this.clear_picks(cx))),
                    )
                    .child(
                        Button::new("argo-diff-selected")
                            .small()
                            .primary()
                            .icon(Lucide::GitCompare)
                            .label("Diff selected")
                            .on_click(cx.listener(|this, _, _, cx| this.diff_selected(cx))),
                    )
            })
    }

    // MARK: Diff

    /// Read what a sync of each Application picked would change, at most a few at a time; the
    /// changes are regrouped as each read ends. A hub's Applications are read from the Argo CD
    /// server set for the hub in Settings (else each says why not).
    pub(super) fn diff_selected(&mut self, cx: &mut Context<Self>) {
        let mut apps = self.selected_apps();
        if apps.is_empty() {
            return;
        }
        let left_out = apps.len().saturating_sub(MAX_APPS);
        apps.truncate(MAX_APPS);
        let names = labels(&apps);
        let targets: Vec<(Source, grouped_diff::CacheKey)> = apps
            .iter()
            .map(|a| {
                let kept = (a.instance.clone(), a.app.namespace.clone(), a.app.name.clone(), a.version.clone());
                (a.source.clone(), kept)
            })
            .collect();
        let n = targets.len();
        let reads = grouped_diff::read_all(n, move |i| {
            let (source, kept) = targets[i].clone();
            let (ns, name) = (kept.1.clone(), kept.2.clone());
            let read: Pin<Box<dyn Future<Output = Read>>> = match source {
                Source::Hub(hub) => Box::pin(grouped_diff::read_hub(hub.context, ns, name)),
                Source::Api(endpoint) => Box::pin(api::sync_preview(endpoint, ns, name)),
            };
            grouped_diff::cached(kept, read)
        });
        let mut g = GroupedDiff {
            picked: apps.iter().map(key).zip(names).collect(),
            left_out,
            results: vec![None; n],
            done: 0,
            groups: Arc::default(),
            noise_groups: Arc::default(),
            noise: false,
            open: HashSet::new(),
            load: Default::default(),
            regroup: Default::default(),
        };
        g.load.spawn_each(cx, reads, |this: &mut Self, (i, read), cx| this.grouped_read(i, read, cx));
        self.grouped = Some(g);
        cx.notify();
    }

    /// The read of the Application at `i` ended: keep it, and regroup the changes off the UI
    /// thread (a newer regrouping replaces this one).
    fn grouped_read(&mut self, i: usize, read: Read, cx: &mut Context<Self>) {
        let Some(g) = &mut self.grouped else { return };
        let Some(slot) = g.results.get_mut(i) else { return };
        if slot.is_none() {
            g.done += 1;
        }
        *slot = Some(read);
        let diffs: Vec<Option<Arc<AppDiff>>> =
            g.results.iter().map(|r| r.as_ref().and_then(|r| r.as_ref().ok()).cloned()).collect();
        let work = cx.background_executor().spawn(async move {
            let refs: Vec<Option<&SmartDiff>> = diffs.iter().map(|d| d.as_ref().map(|d| &d.diff)).collect();
            (Arc::new(grouped_diff::regroup(&refs, false)), Arc::new(grouped_diff::regroup(&refs, true)))
        });
        g.regroup.spawn(cx, work, |this: &mut Self, (groups, noise), cx| {
            if let Some(g) = &mut this.grouped {
                (g.groups, g.noise_groups) = (groups, noise);
            }
            cx.notify();
        });
        cx.notify();
    }

    /// Back to the list; the reads running are dropped. The picks stay.
    fn close_grouped(&mut self, cx: &mut Context<Self>) {
        self.grouped = None;
        cx.notify();
    }

    fn update_grouped(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut GroupedDiff)) {
        if let Some(g) = &mut self.grouped {
            f(g);
        }
        cx.notify();
    }

    // MARK: Rendering

    /// The grouped diff: how far the reads are, the summary, the changes each once with the
    /// Applications they are in (unfolded: each one's lines), the noise hidden and counted,
    /// then those with nothing to sync and those that could not be read.
    pub(super) fn grouped_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = &self.grouped else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let n = g.picked.len();
        let name = |i: usize| g.picked.get(i).map_or_else(String::new, |(_, l)| l.clone());
        let ok: Vec<(usize, &Arc<AppDiff>)> =
            g.results.iter().enumerate().filter_map(|(i, r)| Some((i, r.as_ref()?.as_ref().ok()?))).collect();
        let errors: Vec<(usize, &String)> =
            g.results.iter().enumerate().filter_map(|(i, r)| Some((i, r.as_ref()?.as_ref().err()?))).collect();
        let in_sync: Vec<usize> = ok.iter().filter(|(_, d)| d.in_sync()).map(|(i, _)| *i).collect();
        let noise_only: Vec<usize> = ok.iter().filter(|(_, d)| d.noise_only()).map(|(i, _)| *i).collect();
        let with_changes = ok.iter().filter(|(_, d)| !d.diff.groups.is_empty()).count();
        let noise_total: usize = ok.iter().map(|(_, d)| d.diff.noise()).sum();
        let hooks: usize = ok.iter().map(|(_, d)| d.hooks).sum();
        let changed: usize =
            ok.iter().map(|(_, d)| d.diff.resources.iter().filter(|r| !r.facts.is_empty()).count()).sum();
        let same: usize = ok.iter().map(|(_, d)| d.same).sum();
        let reading = g.done < n;
        let header = h_flex()
            .gap_2()
            .child(
                Button::new("argo-grouped-back")
                    .small()
                    .ghost()
                    .icon(Lucide::ArrowLeft)
                    .label("Applications")
                    .on_click(cx.listener(|this, _, _, cx| this.close_grouped(cx))),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Diff of {}", crate::plural::count(n, "Application"))),
            )
            .child(div().flex_1())
            .when(reading, |h| h.child(div().text_xs().text_color(muted).child(format!("Read {} of {n}…", g.done))))
            .child(
                Button::new("argo-grouped-again")
                    .small()
                    .outline()
                    .icon(Lucide::RefreshCw)
                    .label("Read again")
                    .loading(reading)
                    .disabled(reading)
                    .on_click(cx.listener(|this, _, _, cx| this.diff_selected(cx))),
            );
        let summary = grouped_diff::summary(&g.groups, with_changes);
        let nothing = !reading && summary.is_none() && errors.is_empty();
        let noise_line = format!(
            "{} {}",
            crate::plural::count(noise_total, "noise change"),
            if g.noise { "shown" } else { "hidden" }
        );
        let more_groups = g.groups.len().saturating_sub(GROUPS_SHOWN);
        let read_ok = ok.len();
        let body = v_flex()
            .gap_2()
            .child(div().text_xs().text_color(muted).child(
                "What a sync of each would change now: its live objects against what Argo CD would apply (its \
                 managed resources). The same change in several Applications is one line; unfold it to see each \
                 one's diff.",
            ))
            .when(g.left_out > 0, |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!("{} more picked are not read: at most {MAX_APPS} at a time.", g.left_out)),
                )
            })
            .children(summary.map(|s| div().text_sm().font_weight(FontWeight::MEDIUM).child(selectable(s))))
            .when(nothing, |v| v.child(div().text_sm().text_color(muted).child("A sync would change nothing.")))
            .when(read_ok > 0, |v| {
                v.child(div().text_xs().text_color(muted).child(format!(
                    "{} would change in {} read, {} as git has them already",
                    crate::plural::count(changed, "resource"),
                    crate::plural::count(read_ok, "Application"),
                    same
                )))
            })
            .when(noise_total > 0, |v| {
                v.child(
                    h_flex().gap_2().text_xs().text_color(muted).child(noise_line).child(
                        link("argo-grouped-noise", if g.noise { "hide" } else { "show" }, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.update_grouped(cx, |g| g.noise = !g.noise))),
                    ),
                )
            })
            .when(hooks > 0, |v| {
                v.child(div().text_xs().text_color(muted).child(format!(
                    "{} left out: they run during a sync rather than being kept in sync.",
                    crate::plural::count(hooks, "hook")
                )))
            })
            .children(
                g.groups
                    .iter()
                    .take(GROUPS_SHOWN)
                    .enumerate()
                    .map(|(i, c)| self.cross_row(g, "change", i, c, read_ok, cx)),
            )
            .when(more_groups > 0, |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("… {} more", crate::plural::count(more_groups, "change"))),
                )
            })
            .when(g.noise && !g.noise_groups.is_empty(), |v| {
                v.child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child("NOISE")).children(
                    g.noise_groups
                        .iter()
                        .take(GROUPS_SHOWN)
                        .enumerate()
                        .map(|(i, c)| self.cross_row(g, "noise", i, c, read_ok, cx)),
                )
            })
            .when(!in_sync.is_empty(), |v| {
                v.child(div().text_xs().text_color(muted).child(selectable(format!(
                    "Nothing to sync ({}): {}",
                    in_sync.len(),
                    some(in_sync.iter().map(|&i| name(i)), LISTED)
                ))))
            })
            .when(!noise_only.is_empty(), |v| {
                v.child(div().text_xs().text_color(muted).child(selectable(format!(
                    "Only noise to sync ({}): {}",
                    noise_only.len(),
                    some(noise_only.iter().map(|&i| name(i)), LISTED)
                ))))
            })
            .when(!errors.is_empty(), |v| {
                v.child(
                    v_flex()
                        .gap_0p5()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(format!("Could not be read ({}):", errors.len()))
                        .children(errors.iter().map(|(i, e)| div().child(selectable(format!("{}: {e}", name(*i)))))),
                )
            });
        v_flex()
            .size_full()
            .gap_2()
            .child(header)
            .child(div().id("argo-grouped-body").flex_1().min_h_0().overflow_y_scroll().p_1().child(body))
            .into_any_element()
    }

    /// A change: its line with the Applications it is in (all of them, or which), and
    /// unfolded, each one's resources with their diff.
    fn cross_row(
        &self,
        g: &GroupedDiff,
        kind: &'static str,
        i: usize,
        c: &Cross,
        read_ok: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let muted = cx.theme().muted_foreground;
        let open = g.open.contains(&c.text);
        let name = |a: usize| g.picked.get(a).map_or_else(String::new, |(_, l)| l.clone());
        let where_ = if c.apps.len() == read_ok && read_ok > 1 {
            format!("in all {read_ok}")
        } else {
            format!("in {} of {read_ok}: {}", c.apps.len(), some(c.apps.iter().map(|(a, _)| name(*a)), APPS_NAMED))
        };
        let text = c.text.clone();
        let label = format!("{} {} ({})", if open { "▾" } else { "▸" }, c.text, c.apps.len());
        let toggle = cx.listener(move |this, _, _, cx| {
            let text = text.clone();
            this.update_grouped(cx, move |g| {
                if !g.open.remove(&text) {
                    g.open.insert(text);
                }
            });
        });
        let more_apps = c.apps.len().saturating_sub(APPS_SHOWN);
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(link(SharedString::from(format!("argo-grouped-{kind}-{i}")), &label, cx).on_click(toggle))
                    .child(div().text_xs().text_color(muted).child(selectable(where_))),
            )
            .when(open, |v| {
                v.child(
                    v_flex()
                        .pl_4()
                        .gap_1p5()
                        .children(c.apps.iter().take(APPS_SHOWN).map(|(a, resources)| {
                            let diff = g.results.get(*a).and_then(|r| r.as_ref()?.as_ref().ok());
                            let more = resources.len().saturating_sub(RESOURCES_SHOWN);
                            let mut shown: Vec<Div> = Vec::new();
                            if let Some(d) = diff {
                                for r in resources.iter().take(RESOURCES_SHOWN).filter_map(|r| d.diff.resources.get(*r))
                                {
                                    shown.push(
                                        v_flex()
                                            .gap_0p5()
                                            .child(div().text_xs().text_color(muted).child(r.key.label()))
                                            .child(lines_block(r.lines(g.noise), cx)),
                                    );
                                }
                            }
                            v_flex()
                                .gap_0p5()
                                .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).child(selectable(name(*a))))
                                .children(shown)
                                .when(more > 0, |v| {
                                    v.child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(format!("… {} more", crate::plural::count(more, "resource"))),
                                    )
                                })
                        }))
                        .when(more_apps > 0, |v| {
                            v.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(format!("… {} more", crate::plural::count(more_apps, "Application"))),
                            )
                        }),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::some;

    #[test]
    fn a_long_list_of_names_says_how_many_more() {
        let names = |n: usize| (0..n).map(|i| format!("env{i}"));
        assert_eq!(some(names(2), 3), "env0, env1");
        assert_eq!(some(names(5), 3), "env0, env1, env2, 2 more");
    }
}
