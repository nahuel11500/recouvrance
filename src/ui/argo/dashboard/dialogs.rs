//! The environment page's details, each in a dialog over the overview: opened the same way
//! everywhere (a bordered `Why? ↗` button), titled, centered, closed by its close button or
//! Escape, and the overview left as it was. Nothing unfolds elsewhere on the page.
//!
//! A dialog is drawn from the page's state each time it is drawn, so a read that lands while
//! it is open (a sync running is read every few seconds) shows in it at once.
//!
//! - **Why?**: one line per finding of the diagnosis, its status colored, a few words and at
//!   most one action, the long explanation on hover; what is out of sync by Argo CD state
//!   (`OutOfSync`, `Missing`, `Prune`).
//! - **Sync diff**: the prune verdict first, then one line per component (`components`),
//!   changed, added, removed, each with its resources and their YAML diff on demand.
//! - **Problems** and **Pods**: every problem, one line each; the pods in trouble.
//! - **History**: the deployments, two of them compared.
//!
//! The prune verdict's banner (`prune_banner`) is also the sync page's, next to its Prune box.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::ArgoView;
use super::{Dashboard, chip, with_tip};
use crate::k8s::argo::components::{self, Change, Line};
use crate::k8s::argo::dashboard::Cards;
use crate::k8s::argo::diagnosis::Finding;
use crate::k8s::argo::prune::Verdict;
use crate::status::Tone;
use crate::ui::selectable::selectable;

/// A dialog's width, at most (a narrow window shrinks it).
const WIDTH: f32 = 880.;
/// Names said in a line, at most.
const NAMES: usize = 4;

/// A detail of the environment page, shown in a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ui::argo) enum Detail {
    Why,
    SyncDiff,
    Problems,
    Pods,
    History,
}

impl Detail {
    fn title(self) -> &'static str {
        match self {
            Self::Why => "Why?",
            Self::SyncDiff => "Sync diff",
            Self::Problems => "Problems",
            Self::Pods => "Pods",
            Self::History => "History",
        }
    }
}

/// The button that opens a detail: bordered, `label ↗`, the same everywhere.
pub(super) fn detail_button(id: impl Into<ElementId>, label: &str) -> Button {
    Button::new(id).xsmall().outline().label(format!("{label} ↗"))
}

/// A finding's title as its line says it: the status (before the first `:`) and a few words
/// (after it); a title without one is all status.
pub(super) fn split_title(title: &str) -> (&str, &str) {
    match title.split_once(':') {
        Some((status, words)) if !status.trim().is_empty() => (status.trim(), words.trim()),
        _ => (title.trim(), ""),
    }
}

/// The out-of-sync finding of the diagnosis: its lines are said by Argo CD state instead.
fn is_out_of_sync(f: &Finding) -> bool {
    f.title.starts_with("Out of sync")
}

/// What hovering a finding's line says: Argo CD's message, the objects involved, what to do.
fn finding_tip(f: &Finding) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = &f.message {
        parts.push(m.clone());
    }
    let mut objects: Vec<String> = f.objects.iter().map(|(r, what)| format!("{}: {what}", r.label())).collect();
    if f.more > 0 {
        objects.push(format!("+{} more", f.more));
    }
    if !objects.is_empty() {
        parts.push(objects.join("\n"));
    }
    parts.push(f.hint.clone());
    parts.join("\n\n")
}

/// Some names, the first few, then how many more.
fn some_names(names: &[String], more: usize) -> String {
    let mut out: Vec<String> = names.iter().take(NAMES).cloned().collect();
    let more = more + names.len().saturating_sub(NAMES);
    if more > 0 {
        out.push(format!("+{more} more"));
    }
    out.join(", ")
}

