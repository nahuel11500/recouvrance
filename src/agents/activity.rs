//! The activity log of agent calls: grouped into tasks, repeats folded, filtered,
//! with older calls read back from the log on disk.

#[allow(unused_imports)]
use super::*;

use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;

/// A pause longer than this starts a new group, even within one session.
const GAP_SECS: i64 = 10 * 60;
/// Calls shown at first, and added by "Show older".
const PAGE: usize = 300;
/// Arguments that say what a call worked on: calls on the same target fold together.
const TARGET: &[&str] = &["context", "namespace", "kind", "resource", "name", "pod", "container", "workload", "node"];

#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub(super) enum Shown {
    #[default]
    All,
    Writes,
    Problems,
}

/// The All activity page's state.
pub(super) struct Activity {
    list: ListState,
    pub(super) shown: Shown,
    pub(super) context: Option<String>,
    /// Only this agent's calls.
    pub(super) agent: Option<String>,
    pub(super) search: Entity<InputState>,
    limit: usize,
    /// Expanded runs and rows, by key.
    open: HashSet<String>,
    /// Calls read from the log on disk, oldest first (None: not read yet).
    disk: Option<Rc<Vec<AuditEntry>>>,
    loading: bool,
    /// What the items were built from: rebuilt when it changes.
    sig: u64,
    items: Rc<Vec<Item>>,
    contexts: Vec<String>,
}

impl Activity {
    pub(super) fn new(window: &mut Window, cx: &mut Context<AgentsView>) -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(600.)),
            shown: Shown::All,
            context: None,
            agent: None,
            search: cx.new(|cx| InputState::new(window, cx).placeholder("Search tools, arguments, results…")),
            limit: PAGE,
            open: HashSet::new(),
            disk: None,
            loading: false,
            sig: 0,
            items: Rc::new(Vec::new()),
            contexts: Vec::new(),
        }
    }
}

/// Calls of one agent with no pause longer than [`GAP_SECS`], in one session.
pub(super) struct Group {
    pub runs: Vec<Vec<AuditEntry>>,
}

impl Group {
    fn entries(&self) -> impl Iterator<Item = &AuditEntry> {
        self.runs.iter().flatten()
    }
}

/// Split calls (newest first) into groups, and each group into runs of the same
/// tool on the same target with the same outcome.
pub(super) fn group_calls(entries: Vec<AuditEntry>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for e in entries {
        let prev = groups.last().and_then(|g| g.runs.last()).and_then(|r| r.last());
        let same_group = prev.is_some_and(|p| {
            p.agent_id == e.agent_id && p.session == e.session && p.time.duration_since(e.time).as_secs() <= GAP_SECS
        });
        if !same_group {
            groups.push(Group { runs: vec![vec![e]] });
            continue;
        }
        let Some(g) = groups.last_mut() else { continue };
        let joins = g
            .runs
            .last()
            .and_then(|r| r.last())
            .is_some_and(|p| p.tool == e.tool && target(p) == target(&e) && problem(p) == problem(&e));
        match g.runs.last_mut() {
            Some(run) if joins => run.push(e),
            _ => g.runs.push(vec![e]),
        }
    }
    groups
}

fn target(e: &AuditEntry) -> Vec<(&str, String)> {
    TARGET.iter().filter_map(|k| Some((*k, arg(e, k)?))).collect()
}

fn arg(e: &AuditEntry, key: &str) -> Option<String> {
    match e.args.get(key)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        v => Some(v.to_string()),
    }
}

fn problem(e: &AuditEntry) -> bool {
    matches!(e.decision, Decision::Denied | Decision::Rejected | Decision::Failed)
}

fn writes(e: &AuditEntry) -> bool {
    crate::mcp::server::tool_writes(&e.tool)
}

