//! The Application dashboard of the Argo CD page: a service's Applications, one tab per
//! environment, and the cards of the one shown (see `k8s::argo::dashboard`).
//!
//! Only the tab shown is read: the Application whole, its resource tree, its diff and its
//! commits, by Argo CD's API (or on its hub). It is read again when its row changes (the
//! stream, the hubs' next read), every few seconds while a sync runs, and on Refresh; a
//! newer read replaces the one running, and leaving the tab or the page drops it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;

use super::{ArgoView, Key, key};
use crate::details::ArgoAction;
use crate::k8s;
use crate::k8s::argo::dashboard::{self, Cards, Commit, Problem};
use crate::k8s::argo::diagnosis::span_text;
use crate::k8s::argo::{self, Source, api, short_revision};
use crate::status::Tone;
use crate::ui::selectable::selectable;

/// A card's width: three or four side by side on a wide window.
const CARD: f32 = 380.;
/// How often the tab shown is read while a sync runs.
const RUNNING_EVERY: Duration = Duration::from_secs(3);
/// Problems listed before "+N more".
const PROBLEMS_SHOWN: usize = 5;
/// Names listed per kind of change in the Sync diff card.
const NAMES_SHOWN: usize = 4;

/// The dashboard open on the page.
pub(super) struct Dashboard {
    service: String,
    /// The service's Applications, one tab each.
    members: Vec<Key>,
    tab: usize,
    /// The cards last read, and of which Application.
    cards: Option<(Key, Result<Arc<Cards>, String>)>,
    /// The Application's row when it was read: a change of it reads it again.
    read_for: Option<(Key, argo::Summary)>,
    loading: bool,
    updated: Option<Timestamp>,
    load: crate::latest::Latest,
    /// Reads it again while a sync runs.
    tick: crate::latest::Latest,
    /// The parts unfolded: "why", "steps", "diff", "history", "pods", "problems", "why-<i>".
    open: HashSet<String>,
    /// Made-up environments and their cards (the developer tools' example): nothing is read.
    example: Option<Vec<(String, Arc<Cards>)>>,
}

impl Dashboard {
    fn new(service: String, members: Vec<Key>, tab: usize) -> Self {
        Self {
            service,
            members,
            tab,
            cards: None,
            read_for: None,
            loading: false,
            updated: None,
            load: Default::default(),
            tick: Default::default(),
            open: HashSet::new(),
            example: None,
        }
    }

    fn current(&self) -> Option<&Key> {
        self.members.get(self.tab)
    }

    /// The cards of the tab shown; None while its first read runs.
    fn shown(&self) -> Option<Result<Arc<Cards>, String>> {
        if let Some(tabs) = &self.example {
            return tabs.get(self.tab).map(|(_, c)| Ok(c.clone()));
        }
        let (k, cards) = self.cards.as_ref()?;
        (Some(k) == self.current()).then(|| cards.clone())
    }

    fn is_open(&self, part: &str) -> bool {
        self.open.contains(part)
    }
}

impl ArgoView {
    /// Open the dashboard of the service of the Application `k`, on its tab.
    pub(super) fn open_dashboard(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(rows) = &self.rows else { return };
        let apps = &rows.fleet.apps;
        let Some(ix) = apps.iter().position(|a| key(a) == *k) else { return };
        let (service, members) = rows
            .services
            .iter()
            .find(|(_, m)| m.contains(&ix))
            .map_or_else(|| (apps[ix].app.name.clone(), vec![ix]), |(s, m)| (s.clone(), m.clone()));
        let tab = members.iter().position(|&m| m == ix).unwrap_or(0);
        let members = members.iter().map(|&m| key(&apps[m])).collect();
        self.dashboard = Some(Dashboard::new(service, members, tab));
        self.load_dashboard(cx);
    }

    /// Open the dashboard of the service at `g` of the services, on its first Application.
    pub(super) fn open_service(&mut self, g: usize, cx: &mut Context<Self>) {
        let first = self.rows.as_ref().and_then(|r| r.services.get(g)?.1.first().map(|&m| key(&r.fleet.apps[m])));
        if let Some(k) = first {
            self.open_dashboard(&k, cx);
        }
    }

    pub(super) fn close_dashboard(&mut self, cx: &mut Context<Self>) {
        // Its reads are dropped with it.
        self.dashboard = None;
        cx.notify();
    }