/// The prune verdict, a filled colored banner with an icon: grey not needed, green
/// recommended, red data would be deleted. `compact`: its first line only (the rest on hover).
pub(in crate::ui::argo) fn prune_banner(
    id: impl Into<ElementId>,
    v: &Verdict,
    compact: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let (bg, fg, icon) = match v {
        Verdict::NotNeeded => (theme.muted, theme.foreground, Lucide::Info),
        Verdict::Recommended(_) => (theme.success, theme.success_foreground, Lucide::CircleCheck),
        Verdict::DataLost { .. } => (theme.danger, theme.danger_foreground, Lucide::TriangleAlert),
    };
    let lines = v.lines();
    let tip = format!("{}\n{}\n\n{}", v.title(), lines.join("\n"), v.advice());
    let shown: Vec<String> = if compact { lines.into_iter().take(1).collect() } else { lines };
    with_tip(id, tip)
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(bg)
        .text_color(fg)
        .child(div().flex_none().mt_0p5().child(Icon::new(icon).size_4().text_color(fg)))
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_weight(FontWeight::BOLD).child(v.title()))
                .children(
                    shown.into_iter().map(|l| div().text_xs().when(compact, Styled::truncate).child(selectable(l))),
                ),
        )
}

/// A component's icon and tone: an image or a setting changed, added, removed.
fn change_look(c: Change) -> (Lucide, Tone) {
    match c {
        Change::Image => (Lucide::ArrowUpDown, Tone::Busy),
        Change::Changed => (Lucide::SlidersHorizontal, Tone::Busy),
        Change::Added => (Lucide::Plus, Tone::Good),
        Change::Removed => (Lucide::Trash, Tone::Bad),
    }
}