/// What a group touched: contexts, namespaces, then objects, a few of each.
pub(super) fn touched(g: &Group) -> String {
    let set = |keys: &[&str]| -> BTreeSet<String> {
        g.entries().flat_map(|e| keys.iter().filter_map(|k| arg(e, k))).collect()
    };
    let few = |s: BTreeSet<String>| {
        let n = s.len();
        let mut out: Vec<String> = s.into_iter().take(3).collect();
        if n > 3 {
            out.push(format!("+{}", n - 3));
        }
        out.join(", ")
    };
    [set(&["context"]), set(&["namespace"]), set(&["pod", "name", "workload", "node"])]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(few)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// One call in a line: the command for exec, else its arguments (without the
/// context when the group header already names it).
pub(super) fn call_text(e: &AuditEntry, with_context: bool) -> String {
    if e.tool == "exec_command" {
        let cmd: Vec<String> = e
            .args
            .get("command")
            .and_then(|c| c.as_array())
            .map(|a| a.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect())
            .unwrap_or_default();
        let pod = arg(e, "pod").unwrap_or_default();
        let at = match arg(e, "container") {
            Some(c) => format!("{pod}/{c}"),
            None => pod,
        };
        return format!("{at} $ {}", cmd.join(" "));
    }
    let mut args = e.args.clone();
    if !with_context && let Some(map) = args.as_object_mut() {
        map.remove("context");
    }
    compact_args(&args)
}

/// Does a call pass the filters? `needle` is lowercase.
pub(super) fn keep(e: &AuditEntry, shown: Shown, context: Option<&str>, agent: Option<&str>, needle: &str) -> bool {
    let shown_ok = match shown {
        Shown::All => true,
        Shown::Writes => writes(e),
        Shown::Problems => problem(e),
    };
    shown_ok
        && agent.is_none_or(|a| e.agent_id == a)
        && context.is_none_or(|c| arg(e, "context").as_deref() == Some(c))
        && (needle.is_empty()
            || [&e.tool, &e.agent, &e.detail, &e.args.to_string()].iter().any(|t| t.to_lowercase().contains(needle)))
}

#[derive(Clone)]
pub(super) enum Item {
    Header {
        agent: String,
        session: Option<String>,
        first: jiff::Timestamp,
        last: jiff::Timestamp,
        calls: usize,
        writes: usize,
        problems: usize,
        touched: String,
    },
    /// Several calls folded into one line, or a single call.
    Run {
        key: String,
        calls: Vec<AuditEntry>,
        open: bool,
        with_context: bool,
    },
    /// A call of an expanded run.
    Row {
        key: String,
        call: AuditEntry,
        open: bool,
        with_context: bool,
    },
    Detail {
        call: AuditEntry,
    },
    More {
        shown: usize,
        total: usize,
        can_load: bool,
        loading: bool,
    },
}

fn key(e: &AuditEntry) -> String {
    format!("{}-{}", e.time.as_nanosecond(), e.tool)
}

/// The lines of the page, from groups and what is expanded.
pub(super) fn items(groups: &[Group], open: &HashSet<String>) -> Vec<Item> {
    let mut out = Vec::new();
    for g in groups {
        let all: Vec<&AuditEntry> = g.entries().collect();
        let (Some(newest), Some(oldest)) = (all.first(), all.last()) else { continue };
        let contexts: BTreeSet<String> = all.iter().filter_map(|e| arg(e, "context")).collect();
        let with_context = contexts.len() > 1;
        out.push(Item::Header {
            agent: newest.agent.clone(),
            session: newest.session.clone(),
            first: oldest.time,
            last: newest.time,
            calls: all.len(),
            writes: all.iter().filter(|e| writes(e)).count(),
            problems: all.iter().filter(|e| problem(e)).count(),
            touched: touched(g),
        });
        for run in &g.runs {
            let k = key(&run[0]);
            let run_key = format!("run:{k}");
            if run.len() == 1 {
                let row_key = format!("row:{k}");
                let is_open = open.contains(&row_key);
                out.push(Item::Run { key: row_key, calls: run.clone(), open: is_open, with_context });
                if is_open {
                    out.push(Item::Detail { call: run[0].clone() });
                }
                continue;
            }
            let is_open = open.contains(&run_key);
            out.push(Item::Run { key: run_key, calls: run.clone(), open: is_open, with_context });
            if is_open {
                for call in run {
                    let row_key = format!("row:{}", key(call));
                    let row_open = open.contains(&row_key);
                    out.push(Item::Row { key: row_key, call: call.clone(), open: row_open, with_context });
                    if row_open {
                        out.push(Item::Detail { call: call.clone() });
                    }
                }
            }
        }
    }
    out
}

fn fmt_time(t: jiff::Timestamp) -> String {
    t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M:%S").to_string()
}

/// "14:02:11", or "Oct 20 14:02" for another day.
fn fmt_day_time(t: jiff::Timestamp) -> String {
    let tz = jiff::tz::TimeZone::system();
    let z = t.to_zoned(tz.clone());
    if z.date() == jiff::Timestamp::now().to_zoned(tz).date() {
        z.strftime("%H:%M:%S").to_string()
    } else {
        z.strftime("%b %d %H:%M").to_string()
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// Draw one line of the page; clicks expand or fold through `view`.
fn render_item(item: &Item, ix: usize, view: &WeakEntity<AgentsView>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let toggle = |key: &str| {
        let (view, key) = (view.clone(), key.to_string());
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = view.update(cx, |v, cx| {
                if !v.activity.open.remove(&key) {
                    v.activity.open.insert(key.clone());
                }
                cx.notify();
            });
        }
    };
    let call_line = |id: ElementId, calls: &[AuditEntry], open: bool, nested: bool, with_context: bool, key: &str| {
        let e = &calls[0];
        let color = match e.decision {
            Decision::Allowed | Decision::Approved => theme.success,
            Decision::Denied | Decision::Rejected => theme.warning,
            Decision::Failed => theme.danger,
        };
        let side = if calls.len() > 1 {
            let oldest = calls.last().map(|c| c.time).unwrap_or(e.time);
            format!("{} – {}", fmt_time(oldest), fmt_time(e.time))
        } else {
            e.detail.clone()
        };
        h_flex()
            .id(id)
            .w_full()
            .gap_3()
            .py_0p5()
            .px_2()
            .when(nested, |d| d.pl_8())
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.secondary.opacity(0.5)))
            .font_family(theme.mono_font_family.clone())
            .text_xs()
            .on_click(toggle(key))
            .child(Icon::new(if open { Lucide::ChevronDown } else { Lucide::ChevronRight }).size_3().text_color(muted))
            .child(div().w(px(60.)).flex_shrink_0().text_color(muted).child(fmt_time(e.time)))
            .child(div().w(px(70.)).flex_shrink_0().text_color(color).child(format!("{:?}", e.decision)))
            .child(
                div()
                    .w(px(130.))
                    .flex_shrink_0()
                    .when(writes(e), |d| d.text_color(theme.warning))
                    .child(e.tool.clone()),
            )
            .child(
                div()
                    .w(px(36.))
                    .flex_shrink_0()
                    .text_color(theme.blue)
                    .when(calls.len() > 1, |d| d.child(format!("×{}", calls.len()))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(call_text(e, with_context)),
            )
            .child(
                div()
                    .w(px(240.))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(muted)
                    .child(side),
            )
            .child(div().w(px(60.)).flex_shrink_0().text_color(muted).child(format!("{}ms", e.duration_ms)))
    };
    match item {
        Item::Header { agent, session, first, last, calls, writes, problems, touched } => h_flex()
            .w_full()
            .mt_3()
            .gap_3()
            .px_2()
            .py_1()
            .rounded_t_md()
            .text_xs()
            .bg(theme.secondary.opacity(0.6))
            .child(Icon::new(Lucide::MessagesSquare).size_3p5())
            .child(div().font_weight(FontWeight::SEMIBOLD).child(agent.clone()))
            .when_some(session.clone(), |d, s| {
                d.child(div().text_color(muted).child(format!("session {}", s.chars().take(8).collect::<String>())))
            })
            .child(div().text_color(muted).child(format!("{} – {}", fmt_day_time(*first), fmt_time(*last))))
            .child(plural(*calls, "call"))
            .when(*writes > 0, |d| d.child(div().text_color(theme.warning).child(plural(*writes, "write"))))
            .when(*problems > 0, |d| {
                d.child(div().text_color(theme.danger).child(format!("{problems} denied or failed")))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(muted)
                    .child(touched.clone()),
            )
            .into_any_element(),
        Item::Run { key, calls, open, with_context } => {
            call_line(ElementId::from(("act", ix)), calls, *open, false, *with_context, key).into_any_element()
        }
        Item::Row { key, call, open, with_context } => {
            call_line(ElementId::from(("act", ix)), std::slice::from_ref(call), *open, true, *with_context, key)
                .into_any_element()
        }
        Item::Detail { call } => v_flex()
            .w_full()
            .pl_8()
            .pr_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.3))
            .font_family(theme.mono_font_family.clone())
            .text_xs()
            .child(serde_json::to_string_pretty(&call.args).unwrap_or_default())
            .when(!call.detail.is_empty(), |d| d.child(div().text_color(muted).child(call.detail.clone())))
            .child(div().text_color(muted).child(format!(
                "{} · {}ms · {} bytes returned{}",
                call.time.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M:%S"),
                call.duration_ms,
                call.result_bytes,
                call.session.as_ref().map(|s| format!(" · session {s}")).unwrap_or_default()
            )))
            .into_any_element(),
        Item::More { shown, total, can_load, loading } => {
            let view = view.clone();
            h_flex()
                .py_3()
                .gap_3()
                .justify_center()
                .text_xs()
                .text_color(muted)
                .child(format!("Showing {shown} of {}", plural(*total, "call")))
                .when(*loading, |d| d.child("Reading the log on disk…"))
                .when(!*loading && (shown < total || *can_load), |d| {
                    d.child(
                        Button::new("act-older")
                            .xsmall()
                            .outline()
                            .label(if shown < total { "Show older" } else { "Load older calls from disk" })
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |v, cx| v.show_older(cx));
                            }),
                    )
                })
                .into_any_element()
        }
    }
}

