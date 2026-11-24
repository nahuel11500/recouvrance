//! The Applications picked on the Argo CD page, and their grouped diff: what a sync of each
//! would change now, the same change in several of them shown once with the Applications it
//! is in (see `k8s::argo::grouped_diff`).
//!
//! Applications are picked with their checkbox, a group at a time (a service's heading, or an
//! app of apps and those under it), or all those the filter keeps. The picks are kept by
//! Argo CD, namespace and name, so a new read of the list keeps them. An action on the picked
//! Applications works on those the filter keeps (`ArgoView::selected_apps`): those it hides
//! are counted next to the picks, never acted on.
//!
//! Diff selected reads each one's managed resources, at most a few at a time, off the UI
//! thread, each kept a short while by its resourceVersion (Read again reads anew); the changes
//! are regrouped as reads end, at most every 150 ms. An Application Argo CD couldn't compare
//! is not read: it says why. Leaving the diff drops the reads running.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dashboard::{lines_block, link};
use super::{ArgoView, GroupBy, Key, ancestors, key};
use crate::k8s::argo::grouped_diff::{self, AppDiff, Cross, CrossKey, MAX_APPS};
use crate::k8s::argo::{FleetApp, Source, api};
use crate::smartdiff::SmartDiff;
use crate::ui::selectable::selectable;

/// Changes listed, at most.
const GROUPS_SHOWN: usize = 100;
/// Applications named on a change's or an error's line, at most.
const APPS_NAMED: usize = 6;
/// Applications unfolded under a change, at most.
const APPS_SHOWN: usize = 20;
/// Resources of one Application unfolded under a change, at most.
const RESOURCES_SHOWN: usize = 10;
/// Applications named in the lists of those in sync, with only noise, at most.
const LISTED: usize = 40;
/// Errors listed, at most (each with its Applications).
const ERRORS_SHOWN: usize = 20;
/// The changes are regrouped at most this often while reads end.
const REGROUP_EVERY: Duration = Duration::from_millis(150);

/// What a read of one Application gave.
type Read = Result<Arc<AppDiff>, String>;

/// What is known of one Application of the diff.
#[derive(Clone)]
enum Outcome {
    Diff(Arc<AppDiff>),
    Failed(String),
    /// Argo CD couldn't compare it with git: why. Not read.
    NotCompared(String),
}

/// The grouped diff open on the page.
pub(super) struct GroupedDiff {
    /// The Applications read, in the order of the list by name, with their name as shown.
    picked: Vec<(Key, String)>,
    /// Picked past `MAX_APPS`: not read.
    left_out: usize,
    /// What each one gave, once known.
    results: Vec<Option<Outcome>>,
    done: usize,
    /// The changes across them, and the noise by rule.
    groups: Arc<Vec<Cross>>,
    noise_groups: Arc<Vec<Cross>>,
    /// A regrouping waits or runs; results came in since it took them; when the last ended.
    regrouping: bool,
    regroup_again: bool,
    regrouped_at: Option<Instant>,
    /// Noise shown.
    noise: bool,
    /// The changes unfolded.
    open: HashSet<CrossKey>,
    load: crate::latest::Latest,
    regroup: crate::latest::Latest,
}

impl GroupedDiff {
    fn diff(&self, i: usize) -> Option<&Arc<AppDiff>> {
        match self.results.get(i)? {
            Some(Outcome::Diff(d)) => Some(d),
            _ => None,
        }
    }
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

// MARK: Picking, pure

/// The Applications (indexes) of a group the filter keeps (`keeps`): the service at `ix` of
/// `services`, or the app of apps at `ix` and every one under it (by `parents`).
pub(super) fn members(
    services: &[(String, Vec<usize>)],
    parents: &[Option<usize>],
    keeps: &[bool],
    ix: usize,
    service: bool,
) -> Vec<usize> {
    let kept = |i: usize| keeps.get(i).copied().unwrap_or(false);
    if service {
        return services.get(ix).map_or_else(Vec::new, |(_, m)| m.iter().copied().filter(|&i| kept(i)).collect());
    }
    (0..parents.len()).filter(|&i| kept(i) && (i == ix || ancestors(parents, i).contains(&ix))).collect()
}

/// Pick every one of `keys`; when all are picked already, unpick them.
pub(super) fn flip(selected: &mut HashSet<Key>, keys: Vec<Key>) {
    let all = keys.iter().all(|k| selected.contains(k));
    for k in keys {
        if all {
            selected.remove(&k);
        } else {
            selected.insert(k);
        }
    }
}

/// Of the Applications listed (`listed`: index and key, in the list's order), those picked
/// that the filter keeps (their index, in order), and how many picked it hides.
pub(super) fn split_picks(listed: &[(usize, Key)], keeps: &[bool], selected: &HashSet<Key>) -> (Vec<usize>, usize) {
    let (mut shown, mut hidden) = (Vec::new(), 0);
    for (i, _) in listed.iter().filter(|(_, k)| selected.contains(k)) {
        if keeps.get(*i).copied().unwrap_or(false) {
            shown.push(*i);
        } else {
            hidden += 1;
        }
    }
    (shown, hidden)
}

impl ArgoView {
    // MARK: Picking