impl ArgoView {
    /// Open `detail` in a dialog over the page. What was unfolded in a dialog before starts
    /// folded.
    pub(in crate::ui::argo) fn open_detail(&mut self, detail: Detail, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &mut self.dashboard else { return };
        d.diff_details.clear();
        d.sync_open.clear();
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let body =
                view.update(cx, |this, cx| this.detail_body(detail, cx)).unwrap_or_else(|_| div().into_any_element());
            let close = Button::new("argo-detail-close")
                .outline()
                .label("Close")
                .on_click(|_, window, cx| window.close_dialog(cx));
            dialog.title(detail.title()).w(px(WIDTH)).child(body).footer(h_flex().justify_end().child(close))
        });
    }

    /// Close the dialog shown, then open `detail` in its place.
    fn switch_detail(&mut self, detail: Detail, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        self.open_detail(detail, window, cx);
    }

    /// What the dialog of `detail` shows, from the page as it is now.
    fn detail_body(&self, detail: Detail, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let note = |text: &str| div().text_sm().text_color(muted).child(text.to_string()).into_any_element();
        let Some(d) = &self.dashboard else { return note("The environment page was closed.") };
        let c = match d.shown() {
            None => return note("Reading the Application…"),
            Some(Err(e)) => return note(&e),
            Some(Ok(c)) => c,
        };
        match detail {
            Detail::Why => self.why_body(d, &c, cx),
            Detail::SyncDiff => self.sync_diff_body(d, &c, cx),
            Detail::Problems => self.problems_body(d, &c, cx),
            Detail::Pods => self.pods_body(d, &c, cx),
            Detail::History => self.history_body(d, &c, cx),
        }
    }

    /// A line of a dialog: a box, said more of on hover.
    fn dialog_line(id: impl Into<ElementId>, tip: String, cx: &App) -> Stateful<Div> {
        let theme = cx.theme();
        with_tip(id, tip)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .min_w_0()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .text_sm()
    }

    // MARK: Why?

    /// One line per finding (its status colored, a few words, at most one action; the rest on
    /// hover); what is out of sync by Argo CD state under its finding.
    fn why_body(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        if c.findings.is_empty() {
            let text = if c.clean_is_known() {
                "Nothing wrong: Argo CD reports no error, the last sync went well and every resource is healthy."
            } else {
                "Argo CD reports no error and the last sync went well. Its health per resource wasn't read."
            };
            return div().text_sm().text_color(muted).child(text).into_any_element();
        }
        let k = d.current().cloned();
        let reaches = k.as_ref().and_then(|k| self.find(k)).is_some_and(|a| a.on.is_some());
        let mut out = v_flex().gap_1p5().child(
            div().text_xs().text_color(muted).child("Hover a line for Argo CD's message and what to do about it."),
        );
        for (i, f) in c.findings.iter().enumerate() {
            let (status, words) = split_title(&f.title);
            let action: Option<AnyElement> = if is_out_of_sync(f) {
                Some(
                    detail_button(("argo-why-diff", i), "Sync diff")
                        .on_click(cx.listener(|this, _, window, cx| this.switch_detail(Detail::SyncDiff, window, cx)))
                        .into_any_element(),
                )
            } else if let Some((res, _)) = f.objects.first().filter(|_| reaches) {
                let (k, res) = (k.clone(), res.clone());
                Some(
                    Button::new(("argo-why-open", i))
                        .xsmall()
                        .ghost()
                        .icon(Lucide::ExternalLink)
                        .label(format!("open {}", res.name))
                        .tooltip("Open it on the context that reaches its cluster")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            window.close_dialog(cx);
                            if let Some(k) = &k {
                                this.open_deployed(k, res.clone(), cx);
                            }
                        }))
                        .into_any_element(),
                )
            } else {
                None
            };
            out = out.child(
                Self::dialog_line(("argo-why", i), finding_tip(f), cx)
                    .child(chip(status, f.tone, cx))
                    .child(div().flex_1().min_w_0().truncate().child(words.to_string()))
                    .children(action),
            );
            if is_out_of_sync(f) {
                for (j, s) in components::argo_states(&c.diff).iter().enumerate() {
                    let names: Vec<String> = s.resources.iter().map(|r| r.name.clone()).collect();
                    let all: Vec<String> = s.resources.iter().map(crate::k8s::argo::diagnosis::Res::label).collect();
                    let n = s.resources.len() + s.more;
                    let tip = format!("{} ({}): {}\n\n{}", s.state, s.meaning, n, all.join("\n"));
                    out = out.child(
                        Self::dialog_line(("argo-why-state", j), tip, cx)
                            .ml_6()
                            .child(chip(s.state, Tone::Busy, cx))
                            .child(div().flex_none().text_xs().text_color(muted).child(s.meaning))
                            .child(div().flex_1().min_w_0().truncate().text_xs().child(some_names(&names, s.more))),
                    );
                }
            }
        }
        out.into_any_element()
    }

    // MARK: Sync diff

    /// The prune verdict, then one line per component, changed first, then added, then
    /// removed; each unfolds its resources (`details`). The noise left out in a sentence.
    fn sync_diff_body(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        if c.diff.total() == 0 && c.components.is_empty() {
            return div()
                .text_sm()
                .text_color(muted)
                .child("Nothing to sync: the cluster matches git.")
                .into_any_element();
        }
        let smart = c.smart.as_ref();
        // Where the difference comes from: git moved, or the cluster changed.
        let deployed = c.summary.history.first().map(|h| h.revision.clone()).filter(|r| !r.is_empty());
        let git = match (&deployed, &c.summary.revision) {
            (Some(dep), Some(now)) if dep == now => {
                Some(format!("Git unchanged since the last sync ({now}): what differs was changed in the cluster."))
            }
            (Some(dep), Some(now)) => Some(format!("Git moved since the last sync: {dep} deployed, {now} now.")),
            _ => None,
        };
        let source = match (smart, &c.git_diff) {
            (Some(_), _) => "From Argo CD's managed resources: the cluster against what a sync would leave.",
            (None, Some(Ok(_))) => "From Argo CD's diff.",
            _ => "From the Application's status: what differs in each resource isn't known.",
        };
        let rows: Vec<Div> = c.components.iter().enumerate().map(|(i, l)| self.component_row(i, l, d, c, cx)).collect();
        let noise = smart.and_then(|s| components::noise_sentence(&s.diff));
        let hooks = smart.filter(|s| s.hooks > 0).map(|s| {
            format!(
                "{} left out: they run during a sync rather than being kept in sync.",
                crate::plural::count(s.hooks, "hook")
            )
        });
        let unexplained = smart.filter(|s| !s.unexplained.is_empty()).map(|s| {
            let names: Vec<String> = s.unexplained.iter().map(|k| k.name.clone()).collect();
            format!(
                "Argo CD says these differ where no change shows (its normalization, or noise only): {}",
                some_names(&names, 0)
            )
        });
        let more = (c.diff.more > 0).then(|| {
            format!("{} more differ past those Argo CD listed.", crate::plural::count(c.diff.more, "resource"))
        });
        v_flex()
            .gap_2()
            .child(prune_banner("argo-diff-prune", &c.prune, false, cx))
            .children(git.map(|g| div().text_xs().text_color(muted).child(selectable(g))))
            .child(v_flex().gap_1p5().children(rows))
            .children(
                [noise, hooks, unexplained, more, Some(source.to_string())]
                    .into_iter()
                    .flatten()
                    .map(|t| div().text_xs().text_color(muted).child(selectable(t))),
            )
            .into_any_element()
    }

    /// A component's line: its icon and color by kind of change, its name, what changes, a
    /// line under it, `requires pruning` when removed, and `details` to unfold its resources.
    fn component_row(&self, i: usize, l: &Line, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let (icon, tone) = change_look(l.change);
        let color = tone.color(cx);
        let open = d.diff_details.contains(&l.name);
        let name = l.name.clone();
        let toggle = Button::new(("argo-diff-details", i))
            .xsmall()
            .outline()
            .label(if open { "hide" } else { "details" })
            .tooltip("What changes, resource by resource")
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(d) = &mut this.dashboard
                    && !d.diff_details.remove(&name)
                {
                    d.diff_details.insert(name.clone());
                }
                cx.notify();
            }));
        let what = l.what.clone().map(|w| {
            div()
                .flex_none()
                .px_1p5()
                .rounded_md()
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .child(selectable(w))
        });
        let pruning = (l.change == Change::Removed).then(|| chip("requires pruning", Tone::Bad, cx));
        let head = h_flex()
            .gap_3()
            .items_center()
            .px_3()
            .py_2()
            .rounded_md()
            .border_l_2()
            .border_color(color)
            .bg(color.opacity(0.08))
            .child(
                div()
                    .flex_none()
                    .size(px(28.))
                    .rounded_full()
                    .bg(theme.background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).size_4().text_color(color)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_wrap()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(selectable(l.name.clone())))
                            .children(what)
                            .children(pruning),
                    )
                    .child(div().text_xs().text_color(muted).child(selectable(l.detail.clone()))),
            )
            .child(toggle);
        v_flex().gap_1().child(head).when(open, |v| v.child(self.component_details(i, l, d, c, cx)))
    }

    /// A component's resources: what changes in each, its YAML diff on demand (two columns),
    /// and its noise counted.
    fn component_details(&self, i: usize, l: &Line, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let smart = c.smart.as_ref().map(|s| &s.diff);
        // Argo CD's own diff, when the managed resources weren't read with the smart diff.
        let git = match &c.git_diff {
            Some(Ok(g)) => g.changed.as_slice(),
            _ => &[],
        };
        let mut out = v_flex().gap_1().ml_6().pl_3();
        for (j, key) in l.resources.iter().enumerate() {
            let r = smart.and_then(|s| s.resources.iter().find(|r| r.key == *key));
            let from_git = git.iter().find(|g| {
                (&g.res.group, &g.res.kind, &g.res.namespace, &g.res.name)
                    == (&key.group, &key.kind, &key.namespace, &key.name)
            });
            let what = match (r, from_git) {
                (Some(r), _) => {
                    let mut facts: Vec<String> = r.facts.iter().map(crate::smartdiff::Fact::text).collect();
                    if facts.is_empty() {
                        facts.push("noise only".into());
                    }
                    if !r.noise.is_empty() {
                        facts.push(format!("{} left out", crate::plural::count(r.noise.len(), "noise change")));
                    }
                    facts.join("; ")
                }
                (None, Some(g)) => g.change.text().to_string(),
                (None, None) => match l.change {
                    Change::Added => "new: in git, not in the cluster".into(),
                    Change::Removed => "in the cluster, no longer in git".into(),
                    _ => "differs from git (its live object wasn't compared)".into(),
                },
            };
            let lines: Option<&[crate::history::DiffLine]> =
                r.map(|r| r.lines(false)).or(from_git.map(|g| g.lines.as_slice()));
            let open = d.sync_open.contains(key);
            let k = key.clone();
            let yaml = lines.map(|_| {
                Button::new(SharedString::from(format!("argo-diff-yaml-{i}-{j}")))
                    .xsmall()
                    .ghost()
                    .label(if open { "hide YAML" } else { "YAML" })
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_smart(super::DiffAt::Sync, Some(k.clone()), cx)),
                    )
            });
            out = out
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .min_w_0()
                        .text_xs()
                        .child(div().flex_none().font_weight(FontWeight::MEDIUM).child(selectable(format!(
                            "{} {}",
                            components::kind_word(&key.kind),
                            key.name
                        ))))
                        .child(div().flex_1().min_w_0().text_color(muted).child(selectable(what)))
                        .children(yaml),
                )
                .when_some(lines.filter(|_| open), |v, lines| {
                    v.child(super::lines_block(lines, ("In the cluster", "After a sync"), cx))
                });
        }
        out
    }

    // MARK: Problems, pods, history

    /// Every problem, one line each (its why on hover), with the assistant asked about them.
    fn problems_body(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let p = &c.problems;
        let muted = cx.theme().muted_foreground;
        if p.rows.is_empty() {
            return div().text_sm().text_color(muted).child("Nothing wrong.").into_any_element();
        }
        let k = d.current().cloned();
        let reaches = k.as_ref().and_then(|k| self.find(k)).is_some_and(|a| a.on.is_some());
        let k_all = k;
        let ask = self.ask_button("argo-problems-ask-all", d).label("Ask the assistant").on_click(cx.listener(
            move |this, _, window, cx| {
                window.close_dialog(cx);
                this.ask_problems(k_all.as_ref(), None, cx);
            },
        ));
        let mut counts = vec![crate::plural::count(p.rows.len(), "problem")];
        if p.from_tree {
            counts.push(format!("{} · {} in trouble", crate::plural::count(p.pods.len(), "pod"), p.troubled));
        }
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().text_xs().text_color(muted).child(counts.join(" · ")))
                    .child(ask),
            )
            .children(p.rows.iter().enumerate().map(|(i, r)| self.problem_row(d, i, r, reaches, cx)))
            .into_any_element()
    }

    /// The pods in trouble first, each with its status; the namespace opened on its context.
    fn pods_body(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let p = &c.problems;
        let k = d.current().cloned();
        let reaches =
            k.as_ref().and_then(|k| self.find(k)).is_some_and(|a| a.on.is_some() && !a.dest.namespace.is_empty());
        let mut pods: Vec<&crate::k8s::argo::dashboard::Pod> = p.pods.iter().collect();
        pods.sort_by_key(|pod| match pod.tone {
            Tone::Bad => 0,
            Tone::Busy => 1,
            _ => 2,
        });
        let open = reaches.then(|| {
            Button::new("argo-pods-namespace")
                .xsmall()
                .outline()
                .icon(Lucide::FolderOpen)
                .label("Open the namespace")
                .tooltip("Open the destination namespace on the context that reaches its cluster")
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.close_dialog(cx);
                    if let Some(k) = &k {
                        this.open_destination(k, cx);
                    }
                }))
        });
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().text_xs().text_color(muted).child(format!(
                        "{} · {} in trouble",
                        crate::plural::count(p.pods.len(), "pod"),
                        p.troubled
                    )))
                    .children(open),
            )
            .child(h_flex().flex_wrap().gap_x_4().gap_y_1().children(pods.into_iter().map(|pod| {
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .child(chip(&pod.status, pod.tone, cx))
                    .child(selectable(pod.res.name.clone()))
            })))
            .into_any_element()
    }

    /// The deployments, newest first, each compared with the one before or picked to compare
    /// with another; the comparison under them.
    fn history_body(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_2()
            .children(self.history_rows(d, c, cx))
            .when_some(d.compare.as_ref(), |v, cmp| v.child(self.compare_view(cmp, cx)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{some_names, split_title};

    #[test]
    fn a_title_is_split_into_status_and_words() {
        assert_eq!(split_title("Out of sync: 3 resources"), ("Out of sync", "3 resources"));
        assert_eq!(split_title("Last sync failed: forbidden"), ("Last sync failed", "forbidden"));
        assert_eq!(split_title("Being deleted for 3 h"), ("Being deleted for 3 h", ""));
        assert_eq!(split_title(": odd"), (": odd", ""));
    }

    #[test]
    fn names_are_cut_short() {
        let names: Vec<String> = ["a", "b", "c", "d", "e"].map(String::from).to_vec();
        assert_eq!(some_names(&names, 0), "a, b, c, d, +1 more");
        assert_eq!(some_names(&names[..2], 3), "a, b, +3 more");
    }
}