impl AgentsView {
    /// Show more calls: from memory first, then from the log on disk.
    fn show_older(&mut self, cx: &mut Context<Self>) {
        let a = &mut self.activity;
        let total = a.items.iter().find_map(|i| match i {
            Item::More { shown, total, .. } => Some((*shown, *total)),
            _ => None,
        });
        if let Some((shown, total)) = total
            && shown < total
        {
            a.limit += PAGE;
            cx.notify();
            return;
        }
        if a.disk.is_some() || a.loading {
            return;
        }
        a.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let disk = cx.background_spawn(async { Hub::audit_on_disk() }).await;
            let _ = this.update(cx, |this, cx| {
                this.activity.disk = Some(Rc::new(disk));
                this.activity.loading = false;
                this.activity.limit += PAGE;
                cx.notify();
            });
        })
        .detach();
    }

    /// Rebuild the page's lines when the log, the filters or what is expanded changed.
    fn refresh_activity(&mut self, cx: &mut Context<Self>) {
        use std::hash::{Hash as _, Hasher as _};
        let a = &self.activity;
        let needle = a.search.read(cx).value().trim().to_lowercase();
        let (shown, context, agent) = (a.shown, a.context.clone(), a.agent.clone());
        let filter = |e: &AuditEntry| keep(e, shown, context.as_deref(), agent.as_deref(), &needle);
        let stats = self.hub.audit_stats(|_| false);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (stats.newest.map(|t| t.as_nanosecond()), stats.oldest.map(|t| t.as_nanosecond())).hash(&mut h);
        (shown, &context, &agent, &needle, a.limit, a.loading, a.disk.as_ref().map(|d| d.len())).hash(&mut h);
        let mut open: Vec<&String> = a.open.iter().collect();
        open.sort();
        open.hash(&mut h);
        let sig = h.finish();
        if sig == a.sig {
            return;
        }
        let mut entries = self.hub.recent_audit(a.limit, filter);
        let mut total = self.hub.audit_stats(filter).matching;
        if let Some(disk) = &a.disk {
            // The log on disk holds what is in memory too: take only what is older.
            let older = || disk.iter().rev().filter(|e| stats.oldest.is_none_or(|o| e.time < o)).filter(|e| filter(e));
            total += older().count();
            let room = a.limit.saturating_sub(entries.len());
            entries.extend(older().take(room).cloned());
        }
        let shown_count = entries.len();
        let mut contexts: BTreeSet<String> =
            self.hub.recent_audit(usize::MAX, |_| true).iter().filter_map(|e| arg(e, "context")).collect();
        if let Some(c) = &context {
            contexts.insert(c.clone());
        }
        let mut lines = items(&group_calls(entries), &a.open);
        lines.push(Item::More {
            shown: shown_count,
            total,
            can_load: a.disk.is_none() && stats.full,
            loading: a.loading,
        });
        let a = &mut self.activity;
        let top = a.list.logical_scroll_top();
        a.list.reset(lines.len());
        a.list.scroll_to(top);
        a.items = Rc::new(lines);
        a.contexts = contexts.into_iter().collect();
        a.sig = sig;
    }

    /// The All activity page: filters, then every group, drawn as it scrolls into view.
    pub(super) fn render_activity_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.refresh_activity(cx);
        let theme = cx.theme().clone();
        let a = &self.activity;
        let view = cx.entity().downgrade();
        let audit_error = self
            .hub
            .audit_error()
            .map(|e| div().text_sm().text_color(theme.danger).child(format!("The audit log is not being saved: {e}")));
        let chip = |id: SharedString, label: String, on: bool, set: Box<dyn Fn(&mut Activity)>| {
            Button::new(id).xsmall().when(on, |b| b.primary()).when(!on, |b| b.ghost()).label(label).on_click(
                cx.listener(move |this, _, _, cx| {
                    set(&mut this.activity);
                    cx.notify();
                }),
            )
        };
        let shown = a.shown;
        let shown_chips = [(Shown::All, "All calls"), (Shown::Writes, "Writes"), (Shown::Problems, "Denied or failed")]
            .into_iter()
            .map(|(s, label)| {
                chip(format!("act-shown-{label}").into(), label.into(), shown == s, Box::new(move |a| a.shown = s))
            });
        let context_chips = std::iter::once((None, "All contexts".to_string()))
            .chain(a.contexts.iter().map(|c| (Some(c.clone()), c.clone())))
            .map(|(c, label)| {
                let on = a.context == c;
                chip(format!("act-ctx-{label}").into(), label, on, Box::new(move |a| a.context = c.clone()))
            });
        let agents = self.hub.config().agents;
        let agent_chips = (agents.len() > 1).then(|| {
            std::iter::once((None, "All agents".to_string()))
                .chain(agents.iter().map(|ag| (Some(ag.id.clone()), ag.name.clone())))
                .map(|(id, label)| {
                    let on = a.agent == id;
                    chip(format!("act-agent-{label}").into(), label, on, Box::new(move |a| a.agent = id.clone()))
                })
                .collect::<Vec<_>>()
        });
        let empty = a.items.len() <= 1 && a.items.iter().all(|i| matches!(i, Item::More { total: 0, .. }));
        let items = a.items.clone();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_4()
            .gap_2()
            .child(
                h_flex()
                    .gap_3()
                    .when(!self.advanced, |d| {
                        d.child(Button::new("act-back").ghost().small().icon(Lucide::ArrowLeft).label("Back").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.all_activity = false;
                                cx.notify();
                            }),
                        ))
                    })
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child("Agent activity")),
            )
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                "Every call, newest first, grouped by task (a session, or calls less than 10 minutes apart). \
                 Repeats on the same target fold into one line: click to expand. Also saved to ~/.local/share/recouvrance/audit.jsonl.",
            ))
            .children(audit_error)
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(shown_chips)
                    .child(div().w(px(220.)).ml_2().child(Input::new(&a.search).small())),
            )
            .child(h_flex().gap_1().flex_wrap().children(context_chips))
            .children(agent_chips.map(|c| h_flex().gap_1().flex_wrap().children(c)))
            .child(if empty {
                div().pt_4().text_sm().text_color(theme.muted_foreground).child("No calls match.").into_any_element()
            } else {
                list(a.list.clone(), move |ix, _, cx| match items.get(ix) {
                    Some(item) => render_item(item, ix, &view, cx),
                    None => div().into_any_element(),
                })
                .flex_1()
                .min_h_0()
                .into_any_element()
            })
            .into_any_element()
    }

    /// The latest few groups of one agent, for its page, and a way to the full log.
    pub(super) fn render_activity(&self, agent_id: Option<&str>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entries = self.hub.recent_audit(200, |e| agent_id.is_none_or(|id| e.agent_id == id));
        let audit_error = self
            .hub
            .audit_error()
            .map(|e| div().text_sm().text_color(theme.danger).child(format!("The audit log is not being saved: {e}")));
        let agent = agent_id.map(String::from);
        let see_all =
            Button::new("act-see-all").small().outline().icon(Lucide::Activity).label("See all activity").on_click(
                cx.listener(move |this, _, _, cx| {
                    this.activity.agent = agent.clone();
                    this.all_activity = true;
                    cx.notify();
                }),
            );
        if entries.is_empty() {
            return v_flex()
                .children(audit_error)
                .child(div().text_sm().text_color(theme.muted_foreground).child("No calls yet."))
                .into_any_element();
        }
        let groups: Vec<Group> = group_calls(entries).into_iter().take(3).collect();
        let view = cx.entity().downgrade();
        let lines = items(&groups, &self.activity.open);
        v_flex()
            .children(audit_error)
            .children(lines.iter().enumerate().map(|(ix, item)| render_item(item, ix, &view, cx)))
            .child(h_flex().pt_3().child(see_all))
            .into_any_element()
    }
}

