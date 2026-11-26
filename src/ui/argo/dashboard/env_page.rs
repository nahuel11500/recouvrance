//! The environment page's action bar, under its title, and its full-width views: Resources by
//! category, Sync status (the last sync per resource), History and rollback, Details, Diff
//! (see `k8s::argo::dashboard::views`).
//!
//! The actions are the row menu's (`ArgoAction`, `confirm_api_action`) on the Application whose
//! tab is shown, with the same checks: a read-only server refused first, write mode when sent,
//! a rollback only with auto-sync off. Sync… opens the sync page (`sync`) with its options.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;

use super::super::{ArgoView, Key, Offers};
use super::{Dashboard, card, chip, link, with_tip};
use crate::details::{ArgoAction, OpenOn};
use crate::k8s;
use crate::k8s::argo::dashboard::{Cards, Category, Node, SyncRow, health_tone};
use crate::k8s::argo::diagnosis::{Res, span_text};
use crate::k8s::argo::short_revision;
use crate::k8s::argo::windows::{NextSync, Planner};
use crate::status::Tone;
use crate::ui::selectable::selectable;

/// The views of the environment page, one shown at a time under the action bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ui::argo) enum View {
    /// The cards.
    #[default]
    Overview,
    Resources,
    SyncStatus,
    History,
    Details,
    Diff,
}

/// The tabs of the Details view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ui::argo) enum DetailsTab {
    #[default]
    Summary,
    Sources,
    Manifest,
}

/// What the Resources and Sync status views are filtered by.
#[derive(Clone, Debug, Default)]
pub(in crate::ui::argo) struct ViewFilters {
    pub only_problems: bool,
    pub health: Option<String>,
    pub sync_status: Option<String>,
    pub sync_health: Option<String>,
    pub sync_hook: Option<bool>,
}

