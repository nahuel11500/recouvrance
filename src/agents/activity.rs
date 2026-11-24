//! The activity log of agent calls: grouped into tasks, repeats folded, filtered,
//! with older calls read back from the log on disk.

use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::AgentsView;
use crate::mcp::{AuditEntry, Decision, Hub};
use crate::ui::page_filter::PageFilter;

/// A pause longer than this starts a new group, even within one session and client.
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
    /// Calls that returned Secret values.
    Revealed,
}

/// What the search box asks for: `client:`, `tool:`, `ctx:`, `ns:` and `kind:` terms (part
/// of the value, ignoring case), `is:write` and `is:reveal`, the rest searched as text in the
/// tool, agent, client, arguments and result. Every term must match.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Search {
    client: Vec<String>,
    tool: Vec<String>,
    writes: bool,
    revealed: bool,
    /// The `ctx:`, `ns:` and `kind:` terms and the text, as the Timeline and Problems boxes
    /// read them.
    page: PageFilter,
}

impl Search {
    pub(super) fn parse(input: &str) -> Self {
        let mut s = Self::default();
        let mut rest = Vec::new();
        for word in input.split_whitespace() {
            let lower = word.to_lowercase();
            match lower.split_once(':') {
                Some(("client", v)) if !v.is_empty() => s.client.push(v.to_string()),
                Some(("tool", v)) if !v.is_empty() => s.tool.push(v.to_string()),
                Some(("is", "write" | "writes")) => s.writes = true,
                Some(("is", "reveal" | "revealed" | "secret" | "secrets")) => s.revealed = true,
                _ => rest.push(word),
            }
        }
        s.page = PageFilter::parse(&rest.join(" "));
        s
    }