    /// The Applications listed, in the order of the list by name, with their key.
    fn listed(&self) -> Vec<(usize, Key)> {
        self.rows.as_ref().map_or_else(Vec::new, |r| r.flat.iter().map(|&i| (i, key(&r.fleet.apps[i]))).collect())
    }

    /// The Applications picked that the filter keeps, in the order of the list by name: what
    /// an action on the picked Applications (Diff selected, and Sync selected next to it)
    /// works on. Those the filter hides are left out (and counted on the page).
    pub(super) fn selected_apps(&self) -> Vec<FleetApp> {
        let Some(rows) = &self.rows else { return Vec::new() };
        let (shown, _) = split_picks(&self.listed(), &self.keeps, &self.selected);
        shown.into_iter().map(|i| rows.fleet.apps[i].clone()).collect()
    }

    /// How many of the Applications listed are picked and kept by the filter, and how many
    /// it hides.
    pub(super) fn recount_picked(&mut self) {
        let (shown, hidden) = split_picks(&self.listed(), &self.keeps, &self.selected);
        (self.picked, self.picked_hidden) = (shown.len(), hidden);
    }

    pub(super) fn toggle_pick(&mut self, k: Key, cx: &mut Context<Self>) {
        flip(&mut self.selected, vec![k]);
        self.recount_picked();
        cx.notify();
    }

    fn group_members(&self, ix: usize, service: bool) -> Vec<usize> {
        let Some(rows) = &self.rows else { return Vec::new() };
        members(&rows.services, &rows.parents, &self.keeps, ix, service)
    }

    /// Pick the group (or unpick it when all of it is picked).
    pub(super) fn pick_group(&mut self, ix: usize, service: bool, cx: &mut Context<Self>) {
        let Some(rows) = &self.rows else { return };
        let keys = self.group_members(ix, service).into_iter().map(|i| key(&rows.fleet.apps[i])).collect();
        flip(&mut self.selected, keys);
        self.recount_picked();
        cx.notify();
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

    /// Under the toolbar: pick all filtered, and with Applications picked, how many (and how
    /// many the filter hides), Clear and Diff selected (the place for Sync selected, next).
    pub(super) fn selection_bar(&self, cx: &mut Context<Self>) -> Div {
        let (n, hidden) = (self.picked, self.picked_hidden);
        let muted = cx.theme().muted_foreground;
        let pick_group_hint = match self.group_by {
            GroupBy::Parent => "Tick an Application, or its menu's Pick the group.",
            GroupBy::Service => "Tick an Application, or a service's heading for all of it.",
            GroupBy::None => "Tick the Applications to diff together.",
        };
        let picked = if hidden > 0 {
            format!("{} picked ({hidden} hidden by the filter)", crate::plural::count(n, "Application"))
        } else {
            format!("{} picked", crate::plural::count(n, "Application"))
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
            .when(n + hidden == 0, |h| h.child(div().text_xs().text_color(muted).child(pick_group_hint)))
            .when(n + hidden > 0, |h| {
                h.child(div().text_sm().child(picked))
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
                            .disabled(n == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.diff_selected(false, cx))),
                    )
            })
    }

    // MARK: Diff