    fn select_dashboard_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        let Some(d) = &mut self.dashboard else { return };
        if d.tab == tab {
            return;
        }
        d.tab = tab;
        d.open.clear();
        d.tick.cancel();
        self.load_dashboard(cx);
        cx.notify();
    }

    fn toggle_part(&mut self, part: String, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.dashboard
            && !d.open.remove(&part)
        {
            d.open.insert(part);
        }
        cx.notify();
    }

    /// Read the tab shown, off the UI thread: by Argo CD's API, or on its hub. Its cards are
    /// worked out there too. A result for another tab than the one shown by then is left.
    pub(super) fn load_dashboard(&mut self, cx: &mut Context<Self>) {
        let Some(k) = self.dashboard.as_ref().filter(|d| d.example.is_none()).and_then(|d| d.current().cloned()) else {
            return;
        };
        let Some(a) = self.find(&k).cloned() else {
            if let Some(d) = &mut self.dashboard {
                d.load.cancel();
                d.cards = Some((k, Err("This Application is no longer listed.".into())));
            }
            cx.notify();
            return;
        };
        let (ns, name) = (a.app.namespace.clone(), a.app.name.clone());
        let read: std::pin::Pin<Box<dyn Future<Output = Result<dashboard::Read, String>>>> = match &a.source {
            Source::Hub(hub) => Box::pin(dashboard::read_hub(hub.clone(), ns, name)),
            Source::Api(endpoint) => Box::pin(api::dashboard(endpoint.clone(), ns, name)),
        };
        let exec = cx.background_executor().clone();
        let seen_key = k.clone();
        let work = async move {
            let read = read.await?;
            let cards = exec.spawn(async move {
                let seen = dashboard::seen_images(&seen_key, &read.app);
                Cards::of(read, seen.as_deref(), Timestamp::now())
            });
            Ok::<_, String>(Arc::new(cards.await))
        };
        let Some(d) = &mut self.dashboard else { return };
        d.loading = true;
        d.read_for = Some((k.clone(), a.app.clone()));
        d.load.spawn(cx, work, move |this, cards: Result<Arc<Cards>, String>, cx| {
            let Some(d) = &mut this.dashboard else { return };
            if d.current() != Some(&k) {
                return;
            }
            let running =
                cards.as_ref().is_ok_and(|c| c.summary.in_progress || c.last_sync.as_ref().is_some_and(|s| s.running));
            d.loading = false;
            d.updated = Some(Timestamp::now());
            d.cards = Some((k, cards));
            if running {
                // A sync running: its steps move without its row changing.
                let timer = cx.background_executor().timer(RUNNING_EVERY);
                d.tick.spawn(cx, timer, |this, _, cx| {
                    if crate::app::is_visible(cx.entity_id(), cx) {
                        this.load_dashboard(cx);
                    }
                });
            }
            cx.notify();
        });
        cx.notify();
    }

    /// The page's rows were read again: read the tab shown again when its row changed.
    pub(super) fn dashboard_changed(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.dashboard.as_ref().filter(|d| d.example.is_none()) else { return };
        let Some(k) = d.current().cloned() else { return };
        let now = self.find(&k).map(|a| a.app.clone());
        let before = d.read_for.as_ref().filter(|(rk, _)| *rk == k).map(|(_, s)| s.clone());
        if now.is_some() && now != before {
            self.load_dashboard(cx);
        }
    }

    /// The made-up example of the developer tools: three environments, nothing read.
    #[cfg(feature = "dev")]
    pub fn demo_dashboard(&mut self, cx: &mut Context<Self>) {
        use dashboard::example;
        let now = Timestamp::now();
        let tabs = example::ENVIRONMENTS
            .iter()
            .map(|env| (env.to_string(), Arc::new(Cards::of(example::read(env, now), None, now))))
            .collect();
        let mut d = Dashboard::new("shop (example, no real data)".into(), Vec::new(), 1);
        d.example = Some(tabs);
        d.updated = Some(now);
        self.dashboard = Some(d);
        cx.notify();
    }

    // MARK: Rendering

    /// The dashboard: its header, a tab per environment, and the cards of the one shown.
    pub(super) fn dashboard_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.dashboard else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let updated = d.updated.map(|t| {
            let at = t.to_zoned_datetime(jiff::tz::TimeZone::system()).strftime("%H:%M:%S").to_string();
            format!("Updated at {at}")
        });
        let current = d.current().and_then(|k| self.find(k));
        let by_api = current.is_some_and(|a| matches!(a.source, Source::Api(_)));
        let header = h_flex()
            .gap_2()
            .child(
                Button::new("argo-dash-back")
                    .small()
                    .ghost()
                    .icon(Lucide::ArrowLeft)
                    .label("Applications")
                    .on_click(cx.listener(|this, _, _, cx| this.close_dashboard(cx))),
            )
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(format!("Argo CD · {}", d.service)))
            .child(div().flex_1())
            .children(updated.map(|u| div().text_xs().text_color(muted).child(u)))
            .child(
                Button::new("argo-dash-refresh")
                    .small()
                    .outline()
                    .icon(Lucide::RefreshCw)
                    .label("Refresh")
                    .loading(d.loading)
                    .disabled(d.example.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.load_dashboard(cx))),
            )
            .when_some(d.current().cloned().filter(|_| current.is_some()), |h, k| {
                h.child(
                    Button::new("argo-dash-open")
                        .small()
                        .ghost()
                        .icon(if by_api { Lucide::ExternalLink } else { Lucide::PanelRightOpen })
                        .label(if by_api { "Open in Argo CD" } else { "Open" })
                        .on_click(cx.listener(move |this, _, _, cx| this.open(&k, cx))),
                )
            });
        let tabs: Vec<(String, Tone)> = match &d.example {
            Some(tabs) => tabs.iter().map(|(n, c)| (n.clone(), c.state.tone)).collect(),
            None => d
                .members
                .iter()
                .map(|k| {
                    let tone = self.find(k).map_or(Tone::Done, |a| argo::attention(&a.app));
                    (k.2.clone(), tone)
                })
                .collect(),
        };
        let view = cx.entity().downgrade();
        let tab_bar = TabBar::new(SharedString::from(format!("argo-dash-tabs-{}-{}", tabs.len(), d.tab)))
            .underline()
            .selected_index(d.tab)
            .children(tabs.iter().map(|(name, tone)| {
                Tab::new().prefix(div().mr_1p5().size(px(8.)).rounded_full().bg(tone.color(cx))).label(name.clone())
            }))
            .on_click(move |ix: &usize, _, cx| {
                let ix = *ix;
                let _ = view.update(cx, |this, cx| this.select_dashboard_tab(ix, cx));
            });
        let body = match d.shown() {
            None => div().p_4().text_color(muted).child("Reading the Application…").into_any_element(),
            Some(Err(e)) => div().p_4().text_color(theme.danger).child(selectable(e)).into_any_element(),
            Some(Ok(cards)) => self.cards(d, &cards, cx),
        };
        v_flex()
            .size_full()
            .gap_2()
            .child(header)
            .child(tab_bar)
            .child(div().id("argo-dash-body").flex_1().min_h_0().overflow_y_scroll().child(body))
            .into_any_element()
    }

    fn cards(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_2()
            .pb_4()
            .when_some(c.note.clone(), |v, note| v.child(div().text_xs().text_color(muted).child(selectable(note))))
            .child(
                h_flex()
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .child(self.state_card(d, c, cx))
                    .child(self.versions_card(d, c, cx))
                    .child(in_cluster_card(c, cx))
                    .child(self.problems_card(d, c, cx))
                    .child(self.last_sync_card(d, c, cx))
                    .child(self.diff_card(d, c, cx))
                    .child(self.history_card(d, c, cx)),
            )
            .into_any_element()
    }

    /// A link that unfolds a part of a card, or folds it.
    fn toggle_link(&self, id: &str, part: &str, label: &str, d: &Dashboard, cx: &mut Context<Self>) -> Stateful<Div> {
        let open = d.is_open(part);
        let part = part.to_string();
        link(SharedString::from(format!("argo-dash-{id}")), if open { "hide" } else { label }, cx)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_part(part.clone(), cx)))
    }

    fn state_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let s = &c.state;
        let sync_tone = match s.sync.as_str() {
            "Sync Failed" => Tone::Bad,
            "Syncing" => Tone::Busy,
            other => argo::sync_tone(other, c.summary.automated),
        };
        let degraded = s.degraded.first().map(|first| match s.degraded.len() {
            1 => format!("{first} Degraded"),
            n => format!("{first} Degraded +{}", n - 1),
        });
        let why = self.toggle_link("why", "why", "Why?", d, cx);
        let theme = cx.theme().clone();
        card("State", Some(s.tone), cx)
            .child(div().text_2xl().font_weight(FontWeight::BOLD).text_color(sync_tone.color(cx)).child(s.sync.clone()))
            .child(
                div().text_sm().text_color(argo::health_tone(&s.health).color(cx)).child(selectable(s.health.clone())),
            )
            .when_some(s.health_message.clone(), |v, m| {
                v.child(div().text_xs().text_color(theme.muted_foreground).child(selectable(m)))
            })
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .children(degraded.map(|t| div().text_color(theme.danger).child(selectable(t))))
                    .when(!c.findings.is_empty(), |h| h.child(why)),
            )
            .when(d.is_open("why"), |v| {
                v.children(c.findings.iter().map(|f| {
                    v_flex()
                        .gap_0p5()
                        .pt_1()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(div().size(px(7.)).flex_none().rounded_full().bg(f.tone.color(cx)))
                                .child(
                                    div().text_sm().font_weight(FontWeight::MEDIUM).child(selectable(f.title.clone())),
                                ),
                        )
                        .when_some(f.message.clone(), |v, m| v.child(div().text_xs().child(selectable(m))))
                        .children(f.objects.iter().take(4).map(|(r, what)| {
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(selectable(format!("{}: {what}", r.label())))
                        }))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(selectable(f.hint.clone())))
                }))
            })
    }

    fn versions_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let v = &c.versions;
        let theme = cx.theme().clone();
        let chips = h_flex().flex_wrap().gap_1p5().children(v.images.iter().map(|i| {
            let changed = i.from.is_some();
            let text = match &i.from {
                Some(from) => format!("{from} → {}", i.tag),
                None => i.tag.clone(),
            };
            h_flex()
                .gap_1()
                .px_2()
                .py_0p5()
                .rounded_md()
                .border_1()
                .text_sm()
                .border_color(if changed { theme.danger } else { theme.border })
                .when(changed, |h| h.bg(theme.danger.opacity(0.1)))
                .child(div().text_color(theme.muted_foreground).child(selectable(i.component.clone())))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(if changed { theme.danger } else { theme.foreground })
                        .child(selectable(text)),
                )
        }));
        let mut released = Vec::new();
        if let Some(t) = v.released {
            released.push(format!("Released {} ago", k8s::age(t)));
        }
        if let Some(by) = &v.by {
            released.push(format!("by {by}"));
        }
        let released = released.join(" ");
        let line = match &v.previous {
            Some(p) if released.is_empty() => format!("previous: {p}"),
            Some(p) => format!("{released} · previous: {p}"),
            None => released,
        };
        let rollback = match (v.auto_sync, v.rollback_to) {
            (true, _) => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Auto-sync is on: Argo CD allows no rollback.")
                .into_any_element(),
            (false, None) => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("No deployment before this one to roll back to.")
                .into_any_element(),
            (false, Some(id)) => {
                let k = d.current().cloned();
                Button::new("argo-dash-rollback")
                    .small()
                    .outline()
                    .icon(Lucide::Undo2)
                    .label(format!("Roll back to deployment {id}…"))
                    .disabled(k.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(k) = &k {
                            this.act(k, ArgoAction::Rollback(Some(id)), window, cx);
                        }
                    }))
                    .into_any_element()
            }
        };
        card("Versions", None, cx)
            .child(if v.images.is_empty() {
                div().text_sm().text_color(theme.muted_foreground).child("Argo CD lists no image.").into_any_element()
            } else {
                chips.into_any_element()
            })
            .when(!line.is_empty(), |c| {
                c.child(div().text_xs().text_color(theme.muted_foreground).child(selectable(line)))
            })
            .child(div().child(rollback))
    }

    fn problems_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let p = &c.problems;
        let theme = cx.theme().clone();
        let tone = if p.rows.iter().any(|r| r.tone == Tone::Bad) {
            Tone::Bad
        } else if p.rows.is_empty() {
            Tone::Good
        } else {
            Tone::Busy
        };
        let all = d.is_open("problems");
        let shown = if all { p.rows.len() } else { p.rows.len().min(PROBLEMS_SHOWN) };
        let rows: Vec<Div> =
            p.rows.iter().take(shown).enumerate().map(|(i, r)| self.problem_row(d, i, r, cx)).collect();
        let more = p.rows.len() - shown;
        let k = d.current().cloned();
        let reaches =
            k.as_ref().and_then(|k| self.find(k)).is_some_and(|a| a.on.is_some() && !a.dest.namespace.is_empty());
        let all_pods = if reaches {
            let k = k.clone();
            link("argo-dash-all-pods", "all pods", cx).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(k) = &k {
                    this.open_destination(k, cx);
                }
            }))
        } else {
            self.toggle_link("pods", "pods", "all pods", d, cx)
        };
        card("Problems", Some(tone), cx)
            .when(p.rows.is_empty(), |v| v.child(div().text_sm().text_color(theme.success).child("Nothing wrong.")))
            .children(rows)
            .when(more > 0, |v| v.child(self.toggle_link("more", "problems", &format!("+{more} more"), d, cx)))
            .when(p.from_tree, |v| {
                v.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} · {} in trouble", crate::plural::count(p.pods.len(), "pod"), p.troubled))
                        .when(!p.pods.is_empty(), |h| h.child(all_pods)),
                )
            })
            .when(!p.from_tree, |v| {
                let why = c.tree_error.clone().unwrap_or_else(|| {
                    "From the Application's status: its pods are in Argo CD's resource tree.".into()
                });
                v.child(div().text_xs().text_color(theme.muted_foreground).child(selectable(why)))
            })
            .when(d.is_open("pods") && !reaches, |v| {
                v.children(p.pods.iter().map(|pod| {
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .child(chip(&pod.status, pod.tone, cx))
                        .child(selectable(pod.res.name.clone()))
                }))
            })
    }

    fn problem_row(&self, d: &Dashboard, i: usize, r: &Problem, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let part = format!("why-{i}");
        let open = d.is_open(&part);
        let why = r.why.is_some().then(|| self.toggle_link(&part, &part, "why", d, cx));
        let mut facts = vec![r.res.kind.clone()];
        if let Some((bad, all)) = r.pods {
            facts.push(format!("{bad} of {}", crate::plural::count(all, "pod")));
        }
        if let Some(since) = r.since {
            facts.push(format!("for {}", span_text(Timestamp::now().as_second() - since.as_second())));
        }
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(chip(&r.status, r.tone, cx))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(selectable(r.res.name.clone())),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(selectable(facts.join(" · "))))
                    .children(why),
            )
            .when(open, |v| {
                v.child(
                    div()
                        .pl_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(selectable(r.why.clone().unwrap_or_default())),
                )
            })
    }

    fn last_sync_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(s) = &c.last_sync else {
            return card("Last sync", None, cx).child(div().text_sm().text_color(muted).child("Never synced."));
        };
        let now = Timestamp::now();
        let mut facts = Vec::new();
        if let Some(secs) = s.seconds(now) {
            facts.push(if s.running {
                format!("running for {}", span_text(secs))
            } else {
                format!("took {}", span_text(secs))
            });
        }
        if let Some(by) = &s.by {
            facts.push(format!("by {by}"));
        }
        if let Some(t) = s.finished.or(s.started) {
            facts.push(format!("{} ago", k8s::age(t)));
        }
        if let Some(r) = &s.revision {
            facts.push(format!("revision {r}"));
        }
        let progress = (s.running && !s.steps.is_empty()).then(|| {
            let share = s.done as f32 / s.steps.len() as f32;
            v_flex().gap_0p5().child(div().text_xs().child(format!("{} of {} steps", s.done, s.steps.len()))).child(
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(theme.muted)
                    .child(div().h_full().w(relative(share)).rounded_full().bg(theme.warning)),
            )
        });
        let steps = (!s.steps.is_empty()).then(|| self.toggle_link("steps", "steps", "steps", d, cx));
        card("Last sync", Some(s.tone), cx)
            .child(div().text_xl().font_weight(FontWeight::BOLD).text_color(s.tone.color(cx)).child(s.phase.clone()))
            .when_some(s.message.clone(), |v, m| v.child(div().text_xs().child(selectable(m))))
            .children(progress)
            .child(h_flex().gap_2().text_xs().text_color(muted).child(selectable(facts.join(" · "))).children(steps))
            .when(d.is_open("steps"), |v| {
                v.children(s.steps.iter().map(|st| {
                    v_flex()
                        .child(
                            h_flex()
                                .gap_2()
                                .text_xs()
                                .child(chip(
                                    if st.status.is_empty() { "Waiting" } else { st.status.as_str() },
                                    st.tone,
                                    cx,
                                ))
                                .child(selectable(st.res.label()))
                                .when(st.hook, |h| h.child(div().text_color(muted).child("hook"))),
                        )
                        .when_some(st.message.clone(), |v, m| {
                            v.child(div().pl_2().text_xs().text_color(muted).child(selectable(m)))
                        })
                }))
            })
    }

    fn diff_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let diff = &c.diff;
        let total = diff.total();
        if total == 0 {
            let error = match &c.git_diff {
                Some(Err(e)) => Some(format!("Argo CD's diff couldn't be read ({e}): from the Application's status.")),
                _ => None,
            };
            return card("Sync diff", Some(Tone::Good), cx)
                .child(div().text_sm().child("Nothing to sync: the cluster matches git."))
                .children(error.map(|e| div().text_xs().text_color(muted).child(selectable(e))));
        }
        let names = |sign: &str, list: &[k8s::argo::diagnosis::Res]| {
            (!list.is_empty()).then(|| {
                let mut shown: Vec<String> = list.iter().take(NAMES_SHOWN).map(|r| r.name.clone()).collect();
                if list.len() > NAMES_SHOWN {
                    shown.push(format!("+{}", list.len() - NAMES_SHOWN));
                }
                format!("{sign} {}", shown.join(", "))
            })
        };
        let changed: Vec<k8s::argo::diagnosis::Res> = diff.changed.iter().chain(&diff.unknown).cloned().collect();
        let lines: Vec<String> = [names("~", &changed[..]), names("+", &diff.added[..]), names("–", &diff.pruned[..])]
            .into_iter()
            .flatten()
            .collect();
        let tone = if c.summary.automated { Tone::Busy } else { Tone::Done };
        let source = if diff.from_argo {
            "From Argo CD's diff.".to_string()
        } else {
            match &c.git_diff {
                Some(Err(e)) => format!("From the Application's status: Argo CD's diff couldn't be read ({e})."),
                _ => "From the Application's status.".to_string(),
            }
        };
        let show = self.toggle_link("diff", "diff", "show", d, cx);
        card("Sync diff", Some(tone), cx)
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .child(format!("{total} {}", crate::plural::pick(total, "resource differs", "resources differ"))),
            )
            .child(div().text_sm().child(diff.text()))
            .children(
                lines
                    .into_iter()
                    .map(|l| div().text_xs().font_family(theme.mono_font_family.clone()).child(selectable(l))),
            )
            .child(h_flex().gap_2().text_xs().text_color(muted).child(source).child(show))
            .when(d.is_open("diff"), |v| match &c.git_diff {
                Some(Ok(g)) => v.children(g.changed.iter().map(|r| {
                    v_flex()
                        .gap_0p5()
                        .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(selectable(format!(
                            "{} ({})",
                            r.res.label(),
                            r.change.text()
                        ))))
                        .child(crate::details::diff_block(&r.lines, cx))
                })),
                _ => v.children(
                    changed
                        .iter()
                        .map(|r| (r, "differs"))
                        .chain(diff.added.iter().map(|r| (r, "to create")))
                        .chain(diff.pruned.iter().map(|r| (r, "to prune")))
                        .map(|(r, what)| div().text_xs().child(selectable(format!("{}: {what}", r.label())))),
                ),
            })
    }

    fn history_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let h = &c.history;
        let tone = if h.dots.last().is_some_and(|dot| !dot.ok) { Some(Tone::Bad) } else { None };
        let open = self.toggle_link("history", "history", "open", d, cx);
        card("History", tone, cx)
            .when(h.dots.is_empty(), |v| v.child(div().text_sm().text_color(muted).child("No deployment yet.")))
            .child(h_flex().gap_1().children(
                h.dots.iter().map(|dot| {
                    div().size(px(12.)).rounded_full().bg(if dot.ok { theme.success } else { theme.danger })
                }),
            ))
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{} succeeded, {} failed", h.succeeded, h.failed))
                    .when(!h.dots.is_empty(), |row| row.child(open)),
            )
            .when(d.is_open("history"), |v| {
                v.children(h.dots.iter().rev().map(|dot| {
                    let mut parts = vec![match dot.id {
                        Some(id) => format!("#{id}"),
                        None => "failed sync".to_string(),
                    }];
                    if !dot.revision.is_empty() {
                        parts.push(dot.revision.clone());
                    }
                    if let Some(t) = dot.at {
                        parts.push(format!("{} ago", k8s::age(t)));
                    }
                    if let Some(by) = &dot.by {
                        parts.push(format!("by {by}"));
                    }
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .child(div().size(px(7.)).rounded_full().bg(if dot.ok { theme.success } else { theme.danger }))
                        .child(selectable(parts.join(" · ")))
                }))
            })
    }
}