/// `{"context":"x","kind":"pods","namespace":null}` -> `context=x kind=pods`
pub(super) fn compact_args(args: &serde_json::Value) -> String {
    match args.as_object() {
        Some(map) => map
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| match v.as_str() {
                Some(s) => format!("{k}={s}"),
                None => format!("{k}={v}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        None => args.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{AuditEntry, Decision, HashSet, Item, Shown, call_text, group_calls, items, keep, touched};

    fn call(secs: i64, tool: &str, args: serde_json::Value, decision: Decision) -> AuditEntry {
        AuditEntry {
            time: jiff::Timestamp::from_second(1_800_000_000 + secs).unwrap(),
            agent_id: "a".into(),
            agent: "claude".into(),
            tool: tool.into(),
            args,
            decision,
            detail: String::new(),
            duration_ms: 1,
            result_bytes: 0,
            session: None,
        }
    }

    fn exec(secs: i64, cmd: &str) -> AuditEntry {
        let args = serde_json::json!({"context": "prod", "namespace": "mq", "pod": "rabbit-2", "command": cmd.split(' ').collect::<Vec<_>>()});
        call(secs, "exec_command", args, Decision::Allowed)
    }

    #[test]
    fn pauses_split_groups_and_repeats_fold() {
        // Newest first: three execs on one pod, a log read, then an older task 2 hours before.
        let logs = serde_json::json!({"context": "prod", "pod": "rabbit-2"});
        let mut calls =
            vec![exec(7230, "rabbitmqctl status"), exec(7220, "rabbitmqctl list_queues"), exec(7210, "env")];
        calls.push(call(7200, "get_logs", logs, Decision::Allowed));
        calls.push(call(0, "list_contexts", serde_json::Value::Null, Decision::Allowed));
        let groups = group_calls(calls);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].runs.iter().map(Vec::len).collect::<Vec<_>>(), [3, 1]);
        assert_eq!(touched(&groups[0]), "prod · mq · rabbit-2");
    }

    #[test]
    fn a_denied_call_is_never_folded_into_allowed_ones() {
        let mut denied = exec(10, "rm -rf /data");
        denied.decision = Decision::Denied;
        let groups = group_calls(vec![exec(20, "ls"), denied, exec(0, "ls")]);
        assert_eq!(groups[0].runs.len(), 3);
    }

    #[test]
    fn exec_shows_its_command_and_filters_match() {
        let e = exec(0, "rabbitmqctl status");
        assert_eq!(call_text(&e, false), "rabbit-2 $ rabbitmqctl status");
        let logs = call(0, "get_logs", serde_json::json!({"context": "prod", "pod": "p"}), Decision::Allowed);
        assert_eq!(call_text(&logs, false), "pod=p");
        assert_eq!(call_text(&logs, true), "context=prod pod=p");
        assert!(keep(&e, Shown::Writes, Some("prod"), None, "rabbitmqctl"));
        assert!(!keep(&e, Shown::Problems, None, None, ""));
        assert!(!keep(&e, Shown::All, Some("staging"), None, ""));
        assert!(!keep(&e, Shown::All, None, Some("other-agent"), ""));
    }

    #[test]
    fn expanded_runs_list_their_calls() {
        let groups = group_calls(vec![exec(2, "a"), exec(1, "b")]);
        let folded = items(&groups, &HashSet::new());
        assert_eq!(folded.len(), 2, "header and one folded line");
        let Item::Run { key, .. } = &folded[1] else { panic!("a run") };
        let open: HashSet<String> = [key.clone()].into();
        assert_eq!(items(&groups, &open).len(), 4, "header, the run, its two calls");
    }
}