    /// Read what a sync of each Application picked would change, at most a few at a time; the
    /// changes are regrouped as reads end. One Argo CD couldn't compare is not read. A hub's
    /// Applications are read from the Argo CD server set for the hub in Settings (else each
    /// says why not). `fresh` (Read again): none is taken from those kept.
    pub(super) fn diff_selected(&mut self, fresh: bool, cx: &mut Context<Self>) {
        let mut apps = self.selected_apps();
        if apps.is_empty() {
            return;
        }
        let left_out = apps.len().saturating_sub(MAX_APPS);
        apps.truncate(MAX_APPS);
        let names = labels(&apps);
        let mut results: Vec<Option<Outcome>> = vec![None; apps.len()];
        let mut targets: Vec<(usize, Source, grouped_diff::CacheKey)> = Vec::new();
        for (i, a) in apps.iter().enumerate() {
            if let Some(why) = grouped_diff::not_compared(&a.app) {
                results[i] = Some(Outcome::NotCompared(why));
                continue;
            }
            let kept = (a.instance.clone(), a.app.namespace.clone(), a.app.name.clone(), a.version.clone());
            targets.push((i, a.source.clone(), kept));
        }
        let done = apps.len() - targets.len();
        let reads = grouped_diff::read_all(targets.len(), move |t| {
            let (i, source, kept) = targets[t].clone();
            let (ns, name) = (kept.1.clone(), kept.2.clone());
            let read: Pin<Box<dyn Future<Output = Read>>> = match source {
                Source::Hub(hub) => Box::pin(grouped_diff::read_hub(hub.context, ns, name)),
                Source::Api(endpoint) => Box::pin(api::sync_preview(endpoint, ns, name)),
            };
            let read = grouped_diff::cached(kept, fresh, read);
            async move { (i, read.await) }
        });
        let mut g = GroupedDiff {
            picked: apps.iter().map(key).zip(names).collect(),
            left_out,
            results,
            done,
            groups: Arc::default(),
            noise_groups: Arc::default(),
            regrouping: false,
            regroup_again: false,
            regrouped_at: None,
            noise: false,
            open: HashSet::new(),
            load: Default::default(),
            regroup: Default::default(),
        };
        g.load.spawn_each(cx, reads, |this: &mut Self, (_, (i, read)), cx| this.grouped_read(i, read, cx));
        self.grouped = Some(g);
        cx.notify();
    }

    /// The read of the Application at `i` ended: keep it, and regroup the changes.
    fn grouped_read(&mut self, i: usize, read: Read, cx: &mut Context<Self>) {
        let Some(g) = &mut self.grouped else { return };
        let Some(slot) = g.results.get_mut(i) else { return };
        if slot.is_none() {
            g.done += 1;
        }
        *slot = Some(match read {
            Ok(d) => Outcome::Diff(d),
            Err(e) => Outcome::Failed(e),
        });
        self.schedule_regroup(cx);
        cx.notify();
    }