/// A time as the page says it: the local date and hour, and how long ago.
fn stamp(t: Timestamp) -> String {
    let at = t.to_zoned_datetime(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M:%S").to_string();
    format!("{at} ({} ago)", k8s::age(t))
}

/// A choice among a few, highlighted when picked.
fn choice(id: impl Into<ElementId>, label: String, on: bool) -> Button {
    Button::new(id).xsmall().when(on, ButtonVariants::primary).when(!on, ButtonVariants::ghost).label(label)
}

impl ArgoView {
    /// Show the view `view` of the environment page.
    pub(super) fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.dashboard {
            d.view = view;
            // History is its list, unfolded.
            if view == View::History {
                d.open.insert("history".to_string());
            }
        }
        cx.notify();
    }

    fn change_view_filters(&mut self, f: impl FnOnce(&mut ViewFilters), cx: &mut Context<Self>) {
        if let Some(d) = &mut self.dashboard {
            f(&mut d.filters);
        }
        cx.notify();
    }

    /// Open the sync page for the Application shown, or (`all`) every one of the environment.
    fn sync_from_page(&mut self, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &self.dashboard else { return };
        let keys: Vec<Key> = if all { d.members.clone() } else { d.current().cloned().into_iter().collect() };
        let apps = keys.iter().filter_map(|k| self.find(k).cloned()).collect();
        self.sync_apps(apps, 0, window, cx);
    }

    /// Confirm, then delete the Application `k` by Argo CD's API (see `confirm_api_delete`).
    fn delete_app(&mut self, k: &Key, cascade: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.find(k).cloned() else { return };
        crate::details::confirm_api_delete(&a.server, &a.app.namespace, &a.app.name, cascade, window, cx);
    }

    /// Open `res`, an object the Application `k` deployed, on the context that reaches its
    /// cluster.
    fn open_deployed(&mut self, k: &Key, res: Res, cx: &mut Context<Self>) {
        let Some(context) = self.find(k).and_then(|a| a.on.clone()) else { return };
        cx.emit(OpenOn { context, res });
    }

    /// When a sync by hand can start, under the sync windows of the Application `k`'s project.
    fn manual_sync(&self, k: &Key) -> Option<(NextSync, Timestamp)> {
        let rows = self.rows.as_ref()?;
        let a = rows.fleet.apps.iter().find(|a| super::super::key(a) == *k)?;
        let mut plan = Planner::new(rows.now);
        Some((rows.fleet.sync_window(a, &mut plan, true), rows.now))
    }

    /// The buttons under the title: the actions on the Application shown (and Sync
    /// environment… on all of them), then the views.
    pub(super) fn action_bar(&self, d: &Dashboard, c: Option<&Cards>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let k = d.current().cloned().filter(|k| self.find(k).is_some());
        let found = k.as_ref().and_then(|k| self.find(k));
        let offers = found.map(|a| Offers::of(&a.app));
        let running = offers.is_some_and(|o| o.terminate)
            || c.is_some_and(|c| c.summary.in_progress || c.last_sync.as_ref().is_some_and(|s| s.running));
        let roll_back = c.map_or(offers.is_some_and(|o| o.roll_back), |c| c.versions.rollback_to.is_some());
        let auto_sync = c.is_some_and(|c| c.versions.auto_sync);
        let several = d.members.len() > 1;
        let act = |id: &'static str, label: &'static str, icon: Lucide, action: ArgoAction, cx: &mut Context<Self>| {
            let k = k.clone();
            Button::new(id).small().outline().icon(icon).label(label).disabled(k.is_none()).on_click(cx.listener(
                move |this, _, window, cx| {
                    if let Some(k) = &k {
                        this.act(k, action, window, cx);
                    }
                },
            ))
        };
        // When a sync by hand can start: said next to Sync, so it is seen before syncing.
        let next = k.as_ref().and_then(|k| self.manual_sync(k)).map(|(next, now)| {
            let (text, color) = match &next {
                NextSync::Now | NextSync::NoWindow => ("Can sync now".to_string(), muted),
                NextSync::At(_) => {
                    (format!("A sync window denies a sync: it can sync {}", next.text(now)), theme.warning)
                }
                NextSync::NotSoon => (format!("A sync window denies a sync, {}", next.text(now)), theme.danger),
                NextSync::Unknown(why) => (format!("Sync windows unknown: {why}"), muted),
            };
            div().text_xs().text_color(color).child(text)
        });
        let more = {
            let view = cx.entity().downgrade();
            let k = k.clone();
            let has_destination = found.is_some_and(|a| a.on.is_some() && !a.dest.namespace.is_empty());
            Button::new("argo-dash-more")
                .small()
                .ghost()
                .icon(Lucide::Ellipsis)
                .accessibility_label("More actions")
                .disabled(k.is_none())
                .dropdown_menu(move |menu: PopupMenu, _, _| {
                    let Some(k) = k.clone() else { return menu };
                    let delete = |cascade: bool| {
                        let (view, k) = (view.clone(), k.clone());
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let _ = view.update(cx, |this, cx| this.delete_app(&k, cascade, window, cx));
                        }
                    };
                    let destination = {
                        let (view, k) = (view.clone(), k.clone());
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            let _ = view.update(cx, |this, cx| this.open_destination(&k, cx));
                        }
                    };
                    menu.item(
                        PopupMenuItem::new("Open destination namespace")
                            .icon(Lucide::FolderOpen)
                            .disabled(!has_destination)
                            .on_click(destination),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("Delete with what it deployed…").icon(Lucide::Trash).on_click(delete(true)),
                    )
                    .item(
                        PopupMenuItem::new("Delete, keeping what it deployed…")
                            .icon(Lucide::Trash)
                            .on_click(delete(false)),
                    )
                })
        };
        let open = k.clone().map(|k| {
            Button::new("argo-dash-open")
                .small()
                .ghost()
                .icon(Lucide::ExternalLink)
                .label("Open in Argo CD")
                .on_click(cx.listener(move |this, _, _, cx| this.open(&k, cx)))
        });
        let actions = h_flex()
            .flex_wrap()
            .gap_1p5()
            .child(
                Button::new("argo-dash-sync")
                    .small()
                    .primary()
                    .icon(Lucide::GitBranch)
                    .label("Sync…")
                    .disabled(k.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.sync_from_page(false, window, cx))),
            )
            .when(several, |h| {
                h.child(
                    Button::new("argo-dash-sync-env")
                        .small()
                        .outline()
                        .label("Sync environment…")
                        .disabled(k.is_none())
                        .on_click(cx.listener(|this, _, window, cx| this.sync_from_page(true, window, cx))),
                )
            })
            .children(next)
            .child(act("argo-dash-refresh-app", "Refresh", Lucide::RefreshCw, ArgoAction::Refresh, cx))
            .child(act("argo-dash-hard-refresh", "Hard refresh", Lucide::RefreshCcwDot, ArgoAction::HardRefresh, cx))
            .when(running, |h| {
                h.child(act("argo-dash-terminate", "Terminate…", Lucide::CircleStop, ArgoAction::Terminate, cx))
            })
            .when(roll_back, |h| {
                h.child(act("argo-dash-roll-back", "Roll back…", Lucide::Undo2, ArgoAction::Rollback(None), cx))
            })
            .when(!roll_back && auto_sync, |h| {
                h.child(
                    with_tip("argo-dash-no-roll-back", "Auto-sync is on: Argo CD allows no rollback.".into()).child(
                        Button::new("argo-dash-roll-back-off")
                            .small()
                            .outline()
                            .icon(Lucide::Undo2)
                            .label("Roll back…")
                            .disabled(true),
                    ),
                )
            })
            .child(more)
            .child(div().flex_1())
            .children(open);
        let tab = |id: &'static str, label: String, icon: Lucide, view: View, cx: &mut Context<Self>| {
            let on = d.view == view;
            Button::new(id)
                .small()
                .icon(icon)
                .when(on, ButtonVariants::primary)
                .when(!on, ButtonVariants::ghost)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_view(view, cx)))
        };
        let counted = |label: &str, n: Option<usize>| match n {
            Some(n) if n > 0 => format!("{label} ({n})"),
            _ => label.to_string(),
        };
        let troubled = c.map(|c| c.nodes.iter().filter(|n| n.in_trouble()).count());
        let resources = match (c.map(|c| c.nodes.len()), troubled) {
            (Some(n), Some(bad)) if bad > 0 => format!("Resources ({n}, {bad} in trouble)"),
            (n, _) => counted("Resources", n),
        };
        let views = h_flex()
            .flex_wrap()
            .gap_1()
            .child(tab("argo-dash-view-overview", "Overview".into(), Lucide::LayoutDashboard, View::Overview, cx))
            .child(tab("argo-dash-view-resources", resources, Lucide::Boxes, View::Resources, cx))
            .child(tab("argo-dash-view-sync", "Sync status".into(), Lucide::ListChecks, View::SyncStatus, cx))
            .child(tab("argo-dash-view-history", "History and rollback".into(), Lucide::ScrollText, View::History, cx))
            .child(tab("argo-dash-view-details", "Details".into(), Lucide::Info, View::Details, cx))
            .child(tab(
                "argo-dash-view-diff",
                counted("Diff", c.map(|c| c.diff.total())),
                Lucide::GitCompare,
                View::Diff,
                cx,
            ));
        v_flex().gap_1p5().child(actions).child(views)
    }

    /// The view picked, full width; the cards for the Overview.
    pub(super) fn view_body(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        match d.view {
            View::Overview => self.cards(d, c, cx),
            View::Resources => self.resources_view(d, c, cx),
            View::SyncStatus => self.sync_status_view(d, c, cx),
            View::History => self.history_view(d, c, cx),
            View::Details => self.details_view(d, c, cx),
            View::Diff => self.diff_view(d, c, cx),
        }
    }

    // MARK: Resources

    /// The resources, a column per category, each with its health, kind, name and pods
    /// ready; filtered by name or kind, by those in trouble, and by health. A click opens
    /// one on its cluster.
    fn resources_view(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let f = &d.filters;
        let kept: Vec<&Node> =
            c.nodes.iter().filter(|n| n.keeps(&self.res_query, f.only_problems, f.health.as_deref())).collect();
        let troubled = c.nodes.iter().filter(|n| n.in_trouble()).count();
        let mut healths: Vec<String> = c.nodes.iter().filter_map(|n| n.health.clone()).collect();
        healths.sort();
        healths.dedup();
        let k = d.current().cloned();
        let reaches = k.as_ref().and_then(|k| self.find(k)).and_then(|a| a.on.clone());
        let filters = h_flex()
            .flex_wrap()
            .gap_2()
            .child(div().w(px(260.)).child(Input::new(&self.res_search).small().cleanable(true)))
            .child(
                Button::new("argo-res-problems")
                    .small()
                    .when(f.only_problems, ButtonVariants::primary)
                    .when(!f.only_problems, Button::outline)
                    .label(format!("Only problems ({troubled})"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.change_view_filters(|f| f.only_problems = !f.only_problems, cx);
                    })),
            )
            .children(healths.into_iter().enumerate().map(|(i, h)| {
                let on = f.health.as_deref() == Some(h.as_str());
                let picked = h.clone();
                Button::new(("argo-res-health", i))
                    .small()
                    .when(on, ButtonVariants::primary)
                    .when(!on, Button::outline)
                    .label(h)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let picked = picked.clone();
                        this.change_view_filters(
                            |f| f.health = if f.health.as_ref() == Some(&picked) { None } else { Some(picked) },
                            cx,
                        );
                    }))
            }))
            .child(div().text_xs().text_color(muted).child(format!("{} shown of {}", kept.len(), c.nodes.len())));
        let source = if c.problems.from_tree {
            match &reaches {
                Some(ctx) => format!("From Argo CD's resource tree. A click opens a resource on {ctx}."),
                None => "From Argo CD's resource tree. No context of your kubeconfig reaches its cluster.".to_string(),
            }
        } else {
            "From the Application's status (its resource tree wasn't read): pods unknown.".to_string()
        };
        let columns = Category::ALL.iter().filter_map(|&cat| {
            let nodes: Vec<(usize, &Node)> =
                kept.iter().enumerate().filter(|(_, n)| n.category == cat).map(|(i, n)| (i, *n)).collect();
            if nodes.is_empty() {
                return None;
            }
            let bad = nodes.iter().filter(|(_, n)| n.in_trouble()).count();
            let tone = (bad > 0).then_some(Tone::Bad);
            Some(
                card(&format!("{} · {}", cat.title(), nodes.len()), tone, cx)
                    .flex_1()
                    .flex_basis(px(280.))
                    .min_w(px(240.))
                    .children(nodes.into_iter().map(|(i, n)| self.node_row(i, n, k.as_ref(), reaches.is_some(), cx))),
            )
        });
        v_flex()
            .gap_2()
            .pb_4()
            .child(filters)
            .child(div().text_xs().text_color(muted).child(source))
            .when(kept.is_empty(), |v| {
                v.child(div().p_2().text_sm().text_color(muted).child(if c.nodes.is_empty() {
                    "Argo CD lists no resource of it."
                } else {
                    "No resource matches the filters."
                }))
            })
            .child(h_flex().flex_wrap().items_start().gap_3().children(columns))
            .into_any_element()
    }

    /// One resource: its health's dot, its kind, its name and its pods ready.
    fn node_row(&self, i: usize, n: &Node, k: Option<&Key>, reaches: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme().clone();
        let mut tip = vec![n.res.label()];
        tip.push(format!("Health: {}", n.health.as_deref().unwrap_or("none")));
        if let Some(m) = &n.message {
            tip.push(m.clone());
        }
        if let Some((ready, all)) = n.pods {
            tip.push(format!("{ready} of {} ready", crate::plural::count(all, "pod")));
        }
        if reaches {
            tip.push("Click to open it on its cluster".into());
        }
        let pods = n.pods.map(|(ready, all)| {
            let color = if ready < all { theme.warning } else { theme.muted_foreground };
            div().flex_none().text_xs().text_color(color).child(format!("{ready}/{all}"))
        });
        let (k, res) = (k.cloned(), n.res.clone());
        with_tip(("argo-res", i), tip.join("\n"))
            .flex()
            .flex_row()
            .items_center()
            .gap_1p5()
            .min_w_0()
            .px_1p5()
            .py_0p5()
            .rounded_md()
            .text_sm()
            .when(n.in_trouble(), |h| h.bg(theme.danger.opacity(0.08)))
            .child(div().flex_none().size(px(8.)).rounded_full().bg(n.tone.color(cx)))
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .rounded(px(4.))
                    .bg(theme.muted)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(n.res.kind.clone()),
            )
            .child(div().flex_1().min_w_0().truncate().child(n.res.name.clone()))
            .children(pods)
            .when(reaches, |h| {
                h.cursor_pointer().hover(|s| s.bg(theme.secondary)).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(k) = &k {
                        this.open_deployed(k, res.clone(), cx);
                    }
                }))
            })
    }

    // MARK: Sync status

    /// The last operation: what, its phase and message whole, when, how long, its revision and
    /// who started it; then its result per resource, in the order Argo CD runs them, filtered by
    /// status, health and hook.
    fn sync_status_view(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(op) = &c.operation else {
            return div().p_2().text_sm().text_color(muted).child("No sync yet.").into_any_element();
        };
        let now = Timestamp::now();
        let fact = |label: &str, value: String| {
            h_flex()
                .gap_2()
                .text_sm()
                .child(div().w(px(110.)).flex_none().text_color(muted).child(label.to_string()))
                .child(div().min_w_0().child(selectable(value)))
        };
        let duration = op.started.map(|s| {
            let end = op.finished.unwrap_or(now);
            let secs = (end.as_second() - s.as_second()).max(0);
            if op.finished.is_some() { span_text(secs) } else { format!("{} so far", span_text(secs)) }
        });
        let journal = self.journal_link("argo-dash-journal-status", d, cx);
        let header = card("Operation", Some(op.tone), cx)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .text_color(op.tone.color(cx))
                            .child(op.phase.clone()),
                    )
                    .child(div().text_sm().child(op.kind.clone())),
            )
            .when_some(op.message.clone(), |v, m| v.child(fact("Message", m)))
            .when_some(op.started, |v, t| v.child(fact("Started", stamp(t))))
            .when_some(duration, |v, t| v.child(fact("Duration", t)))
            .when_some(op.finished, |v, t| v.child(fact("Finished", stamp(t))))
            .when_some(op.revision.clone(), |v, r| v.child(fact("Revision", short_revision(&r))))
            .when_some(op.by.clone(), |v, by| v.child(fact("Initiated by", by)))
            .when_some(op.retries, |v, n| v.child(fact("Retried", crate::plural::count(n as usize, "time"))))
            .children(journal.map(|j| h_flex().child(j)));
        let f = &d.filters;
        let rows: Vec<&SyncRow> = c
            .sync_rows
            .iter()
            .filter(|r| r.keeps(f.sync_status.as_deref(), f.sync_health.as_deref(), f.sync_hook))
            .collect();
        let values = |get: fn(&SyncRow) -> String| {
            let mut v: Vec<String> = c.sync_rows.iter().map(get).filter(|s| !s.is_empty()).collect();
            v.sort();
            v.dedup();
            v
        };
        let statuses = values(|r| r.step.result().to_string());
        let healths = values(|r| r.health.clone().unwrap_or_else(|| "None".into()));
        let picker = |id: &'static str,
                      label: &'static str,
                      values: Vec<String>,
                      current: Option<String>,
                      set: fn(&mut ViewFilters, Option<String>),
                      cx: &mut Context<Self>| {
            h_flex()
                .gap_1()
                .flex_wrap()
                .child(div().w(px(56.)).text_xs().text_color(muted).child(label))
                .child(
                    choice((id, 0usize), "All".into(), current.is_none())
                        .on_click(cx.listener(move |this, _, _, cx| this.change_view_filters(|f| set(f, None), cx))),
                )
                .children(values.into_iter().enumerate().map(|(i, v)| {
                    let on = current.as_deref() == Some(v.as_str());
                    let picked = v.clone();
                    choice((id, i + 1), v, on).on_click(cx.listener(move |this, _, _, cx| {
                        let picked = picked.clone();
                        this.change_view_filters(|f| set(f, Some(picked)), cx);
                    }))
                }))
        };
        let hook = |id: usize, label: &str, value: Option<bool>, cx: &mut Context<Self>| {
            choice(("argo-sync-f-hook", id), label.to_string(), f.sync_hook == value)
                .on_click(cx.listener(move |this, _, _, cx| this.change_view_filters(|f| f.sync_hook = value, cx)))
        };
        let filters = v_flex()
            .gap_1()
            .child(picker(
                "argo-sync-f-status",
                "Status",
                statuses,
                f.sync_status.clone(),
                |f, v| f.sync_status = v,
                cx,
            ))
            .child(picker("argo-sync-f-health", "Health", healths, f.sync_health.clone(), |f, v| f.sync_health = v, cx))
            .child(
                h_flex()
                    .gap_1()
                    .child(div().w(px(56.)).text_xs().text_color(muted).child("Hook"))
                    .child(hook(0, "All", None, cx))
                    .child(hook(1, "Hooks", Some(true), cx))
                    .child(hook(2, "Not hooks", Some(false), cx)),
            );
        let cell = |w: f32| div().w(px(w)).flex_none().min_w_0().truncate();
        let head = h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(theme.border)
            .child(cell(44.).child("Wave"))
            .child(cell(140.).child("Kind"))
            .child(cell(130.).child("Namespace"))
            .child(div().flex_1().min_w(px(140.)).child("Name"))
            .child(cell(100.).child("Status"))
            .child(cell(96.).child("Health"))
            .child(cell(80.).child("Hook"))
            .child(div().flex_1().min_w(px(160.)).child("Message"));
        let table = rows.iter().enumerate().map(|(i, r)| {
            let st = &r.step;
            let result = st.result().to_string();
            let tone = crate::status::tone(&result).unwrap_or(Tone::Done);
            let health = r.health.clone().unwrap_or_else(|| "None".into());
            let hook = if r.hook { st.hook_type.clone() } else { String::new() };
            h_flex()
                .id(("argo-sync-row", i))
                .gap_2()
                .px_2()
                .py_0p5()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(cell(44.).text_color(muted).child(r.wave.to_string()))
                .child(cell(140.).child(selectable(st.kind.clone())))
                .child(cell(130.).text_color(muted).child(selectable(st.namespace.clone())))
                .child(div().flex_1().min_w(px(140.)).truncate().child(selectable(st.name.clone())))
                .child(cell(100.).child(chip(&result, tone, cx)))
                .child(cell(96.).child(chip(&health, health_tone(r.health.as_deref()), cx)))
                .child(cell(80.).text_xs().text_color(muted).child(hook))
                .child(div().flex_1().min_w(px(160.)).text_xs().child(selectable(st.message.clone())))
        });
        v_flex()
            .gap_2()
            .pb_4()
            .child(header)
            .child(filters)
            .child(div().text_xs().text_color(muted).child(format!(
                "{} shown of {} · in the order Argo CD runs them: phase, wave, hooks first",
                rows.len(),
                crate::plural::count(c.sync_rows.len(), "resource")
            )))
            .child(v_flex().child(head).children(table))
            .into_any_element()
    }

    // MARK: History

    /// The history full width, its list unfolded, with the Versions card (its rollback).
    fn history_view(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_3()
            .pb_4()
            .child(self.history_card(d, c, cx))
            .when_some(d.compare.as_ref(), |v, cmp| v.child(self.compare_view(cmp, cx)))
            .child(self.versions_card(d, c, cx))
            .into_any_element()
    }

    // MARK: Diff

    /// What a sync would change, full width.
    fn diff_view(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        if c.diff.total() == 0 {
            let muted = cx.theme().muted_foreground;
            return div()
                .p_2()
                .text_sm()
                .text_color(muted)
                .child("In sync: the cluster matches git.")
                .into_any_element();
        }
        frame_full(cx).child(self.diff_body(d, c, cx)).into_any_element()
    }

    // MARK: Details

    /// The Application's details: Summary, Sources, Manifest.
    fn details_view(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let det = &c.details;
        let tab = |id: &'static str, label: &'static str, t: DetailsTab, cx: &mut Context<Self>| {
            choice(id, label.to_string(), d.details_tab == t).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(d) = &mut this.dashboard {
                    d.details_tab = t;
                }
                cx.notify();
            }))
        };
        let tabs = h_flex()
            .gap_1()
            .child(tab("argo-det-summary", "Summary", DetailsTab::Summary, cx))
            .child(tab("argo-det-sources", "Sources", DetailsTab::Sources, cx))
            .child(tab("argo-det-manifest", "Manifest", DetailsTab::Manifest, cx));
        let row = |label: &str, value: AnyElement| {
            h_flex()
                .gap_3()
                .items_start()
                .py_0p5()
                .text_sm()
                .child(div().w(px(140.)).flex_none().text_color(muted).child(label.to_string()))
                .child(div().flex_1().min_w_0().child(value))
        };
        let text = |s: String| selectable(s).into_any_element();
        let body: AnyElement = match d.details_tab {
            DetailsTab::Summary => {
                let pairs = |list: &[(String, String)]| {
                    v_flex()
                        .gap_0p5()
                        .children(list.iter().map(|(k, v)| div().text_xs().child(selectable(format!("{k}={v}")))))
                        .into_any_element()
                };
                let annotations = if d.is_open("annotations") {
                    v_flex()
                        .gap_0p5()
                        .child(pairs(&det.annotations))
                        .child(self.toggle_link("annotations-fold", "annotations", "hide", d, cx))
                        .into_any_element()
                } else {
                    let label = format!("{} (show)", crate::plural::count(det.annotations.len(), "annotation"));
                    h_flex().child(self.toggle_link("annotations", "annotations", &label, d, cx)).into_any_element()
                };
                let cluster = match (det.cluster.is_empty(), det.server.is_empty()) {
                    (false, false) => format!("{} ({})", det.cluster, det.server),
                    (false, true) => det.cluster.clone(),
                    _ => det.server.clone(),
                };
                let status = match &det.revision {
                    Some(r) => format!("{} to {r}", det.sync),
                    None => det.sync.clone(),
                };
                let health = match &det.health_message {
                    Some(m) => format!("{} · {m}", det.health),
                    None => det.health.clone(),
                };
                let urls = v_flex()
                    .gap_0p5()
                    .children(det.urls.iter().enumerate().map(|(i, u)| {
                        let url = u.clone();
                        link(("argo-det-url", i), u, cx).on_click(move |_, _, cx| cx.open_url(&url))
                    }))
                    .into_any_element();
                let images = v_flex()
                    .gap_0p5()
                    .children(det.images.iter().map(|i| {
                        div().text_xs().font_family(theme.mono_font_family.clone()).child(selectable(i.clone()))
                    }))
                    .into_any_element();
                let namespace = det.namespace.clone();
                let copy_ns = with_tip("argo-det-ns", "Click to copy".into())
                    .cursor_pointer()
                    .child(namespace.clone())
                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(namespace.clone())))
                    .into_any_element();
                let or_none = |v: &[String]| if v.is_empty() { "none".to_string() } else { v.join(", ") };
                v_flex()
                    .child(row("Project", text(det.project.clone())))
                    .child(row("Labels", if det.labels.is_empty() { text("none".into()) } else { pairs(&det.labels) }))
                    .child(row("Annotations", annotations))
                    .child(row("Cluster", text(cluster)))
                    .child(row("Namespace", copy_ns))
                    .child(row("Created", text(det.created.map_or_else(|| "unknown".into(), stamp))))
                    .child(row("Auto-sync", text(det.auto_sync.clone())))
                    .child(row("Sync options", text(or_none(&det.sync_options))))
                    .child(row("Retry", text(det.retry.clone().unwrap_or_else(|| "off".into()))))
                    .child(row("Status", text(status)))
                    .child(row("Health", text(health)))
                    .child(row("Links", if det.urls.is_empty() { text("none".into()) } else { urls }))
                    .child(row("Images", if det.images.is_empty() { text("none".into()) } else { images }))
                    .into_any_element()
            }
            DetailsTab::Sources => v_flex()
                .gap_1()
                .when(det.sources.is_empty(), |v| v.child(div().text_sm().text_color(muted).child("No source read.")))
                .children(det.sources.iter().enumerate().map(|(i, s)| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().w(px(24.)).text_color(muted).child(format!("{}", i + 1)))
                        .child(div().min_w_0().font_family(theme.mono_font_family.clone()).child(selectable(s.clone())))
                }))
                .into_any_element(),
            DetailsTab::Manifest => {
                let yaml = det.manifest.clone();
                v_flex()
                    .gap_1()
                    .child(h_flex().child(link("argo-det-copy", "copy", cx).on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(yaml.clone()));
                    })))
                    .child(
                        div()
                            .p_2()
                            .rounded_md()
                            .bg(theme.muted)
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .children(det.manifest.lines().map(|l| div().child(selectable(l.to_string())))),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .gap_2()
            .pb_4()
            .child(tabs)
            .when(c.note.is_some(), |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("From the Applications list's copy: some details aren't known until it is read again."),
                )
            })
            .child(frame_full(cx).child(body))
            .into_any_element()
    }
}

/// A box full width.
fn frame_full(cx: &App) -> Div {
    super::frame(cx).w_full()
}