/// The In cluster card: the commit each source deploys.
fn in_cluster_card(c: &Cards, cx: &App) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let several = c.deployed.len() > 1;
    card("In cluster", None, cx)
        .when(c.deployed.is_empty(), |v| v.child(div().text_sm().text_color(muted).child("Nothing deployed yet.")))
        .children(c.deployed.iter().map(|(dep, commit)| {
            let repo = dep.repo.trim_end_matches(".git").rsplit('/').next().unwrap_or_default().to_string();
            let head = match &dep.chart {
                Some(chart) => format!("chart {chart} {}", dep.revision),
                None => short_revision(&dep.revision),
            };
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().font_family(theme.mono_font_family.clone()).child(selectable(head)))
                        .when(several || !repo.is_empty(), |h| {
                            h.child(div().text_xs().text_color(muted).child(selectable(repo)))
                        }),
                )
                .child(commit_lines(commit.as_ref(), dep.chart.is_some(), c.note.is_some(), cx))
        }))
}

fn commit_lines(commit: Option<&Result<Commit, String>>, chart: bool, noted: bool, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    match commit {
        Some(Ok(c)) => {
            let mut by = Vec::new();
            if let Some(a) = &c.author {
                by.push(a.clone());
            }
            if let Some(t) = c.date {
                by.push(format!("{} ago", k8s::age(t)));
            }
            if !c.tags.is_empty() {
                by.push(c.tags.join(", "));
            }
            v_flex()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(selectable(c.message.lines().next().unwrap_or_default().to_string())),
                )
                .child(div().text_xs().text_color(muted).child(selectable(by.join(" · "))))
        }
        Some(Err(e)) => v_flex()
            .child(div().text_xs().text_color(muted).child(selectable(format!("Its commit couldn't be read: {e}")))),
        None if chart => v_flex().child(div().text_xs().text_color(muted).child("A chart's version: no commit.")),
        None if noted => v_flex()
            .child(div().text_xs().text_color(muted).child("Its commit is read from Argo CD's API (see above).")),
        None => v_flex(),
    }
}

/// A card: its title, and its background tinted by `tone` when it says something is right
/// or wrong.
fn card(title: &str, tone: Option<Tone>, cx: &App) -> Div {
    let theme = cx.theme();
    let tint = tone.filter(|t| *t != Tone::Done).map(|t| t.color(cx).opacity(0.08));
    v_flex()
        .w(px(CARD))
        .flex_none()
        .gap_1p5()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(tone.filter(|t| *t != Tone::Done).map_or(theme.border, |t| t.color(cx).opacity(0.5)))
        .bg(tint.unwrap_or(theme.background))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(title.to_uppercase()),
        )
}

/// A status in its tone, boxed.
fn chip(text: &str, tone: Tone, cx: &App) -> Div {
    let color = tone.color(cx);
    div().flex_none().px_1p5().rounded_md().text_xs().text_color(color).bg(color.opacity(0.12)).child(text.to_string())
}

/// A word that does something when clicked.
fn link(id: impl Into<ElementId>, text: &str, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .text_xs()
        .text_color(cx.theme().blue)
        .cursor_pointer()
        .hover(Styled::underline)
        .child(text.to_string())
}