    /// Regroup the changes off the UI thread, at most every `REGROUP_EVERY`: results that come
    /// while one waits or runs are taken by the next, started when it ends.
    fn schedule_regroup(&mut self, cx: &mut Context<Self>) {
        let Some(g) = &mut self.grouped else { return };
        if g.regrouping {
            g.regroup_again = true;
            return;
        }
        (g.regrouping, g.regroup_again) = (true, false);
        let wait = g.regrouped_at.map_or(Duration::ZERO, |t| REGROUP_EVERY.saturating_sub(t.elapsed()));
        let diffs: Vec<Option<Arc<AppDiff>>> = (0..g.results.len()).map(|i| g.diff(i).cloned()).collect();
        let exec = cx.background_executor().clone();
        let work = async move {
            exec.timer(wait).await;
            exec.spawn(async move {
                let refs: Vec<Option<&SmartDiff>> = diffs.iter().map(|d| d.as_ref().map(|d| &d.diff)).collect();
                (Arc::new(grouped_diff::regroup(&refs, false)), Arc::new(grouped_diff::regroup(&refs, true)))
            })
            .await
        };
        g.regroup.spawn(cx, work, |this: &mut Self, (groups, noise), cx| {
            let again = match &mut this.grouped {
                Some(g) => {
                    (g.groups, g.noise_groups) = (groups, noise);
                    (g.regrouping, g.regrouped_at) = (false, Some(Instant::now()));
                    g.regroup_again
                }
                None => false,
            };
            if again {
                this.schedule_regroup(cx);
            }
            cx.notify();
        });
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
    /// then those with nothing to sync, those Argo CD says differ with no change shown, those
    /// it couldn't compare and those that could not be read.
    pub(super) fn grouped_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = &self.grouped else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let n = g.picked.len();
        let name = |i: usize| g.picked.get(i).map_or_else(String::new, |(_, l)| l.clone());
        let ok: Vec<(usize, &Arc<AppDiff>)> = (0..n).filter_map(|i| Some((i, g.diff(i)?))).collect();
        let failed = |not_compared: bool| {
            let found = g.results.iter().enumerate().filter_map(move |(i, r)| match (r, not_compared) {
                (Some(Outcome::Failed(e)), false) | (Some(Outcome::NotCompared(e)), true) => Some((i, e.as_str())),
                _ => None,
            });
            grouped_diff::errors_by_text(found)
        };
        let (errors, not_compared) = (failed(false), failed(true));
        let in_sync: Vec<usize> = ok.iter().filter(|(_, d)| d.in_sync()).map(|(i, _)| *i).collect();
        let noise_only: Vec<usize> = ok.iter().filter(|(_, d)| d.noise_only()).map(|(i, _)| *i).collect();
        let unexplained: Vec<String> = ok
            .iter()
            .filter(|(_, d)| !d.unexplained.is_empty())
            .map(|(i, d)| {
                format!("{} ({})", name(*i), some(d.unexplained.iter().map(crate::smartdiff::ObjectKey::label), 3))
            })
            .collect();
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
                    .on_click(cx.listener(|this, _, _, cx| this.diff_selected(true, cx))),
            );
        let summary = grouped_diff::summary(&g.groups, with_changes);
        let nothing = !reading && summary.is_none() && errors.is_empty() && not_compared.is_empty();
        let noise_line = format!(
            "{} {}",
            crate::plural::count(noise_total, "noise change"),
            if g.noise { "shown" } else { "hidden" }
        );
        let more_groups = g.groups.len().saturating_sub(GROUPS_SHOWN);
        let more_noise = g.noise_groups.len().saturating_sub(GROUPS_SHOWN);
        let read_ok = ok.len();
        let more = |n: usize, what: &str| {
            div().text_xs().text_color(muted).child(format!("… {} more", crate::plural::count(n, what)))
        };
        let error_lines = |list: &[(String, Vec<usize>)]| -> Vec<Div> {
            let mut out: Vec<Div> = list
                .iter()
                .take(ERRORS_SHOWN)
                .map(|(e, apps)| {
                    let which = some(apps.iter().map(|&i| name(i)), APPS_NAMED);
                    div().child(selectable(format!(
                        "{e} ({}: {which})",
                        crate::plural::count(apps.len(), "Application")
                    )))
                })
                .collect();
            if list.len() > ERRORS_SHOWN {
                out.push(div().child(format!("… {} more", crate::plural::count(list.len() - ERRORS_SHOWN, "error"))));
            }
            out
        };
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
            .when(more_groups > 0, |v| v.child(more(more_groups, "change")))
            .when(g.noise && !g.noise_groups.is_empty(), |v| {
                v.child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child("NOISE"))
                    .children(
                        g.noise_groups
                            .iter()
                            .take(GROUPS_SHOWN)
                            .enumerate()
                            .map(|(i, c)| self.cross_row(g, "noise", i, c, read_ok, cx)),
                    )
                    .when(more_noise > 0, |v| v.child(more(more_noise, "noise rule")))
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
            .when(!unexplained.is_empty(), |v| {
                v.child(div().text_xs().text_color(theme.warning).child(selectable(format!(
                    "Argo CD says these differ where no change shows (its normalization, or noise only) ({}): {}",
                    unexplained.len(),
                    some(unexplained.iter().cloned(), LISTED)
                ))))
            })
            .when(!not_compared.is_empty(), |v| {
                v.child(
                    v_flex()
                        .gap_0p5()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("Argo CD could not compare these with git, so what a sync would do isn't known:")
                        .children(error_lines(&not_compared)),
                )
            })
            .when(!errors.is_empty(), |v| {
                v.child(
                    v_flex()
                        .gap_0p5()
                        .text_xs()
                        .text_color(theme.danger)
                        .child("Could not be read:")
                        .children(error_lines(&errors)),
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
    /// unfolded, each one's resources with their diff (worked out when it was read).
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
        let open = g.open.contains(&c.key);
        let name = |a: usize| g.picked.get(a).map_or_else(String::new, |(_, l)| l.clone());
        let where_ = if c.apps.len() == read_ok && read_ok > 1 {
            format!("in all {read_ok}")
        } else {
            format!("in {} of {read_ok}: {}", c.apps.len(), some(c.apps.iter().map(|(a, _)| name(*a)), APPS_NAMED))
        };
        let k = c.key.clone();
        let label = format!("{} {} ({})", if open { "▾" } else { "▸" }, c.text, c.apps.len());
        let toggle = cx.listener(move |this, _, _, cx| {
            let k = k.clone();
            this.update_grouped(cx, move |g| {
                if !g.open.remove(&k) {
                    g.open.insert(k);
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
                            let more = resources.len().saturating_sub(RESOURCES_SHOWN);
                            let mut shown: Vec<Div> = Vec::new();
                            if let Some(d) = g.diff(*a) {
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
    use std::collections::HashSet;

    use super::{flip, members, some, split_picks};

    #[test]
    fn a_long_list_of_names_says_how_many_more() {
        let names = |n: usize| (0..n).map(|i| format!("env{i}"));
        assert_eq!(some(names(2), 3), "env0, env1");
        assert_eq!(some(names(5), 3), "env0, env1, env2, 2 more");
    }

    fn k(name: &str) -> super::Key {
        ("argocd.example.com".into(), "argocd".into(), name.into())
    }

    /// A group is what the filter keeps of it: a service's Applications, or an app of apps
    /// with every one under it (however deep).
    #[test]
    fn a_group_is_what_the_filter_keeps_of_it() {
        // 0 made 1 and 2; 2 made 3; 4 alone.
        let parents = [None, Some(0), Some(0), Some(2), None];
        let services = vec![("web".to_string(), vec![1, 3]), ("api".to_string(), vec![0, 2, 4])];
        let all = [true; 5];
        assert_eq!(members(&services, &parents, &all, 0, false), [0, 1, 2, 3]);
        assert_eq!(members(&services, &parents, &all, 2, false), [2, 3]);
        assert_eq!(members(&services, &parents, &all, 4, false), [4]);
        assert_eq!(members(&services, &parents, &all, 1, true), [0, 2, 4]);
        let some_kept = [true, false, true, true, false];
        assert_eq!(members(&services, &parents, &some_kept, 0, false), [0, 2, 3]);
        assert_eq!(members(&services, &parents, &some_kept, 0, true), [3]);
        assert!(members(&services, &parents, &all, 9, true).is_empty());
    }

    #[test]
    fn picking_a_group_twice_unpicks_it_and_a_partial_group_is_picked_whole() {
        let mut picked: HashSet<super::Key> = HashSet::new();
        flip(&mut picked, vec![k("a"), k("b")]);
        assert_eq!(picked.len(), 2);
        flip(&mut picked, vec![k("a"), k("b")]);
        assert!(picked.is_empty());
        picked.insert(k("a"));
        flip(&mut picked, vec![k("a"), k("b")]);
        assert!(picked.contains(&k("a")) && picked.contains(&k("b")), "some picked: all are");
    }

    /// Picks the filter hides are counted, never acted on.
    #[test]
    fn picks_hidden_by_the_filter_are_counted_and_left_out() {
        let listed = vec![(2, k("a")), (0, k("b")), (1, k("c"))];
        let picked: HashSet<super::Key> = [k("a"), k("b"), k("gone")].into_iter().collect();
        let keeps = [false, true, true];
        assert_eq!(split_picks(&listed, &keeps, &picked), (vec![2], 1), "b hidden; gone not listed");
        assert_eq!(split_picks(&listed, &[true; 3], &picked), (vec![2, 0], 0), "in the list's order");
    }
}