    /// Whether a call matches every term.
    pub(super) fn matches(&self, e: &AuditEntry) -> bool {
        use crate::filter_syntax::contains_ci;
        // Every call of the log, on disk too, goes through here: nothing is allocated for a
        // term that isn't there.
        let client = e.client.as_deref().unwrap_or_default();
        let missed = self.client.iter().any(|c| !contains_ci(client, c))
            || self.tool.iter().any(|t| !contains_ci(&e.tool, t))
            || (self.writes && !writes(e))
            || (self.revealed && e.revealed.is_empty());
        if missed {
            return false;
        }
        if self.page.is_empty() {
            return true;
        }
        let text = if self.page.text.is_empty() {
            String::new()
        } else {
            let (args, revealed) = (e.args.to_string(), e.revealed.join(" "));
            [e.tool.as_str(), e.agent.as_str(), client, e.detail.as_str(), args.as_str(), revealed.as_str()].join(" ")
        };
        let context = arg(e, "context").unwrap_or_default();
        let kind = arg(e, "kind").unwrap_or_default();
        self.page.matches(&context, arg(e, "namespace").as_deref(), &kind, &text)
    }
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
            search: cx.new(|cx| InputState::new(window, cx).placeholder("Search, or client: tool: ns: is:write…")),
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

/// Calls of one agent with no pause longer than [`GAP_SECS`], in one session, from one client.
pub(super) struct Group {
    pub runs: Vec<Vec<AuditEntry>>,
}

impl Group {
    fn entries(&self) -> impl Iterator<Item = &AuditEntry> {
        self.runs.iter().flatten()
    }
}

/// Split calls (newest first) into groups, and each group into runs of the same
/// tool on the same target with the same outcome, all writes or none, all showing Secret
/// values or none: a line shows whether its newest call wrote or showed them, so a dry run
/// never joins the real call's run, nor a masked read one that showed the values.
pub(super) fn group_calls(entries: Vec<AuditEntry>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for e in entries {
        let prev = groups.last().and_then(|g| g.runs.last()).and_then(|r| r.last());
        // The client matters where no session tells calls apart: a stateless client sends
        // none, and your Claude Code and the assistant may share an agent.
        let same_group = prev.is_some_and(|p| {
            p.agent_id == e.agent_id
                && p.session == e.session
                && p.client == e.client
                && p.time.duration_since(e.time).as_secs() <= GAP_SECS
        });
        if !same_group {
            groups.push(Group { runs: vec![vec![e]] });
            continue;
        }
        let Some(g) = groups.last_mut() else { continue };
        let joins = g.runs.last().and_then(|r| r.last()).is_some_and(|p| {
            p.tool == e.tool
                && target(p) == target(&e)
                && problem(p) == problem(&e)
                && writes(p) == writes(&e)
                && p.revealed.is_empty() == e.revealed.is_empty()
        });
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

/// Whether a call may have changed something: what the Writes filter keeps, each task counts
/// and the tool's color shows.
fn writes(e: &AuditEntry) -> bool {
    crate::mcp::server::call_writes(&e.tool, &e.args)
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
            .map_or_default(|a| a.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), String::from)).collect());
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

/// Does a call pass the filters?
pub(super) fn keep(e: &AuditEntry, shown: Shown, context: Option<&str>, agent: Option<&str>, search: &Search) -> bool {
    let shown_ok = match shown {
        Shown::All => true,
        Shown::Writes => writes(e),
        Shown::Problems => problem(e),
        Shown::Revealed => !e.revealed.is_empty(),
    };
    shown_ok
        && agent.is_none_or(|a| e.agent_id == a)
        && context.is_none_or(|c| arg(e, "context").as_deref() == Some(c))
        && search.matches(e)
}

#[derive(Clone)]
pub(super) enum Item {
    Header {
        agent: String,
        client: Option<String>,
        session: Option<String>,
        first: jiff::Timestamp,
        last: jiff::Timestamp,
        calls: usize,
        writes: usize,
        problems: usize,
        /// Calls that returned Secret values.
        revealed: usize,
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
            client: newest.client.clone(),
            session: newest.session.clone(),
            first: oldest.time,
            last: newest.time,
            calls: all.len(),
            writes: all.iter().filter(|e| writes(e)).count(),
            problems: all.iter().filter(|e| problem(e)).count(),
            revealed: all.iter().filter(|e| !e.revealed.is_empty()).count(),
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
    t.to_zoned_datetime(jiff::tz::TimeZone::system()).strftime("%H:%M:%S").to_string()
}

/// "14:02:11", or "Oct 20 14:02" for another day.
fn fmt_day_time(t: jiff::Timestamp) -> String {
    let tz = jiff::tz::TimeZone::system();
    let z = t.to_zoned_datetime(tz.clone());
    if z.date() == jiff::Timestamp::now().to_zoned_datetime(tz).date() {
        z.strftime("%H:%M:%S").to_string()
    } else {
        z.strftime("%b %d %H:%M").to_string()
    }
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
            let oldest = calls.last().map_or(e.time, |c| c.time);
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
            .when(nested, Styled::pl_8)
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
                h_flex()
                    .w(px(130.))
                    .flex_shrink_0()
                    .gap_1()
                    .when(writes(e), |d| d.text_color(theme.warning))
                    .child(e.tool.clone())
                    // It returned Secret values: the detail says which.
                    .when(!e.revealed.is_empty(), |d| {
                        d.child(Icon::new(Lucide::Eye).size_3().text_color(theme.warning))
                    }),
            )
            .child(
                div()
                    .w(px(36.))
                    .flex_shrink_0()
                    .text_color(theme.blue)
                    .when(calls.len() > 1, |d| d.child(format!("×{}", calls.len()))),
            )
            // As the page narrows (the assistant dock open), the result beside the call gives up
            // room twice as readily as the call, which keeps at least 120px (#749).
            .child(
                div()
                    .flex_grow(1.)
                    .flex_basis(px(240.))
                    .min_w(px(120.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(call_text(e, with_context)),
            )
            .child(
                div()
                    .flex_basis(px(240.))
                    .flex_shrink(2.)
                    .min_w(px(80.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(muted)
                    .child(side),
            )
            .child(div().w(px(60.)).flex_shrink_0().text_color(muted).child(format!("{}ms", e.duration_ms)))
    };
    match item {
        Item::Header { agent, client, session, first, last, calls, writes, problems, revealed, touched } => h_flex()
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
            .when_some(client.clone(), |d, c| d.child(div().text_color(muted).child(c)))
            .when_some(session.clone(), |d, s| {
                d.child(div().text_color(muted).child(format!("session {}", s.chars().take(8).collect::<String>())))
            })
            .child(div().text_color(muted).child(format!("{} – {}", fmt_day_time(*first), fmt_time(*last))))
            .child(crate::plural::count(*calls, "call"))
            .when(*writes > 0, |d| {
                d.child(div().text_color(theme.warning).child(crate::plural::count(*writes, "write")))
            })
            .when(*problems > 0, |d| {
                d.child(div().text_color(theme.danger).child(format!("{problems} denied or failed")))
            })
            .when(*revealed > 0, |d| {
                d.child(div().text_color(theme.warning).child(format!("{revealed} showed Secret values")))
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
            .when_some(call.change.as_deref(), |d, change| d.child(change_block(change, cx)))
            .when(!call.revealed.is_empty(), |d| {
                d.child(
                    div().text_color(theme.warning).child(format!("Secret values shown: {}", call.revealed.join(", "))),
                )
            })
            .child(div().text_color(muted).child(format!(
                "{} · {}ms · {} bytes returned{}{}",
                call.time.to_zoned_datetime(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M:%S"),
                call.duration_ms,
                call.result_bytes,
                call.client.as_ref().map_or_default(|c| format!(" · {c}")),
                call.session.as_ref().map_or_default(|s| format!(" · session {s}"))
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
                .child(format!("Showing {shown} of {}", crate::plural::count(*total, "call")))
                .when(*loading, |d| d.child("Reading the log on disk…"))
                .when(!*loading && (shown < total || *can_load), |d| {
                    d.child(
                        Button::new("act-older")
                            .xsmall()
                            .outline()
                            .label(if shown < total { "Show older" } else { "Load older calls from disk" })
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, AgentsView::show_older);
                            }),
                    )
                })
                .into_any_element()
        }
    }
}

/// What a call was to change, as its approval showed it (`Preview::text`): added lines in
/// green, removed lines and warnings in red.
fn change_block(text: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    v_flex()
        .py_1()
        .child(div().text_color(theme.muted_foreground).child("What the approval showed:"))
        .children(text.lines().map(|line| {
            let color = if line.starts_with("  + ") {
                theme.success
            } else if line.starts_with("  - ") || line.starts_with("  ! ") {
                theme.danger
            } else {
                theme.foreground
            };
            div().text_color(color).child(line.to_string())
        }))
        .into_any_element()
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
        let stats = self.hub.audit_stats(|_| false);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (stats.newest.map(jiff::Timestamp::as_nanosecond), stats.oldest.map(jiff::Timestamp::as_nanosecond))
            .hash(&mut h);
        (shown, &context, &agent, &needle, a.limit, a.loading, a.disk.as_ref().map(|d| d.len())).hash(&mut h);
        let mut open: Vec<&String> = a.open.iter().collect();
        open.sort();
        open.hash(&mut h);
        let sig = h.finish();
        if sig == a.sig {
            return;
        }
        let search = Search::parse(&needle);
        let filter = |e: &AuditEntry| keep(e, shown, context.as_deref(), agent.as_deref(), &search);
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
            Button::new(id)
                .xsmall()
                .when(on, ButtonVariants::primary)
                .when(!on, ButtonVariants::ghost)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    set(&mut this.activity);
                    cx.notify();
                }))
        };
        let shown = a.shown;
        let shown_chips = [
            (Shown::All, "All calls"),
            (Shown::Writes, "Writes"),
            (Shown::Problems, "Denied or failed"),
            (Shown::Revealed, "Secret values shown"),
        ]
        .into_iter()
        .map(|(s, label)| {
            chip(format!("act-shown-{label}").into(), label.into(), shown == s, Box::new(move |a| a.shown = s))
        });
        let context_chips = std::iter::once((None, "All contexts".to_string()))
            .chain(a.contexts.iter().map(|c| (Some(c.clone()), c.clone())))
            .map(|(c, label)| {
                let on = a.context == c;
                chip(format!("act-ctx-{label}").into(), label, on, Box::new(move |a| a.context.clone_from(&c)))
            });
        let agents = self.hub.config().agents;
        let agent_chips = (agents.len() > 1).then(|| {
            std::iter::once((None, "All agents".to_string()))
                .chain(agents.iter().map(|ag| (Some(ag.id.clone()), ag.name.clone())))
                .map(|(id, label)| {
                    let on = a.agent == id;
                    chip(format!("act-agent-{label}").into(), label, on, Box::new(move |a| a.agent.clone_from(&id)))
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
            .child(div().text_sm().text_color(theme.muted_foreground).child(format!(
                "Every call, newest first, grouped by task (one client's session, or calls less than 10 minutes apart). \
                 Repeats on the same target fold into one line: click to expand. Search takes client:, tool:, ctx:, \
                 ns: and kind: terms, is:write and is:reveal. Also saved to {}.",
                crate::paths::shown_data("audit.jsonl")
            )))
            .children(audit_error)
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(shown_chips)
                    .child(div().w(px(280.)).ml_2().child(Input::new(&a.search).small())),
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
                    this.activity.agent.clone_from(&agent);
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
    use super::{AuditEntry, Decision, HashSet, Item, Search, Shown, call_text, group_calls, items, keep, touched};

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
            change: None,
            client: None,
            revealed: Vec::new(),
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

    /// A line takes its write color and text from its newest call, so a dry run never folds
    /// with the real call of the same tool on the same object. Dry runs still fold together.
    #[test]
    fn a_dry_run_is_never_folded_into_real_calls() {
        let set_image = |secs: i64, dry_run: Option<bool>| {
            let args = serde_json::json!({"context": "prod", "kind": "deployments", "namespace": "app", "name": "web",
                "container": "web", "image": "web:2", "dry_run": dry_run});
            call(secs, "set_image", args, Decision::Allowed)
        };
        // Newest first: the real call, then the two dry runs that came before it.
        let groups = group_calls(vec![set_image(30, None), set_image(20, Some(true)), set_image(10, Some(true))]);
        assert_eq!(groups[0].runs.iter().map(Vec::len).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn exec_shows_its_command_and_filters_match() {
        let e = exec(0, "rabbitmqctl status");
        assert_eq!(call_text(&e, false), "rabbit-2 $ rabbitmqctl status");
        let logs = call(0, "get_logs", serde_json::json!({"context": "prod", "pod": "p"}), Decision::Allowed);
        assert_eq!(call_text(&logs, false), "pod=p");
        assert_eq!(call_text(&logs, true), "context=prod pod=p");
        assert!(keep(&e, Shown::Writes, Some("prod"), None, &Search::parse("rabbitmqctl")));
        assert!(!keep(&e, Shown::Problems, None, None, &Search::default()));
        assert!(!keep(&e, Shown::All, Some("staging"), None, &Search::default()));
        assert!(!keep(&e, Shown::All, None, Some("other-agent"), &Search::default()));
    }

    /// Writes are the calls that may have changed something: `set_image` is one (a list of
    /// tools kept by hand missed it), a dry run or an Argo CD diagnosis is not (#843).
    #[test]
    fn writes_are_the_calls_that_may_have_changed_something() {
        let image = serde_json::json!({"context": "prod", "kind": "deployments", "namespace": "app", "name": "web",
            "container": "web", "image": "web:2", "dry_run": null});
        let apply = serde_json::json!({"context": "prod", "namespace": "app", "dry_run": true, "bytes": 120});
        let diagnose = serde_json::json!({"context": "prod", "kind": "applications", "namespace": "argocd",
            "name": "web", "action": "diagnose"});
        let calls = vec![
            call(30, "set_image", image, Decision::Allowed),
            call(20, "apply_manifest", apply, Decision::Allowed),
            call(10, "gitops", diagnose, Decision::Allowed),
        ];
        let kept: Vec<&str> = calls
            .iter()
            .filter(|e| keep(e, Shown::Writes, None, None, &Search::default()))
            .map(|e| e.tool.as_str())
            .collect();
        assert_eq!(kept, ["set_image"]);
        let lines = items(&group_calls(calls), &HashSet::new());
        let Some(Item::Header { calls: total, writes, .. }) = lines.first() else { panic!("a header first") };
        assert_eq!((*total, *writes), (3, 1));
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

    fn from(client: Option<&str>, mut e: AuditEntry) -> AuditEntry {
        e.client = client.map(String::from);
        e
    }

    /// Without a session (a stateless client), calls of one agent from two clients, your
    /// Claude Code and the assistant, are two tasks even when they interleave in time.
    #[test]
    fn calls_of_two_clients_are_two_tasks() {
        let code = Some("claude-code 2.1.3");
        let assistant = Some("assistant via claude-code 2.1.3");
        let groups =
            group_calls(vec![from(code, exec(30, "ls")), from(assistant, exec(20, "ls")), from(code, exec(10, "ls"))]);
        assert_eq!(groups.len(), 3);
        let lines = items(&group_calls(vec![from(code, exec(30, "ls")), from(code, exec(20, "env"))]), &HashSet::new());
        let Some(Item::Header { client, .. }) = lines.first() else { panic!("a header first") };
        assert_eq!(client.as_deref(), code);
    }

    /// A call that showed Secret values is its own line, counted in its task's header, and
    /// found by `is:reveal` and the "Secret values shown" filter.
    #[test]
    fn calls_that_showed_secret_values_stand_out() {
        let get = |secs: i64, revealed: &[&str]| {
            let args = serde_json::json!({"context": "prod", "kind": "secrets", "namespace": "shop", "name": "db"});
            let mut e = call(secs, "get_resource", args, Decision::Allowed);
            e.revealed = revealed.iter().map(|r| String::from(*r)).collect();
            e
        };
        let calls = vec![get(30, &["Secret shop/db"]), get(20, &[]), get(10, &[])];
        let groups = group_calls(calls.clone());
        assert_eq!(groups[0].runs.iter().map(Vec::len).collect::<Vec<_>>(), [1, 2]);
        let lines = items(&groups, &HashSet::new());
        let Some(Item::Header { revealed, .. }) = lines.first() else { panic!("a header first") };
        assert_eq!(*revealed, 1);
        for search in [Search::parse("is:reveal"), Search::parse("shop/db")] {
            let kept = calls.iter().filter(|e| keep(e, Shown::All, None, None, &search)).count();
            assert_eq!(kept, 1, "{search:?}");
        }
        assert_eq!(calls.iter().filter(|e| keep(e, Shown::Revealed, None, None, &Search::default())).count(), 1);
    }

    /// Each search term narrows the calls: `client:`, `tool:`, `ctx:`, `ns:`, `kind:` match
    /// part of their value, `is:write` keeps calls that may have changed something, and the
    /// other words are searched as text, the client included. Every term must match.
    #[test]
    fn search_terms_narrow_the_calls() {
        let scale = serde_json::json!({"context": "prod-eu", "kind": "deployments", "namespace": "shop", "name": "web", "replicas": 0});
        let calls = [
            from(Some("claude-code 2.1.3"), call(30, "scale", scale, Decision::Allowed)),
            from(Some("codex 0.40"), exec(20, "rabbitmqctl status")),
            call(10, "list_contexts", serde_json::Value::Null, Decision::Allowed),
        ];
        let tools = |input: &str| -> Vec<String> {
            let search = Search::parse(input);
            calls.iter().filter(|e| keep(e, Shown::All, None, None, &search)).map(|e| e.tool.clone()).collect()
        };
        assert_eq!(tools(""), ["scale", "exec_command", "list_contexts"]);
        assert_eq!(tools("client:claude"), ["scale"]);
        assert_eq!(tools("client:CODEX"), ["exec_command"]);
        assert_eq!(tools("tool:exec"), ["exec_command"]);
        assert_eq!(tools("ctx:prod"), ["scale", "exec_command"]);
        assert_eq!(tools("ns:shop"), ["scale"]);
        assert_eq!(tools("kind:deployment"), ["scale"]);
        assert_eq!(tools("is:write"), ["scale", "exec_command"]);
        assert_eq!(tools("is:write ns:mq"), ["exec_command"]);
        assert_eq!(tools("codex rabbitmqctl"), ["exec_command"]);
        assert_eq!(tools("codex web"), Vec::<&str>::new());
        // A term with nothing after its colon is text.
        assert_eq!(tools("client:"), Vec::<&str>::new());
        assert_eq!(Search::parse("tool:Scale web"), Search::parse("tool:scale WEB"));
    }
}
