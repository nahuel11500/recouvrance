//! Timeline page: the changes recorded while the app watched (see `crate::timeline`) on the
//! connected clusters, their events interleaved, newest first. A change's diff is worked
//! out when its row is picked.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::{OpenObject, diff_block};
use crate::history::DiffLine;
use crate::k8s::{self, ClusterEvent};
use crate::timeline::{self, Record, What};

/// One row: a change recorded, or an event, with its context.
#[derive(Clone)]
enum Entry {
    Change(Arc<str>, Arc<Record>),
    Event(Arc<str>, Arc<ClusterEvent>),
}

impl Entry {
    fn context(&self) -> &str {
        match self {
            Self::Change(c, _) | Self::Event(c, _) => c,
        }
    }

    fn time(&self) -> Option<jiff::Timestamp> {
        match self {
            Self::Change(_, r) => Some(r.time),
            Self::Event(_, e) => e.line.last,
        }
    }

    fn kind(&self) -> &str {
        match self {
            Self::Change(_, r) => &r.kind,
            Self::Event(_, e) => &e.object_kind,
        }
    }

    fn namespace(&self) -> Option<&str> {
        match self {
            Self::Change(_, r) => r.namespace.as_deref(),
            Self::Event(_, e) => e.object_namespace.as_deref(),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Change(_, r) => &r.name,
            Self::Event(_, e) => &e.object_name,
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Change(_, r) => r.summary.clone(),
            Self::Event(_, e) => {
                let l = &e.line;
                let count = if l.count > 1 { format!(" (×{})", l.count) } else { String::new() };
                format!("{} {}: {}{count}", l.kind, l.reason, l.message)
            }
        }
    }

    fn is_event(&self) -> bool {
        matches!(self, Self::Event(..))
    }

    /// A Warning event.
    fn warning(&self) -> bool {
        matches!(self, Self::Event(_, e) if e.line.kind == "Warning")
    }

    /// The same row (a list rebuilt keeps the one picked).
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Change(_, a), Self::Change(_, b)) => Arc::ptr_eq(a, b),
            (Self::Event(_, a), Self::Event(_, b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// What opening the row opens: the object, by kubectl's name for its kind.
    fn open(&self) -> OpenObject {
        let kind = match self {
            Self::Change(_, r) => r.resource.clone(),
            Self::Event(_, e) => e.object_kind.clone(),
        };
        OpenObject {
            kind,
            name: self.name().to_string(),
            namespace: self.namespace().map(String::from),
            context: Some(self.context().to_string()),
        }
    }
}

/// Which events are shown with the changes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Events {
    All,
    Warnings,
    Hidden,
}

/// The filter as typed: `ctx:`, `ns:` and `kind:` terms, the rest searched as text. Each
/// term must match (lowercase).
#[derive(Default, Debug, PartialEq)]
struct Filter {
    context: Vec<String>,
    namespace: Vec<String>,
    kind: Vec<String>,
    text: Vec<String>,
}

fn parse_filter(input: &str) -> Filter {
    let mut f = Filter::default();
    for word in input.split_whitespace() {
        let word = word.to_lowercase();
        let term = match word.split_once(':') {
            Some(("ctx" | "context", v)) if !v.is_empty() => Some((&mut f.context, v.to_string())),
            Some(("ns" | "namespace", v)) if !v.is_empty() => Some((&mut f.namespace, v.to_string())),
            Some(("kind", v)) if !v.is_empty() => Some((&mut f.kind, v.to_string())),
            _ => None,
        };
        match term {
            Some((terms, v)) => terms.push(v),
            None => f.text.push(word),
        }
    }
    f
}

/// Whether a row is shown: on `context` (None: all), with the events asked for, matching
/// every term of the filter.
fn shown(e: &Entry, f: &Filter, context: Option<&str>, events: Events) -> bool {
    if context.is_some_and(|c| c != e.context()) {
        return false;
    }
    match events {
        Events::Hidden if e.is_event() => return false,
        Events::Warnings if e.is_event() && !e.warning() => return false,
        _ => {}
    }
    let kind = e.kind().to_lowercase();
    // `kind:configmap`, `kind:configmaps`, `kind:config`.
    let kind_ok = |k: &String| kind.contains(k.as_str()) || format!("{kind}s") == *k;
    let ns = e.namespace().unwrap_or_default().to_lowercase();
    if !(f.context.iter().all(|c| e.context().to_lowercase().contains(c.as_str()))
        && f.namespace.iter().all(|n| ns.contains(n.as_str()))
        && f.kind.iter().all(kind_ok))
    {
        return false;
    }
    if f.text.is_empty() {
        return true;
    }
    let text = format!("{} {kind} {ns}/{} {}", e.context(), e.name(), e.summary()).to_lowercase();
    f.text.iter().all(|t| text.contains(t.as_str()))
}

/// Changes and events in one list, newest first (no time last).
fn merged(records: &[Entry], events: &[Entry]) -> Vec<Entry> {
    let mut all: Vec<Entry> = records.iter().chain(events).cloned().collect();
    all.sort_by_key(|e| std::cmp::Reverse(e.time()));
    all
}

pub struct TimelineView {
    /// The connected clusters: their records, and their events.
    clusters: Vec<(String, Client)>,
    /// The connected contexts' records, read at `version` (see `timeline::version`).
    records: Vec<Entry>,
    version: u64,
    events: Vec<Entry>,
    /// Events that could not be read, by cluster (the others are shown).
    event_errors: Vec<String>,
    events_loading: bool,
    /// When the events were last read.
    events_read: Option<Instant>,
    /// Every row, newest first, and the ones the filters leave.
    entries: Vec<Entry>,
    shown: Vec<usize>,
    filter: Entity<InputState>,
    /// One context, or all.
    context: Option<String>,
    events_shown: Events,
    /// The row picked, and its diff (worked out when picked).
    picked: Option<Entry>,
    diff: Option<Arc<Vec<DiffLine>>>,
    load_events: crate::latest::Latest,
    _subs: Vec<Subscription>,
    _poll: Task<()>,
}

impl EventEmitter<OpenObject> for TimelineView {}

/// How often an open page looks for new records (cheap: a counter).
const POLL: Duration = Duration::from_secs(2);

impl TimelineView {
    pub fn new(clusters: Vec<(String, Client)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter: text, ns:shop, kind:ConfigMap, ctx:prod"));
        let subs = vec![cx.subscribe(&filter, |this: &mut Self, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.refilter(cx);
                cx.notify();
            }
        })];
        // While shown: new records every few seconds, events again every overview refresh.
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                let alive = this.update(cx, |this, cx| {
                    if timeline::version() != this.version {
                        this.read_records(cx);
                        cx.notify();
                    }
                    let every = crate::settings::secs(crate::settings::get().overview_refresh);
                    if !this.events_loading && this.events_read.is_none_or(|t| t.elapsed() >= every) {
                        this.read_events(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
                exec.timer(POLL).await;
            }
        });
        let mut this = Self {
            clusters,
            records: Vec::new(),
            version: 0,
            events: Vec::new(),
            event_errors: Vec::new(),
            events_loading: false,
            events_read: None,
            entries: Vec::new(),
            shown: Vec::new(),
            filter,
            context: None,
            events_shown: Events::Warnings,
            picked: None,
            diff: None,
            load_events: Default::default(),
            _subs: subs,
            _poll: poll,
        };
        this.read_records(cx);
        this.read_events(cx);
        this
    }

    /// The records of the connected contexts, as the recorder has them now.
    fn read_records(&mut self, cx: &App) {
        self.version = timeline::version();
        let connected = |c: &str| self.clusters.iter().any(|(n, _)| n == c);
        self.records =
            timeline::all().into_iter().filter(|(c, _)| connected(c)).map(|(c, r)| Entry::Change(c, r)).collect();
        self.rebuild(cx);
    }

    /// Each cluster's events: every namespace, or each one set as accessible for it.
    fn read_events(&mut self, cx: &mut Context<Self>) {
        if self.events_shown == Events::Hidden {
            self.events.clear();
            self.event_errors.clear();
            self.rebuild(cx);
            return;
        }
        self.events_loading = true;
        let normal = self.events_shown == Events::All;
        let reads: Vec<(String, Client, Option<String>)> = (self.clusters.iter())
            .flat_map(|(ctx, client)| {
                let accessible = crate::settings::cluster(ctx).namespaces;
                let scopes: Vec<Option<String>> =
                    if accessible.is_empty() { vec![None] } else { accessible.into_iter().map(Some).collect() };
                scopes.into_iter().map(move |ns| (ctx.clone(), client.clone(), ns))
            })
            .collect();
        let work = futures::future::join_all(reads.into_iter().map(|(ctx, client, ns)| async move {
            let read = k8s::recent_events(client, ns.clone(), normal).await;
            (ctx, ns, read)
        }));
        // A new read (events shown changed, a refresh) replaces the one running.
        self.load_events.spawn(cx, work, |this, results, cx| {
            this.events_loading = false;
            this.events_read = Some(Instant::now());
            this.events.clear();
            this.event_errors.clear();
            for (ctx, ns, read) in results {
                match read {
                    Ok(events) => {
                        let c: Arc<str> = ctx.into();
                        this.events.extend(events.into_iter().map(|e| Entry::Event(c.clone(), Arc::new(e))));
                    }
                    Err(e) => {
                        let place = ns.map(|ns| format!("{ctx}/{ns}")).unwrap_or(ctx);
                        this.event_errors.push(format!("{place}: events: {}", k8s::short_error(&e)));
                    }
                }
            }
            this.rebuild(cx);
            cx.notify();
        });
    }

    fn rebuild(&mut self, cx: &App) {
        self.entries = merged(&self.records, &self.events);
        // The row picked stays picked if it is still there.
        if let Some(p) = &self.picked
            && !self.entries.iter().any(|e| e.same(p))
        {
            self.picked = None;
            self.diff = None;
        }
        self.refilter(cx);
    }

    fn refilter(&mut self, cx: &App) {
        let f = parse_filter(&self.filter.read(cx).value());
        let (context, events) = (self.context.as_deref(), self.events_shown);
        self.shown = (0..self.entries.len()).filter(|&i| shown(&self.entries[i], &f, context, events)).collect();
    }

    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(e) = self.entries.get(ix).cloned() else { return };
        if self.picked.as_ref().is_some_and(|p| p.same(&e)) {
            self.picked = None;
            self.diff = None;
        } else {
            // Worked out now, once: never for rows only scrolled past.
            self.diff = match &e {
                Entry::Change(_, r) => r.diff().map(|d| Arc::new(d.lines())),
                Entry::Event(..) => None,
            };
            self.picked = Some(e);
        }
        cx.notify();
    }

    fn row(&self, i: usize, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let multi = self.clusters.len() > 1;
        let e = &self.entries[ix];
        let picked = self.picked.as_ref().is_some_and(|p| p.same(e));
        let source = match e {
            Entry::Change(_, r) => match r.what {
                What::Created => "Created",
                What::Updated => "Changed",
                What::Deleted => "Deleted",
            },
            Entry::Event(..) => "Event",
        };
        let cell = |w: f32| div().w(px(w)).flex_shrink_0().overflow_hidden().text_ellipsis().whitespace_nowrap();
        let place = match e.namespace() {
            Some(ns) => format!("{ns}/{}", e.name()),
            None => e.name().to_string(),
        };
        h_flex()
            .id(("timeline", i))
            .w_full()
            .gap_3()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .when(picked, |d| d.bg(theme.blue.opacity(0.12)))
            .on_click(cx.listener(move |this, _, _, cx| this.pick(ix, cx)))
            .child(cell(56.).text_sm().text_color(muted).child(e.time().map(k8s::age).unwrap_or_default()))
            .when(multi, |d| d.child(cell(140.).text_sm().text_color(muted).child(e.context().to_string())))
            .child(cell(64.).text_xs().text_color(if e.is_event() { muted } else { theme.blue }).child(source))
            .child(cell(130.).text_xs().text_color(muted).child(e.kind().to_string()))
            .child(cell(260.).text_sm().child(place))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .when(e.warning(), |d| d.text_color(theme.warning))
                    .child(e.summary()),
            )
            .into_any_element()
    }

    /// The row picked: all it says, its diff, and a way to its object.
    fn detail(&self, e: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let when = e.time().map(|t| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string()).unwrap_or_default();
        let connected = self.clusters.iter().any(|(c, _)| c == e.context());
        let deleted = matches!(e, Entry::Change(_, r) if r.what == What::Deleted);
        let open = e.open();
        let body = match (e, &self.diff) {
            (Entry::Event(..), _) => div().into_any_element(),
            (_, Some(lines)) if lines.is_empty() => {
                div().text_xs().text_color(muted).child("No difference.").into_any_element()
            }
            (_, Some(lines)) => diff_block(lines, cx),
            (_, None) => div()
                .text_xs()
                .text_color(muted)
                .child(
                    "No diff kept: too big, or the kind is recorded from its lists, which compare labels and \
                     annotations (Settings → Timeline records whole objects of the kinds named there).",
                )
                .into_any_element(),
        };
        v_flex()
            .id("timeline-detail")
            .max_h(px(360.))
            .overflow_y_scroll()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_3()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(format!(
                        "{} {}{}",
                        e.kind(),
                        e.namespace().map(|ns| format!("{ns}/")).unwrap_or_default(),
                        e.name()
                    )))
                    .child(div().text_xs().text_color(muted).child(format!("{} · {when}", e.context())))
                    .child(div().flex_1())
                    .when(connected && !deleted, |d| {
                        d.child(Button::new("timeline-open").xsmall().ghost().label("Open").on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.emit(open.clone());
                            },
                        )))
                    }),
            )
            .child(div().text_sm().when(e.warning(), |d| d.text_color(theme.warning)).child(e.summary()))
            .child(body)
            .into_any_element()
    }
}

impl Render for TimelineView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let recording = crate::settings::with(|s| s.timeline.record);
        let choice = |id: &'static str, on: bool| {
            let b = Button::new(id).xsmall();
            if on { b.primary() } else { b.ghost() }
        };
        let header = h_flex()
            .gap_3()
            .child(Icon::new(Lucide::ClockArrowDown).size_5())
            .child(div().text_xl().font_weight(FontWeight::MEDIUM).child("Timeline"))
            .child(div().text_sm().text_color(muted).child(if recording {
                "changes recorded while the app watched, and events"
            } else {
                "recording is off (Settings → Timeline): what was recorded, and events"
            }))
            .child(div().flex_1())
            .child(
                Button::new("timeline-refresh")
                    .small()
                    .ghost()
                    .icon(Lucide::RefreshCw)
                    .label(if self.events_loading { "Reading events…" } else { "Refresh" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.read_records(cx);
                        this.read_events(cx);
                        cx.notify();
                    })),
            );
        let mut contexts = h_flex().gap_1();
        if self.clusters.len() > 1 {
            contexts =
                contexts.child(choice("timeline-ctx-all", self.context.is_none()).label("All clusters").on_click(
                    cx.listener(|this, _, _, cx| {
                        this.context = None;
                        this.refilter(cx);
                        cx.notify();
                    }),
                ));
            for (i, (ctx, _)) in self.clusters.iter().enumerate() {
                let c = ctx.clone();
                let on = self.context.as_deref() == Some(ctx.as_str());
                contexts = contexts.child(
                    Button::new(("timeline-ctx", i))
                        .xsmall()
                        .when(on, ButtonVariants::primary)
                        .when(!on, ButtonVariants::ghost)
                        .label(ctx.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.context = Some(c.clone());
                            this.refilter(cx);
                            cx.notify();
                        })),
                );
            }
        }
        let events_choice =
            [(Events::All, "All events"), (Events::Warnings, "Warnings only"), (Events::Hidden, "No events")]
                .into_iter()
                .enumerate()
                .fold(h_flex().gap_1(), |row, (i, (v, label))| {
                    row.child(
                        Button::new(("timeline-events", i))
                            .xsmall()
                            .when(v == self.events_shown, ButtonVariants::primary)
                            .when(v != self.events_shown, ButtonVariants::ghost)
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.events_shown != v {
                                    // Normal events are read only when shown.
                                    let read =
                                        v == Events::All || this.events_shown == Events::Hidden || v == Events::Hidden;
                                    this.events_shown = v;
                                    if read {
                                        this.read_events(cx);
                                    }
                                    this.refilter(cx);
                                    cx.notify();
                                }
                            })),
                    )
                });
        let controls = h_flex()
            .gap_3()
            .flex_wrap()
            .child(div().w(px(380.)).child(Input::new(&self.filter).small().cleanable(true)))
            .child(contexts)
            .child(events_choice);
        let problems = timeline::problems();
        let empty = self.shown.is_empty();
        let empty_text = if self.entries.is_empty() {
            "Nothing recorded yet. Changes are recorded while a list or the Problems scan watches a kind \
             (pods, events and ReplicaSets excepted); Settings → Timeline adds kinds to watch whole."
        } else {
            "Nothing matches the filters."
        };
        let picked = self.picked.clone();
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(header)
            .child(controls)
            .children(problems.into_iter().map(|p| div().text_xs().text_color(theme.danger).child(p)))
            .children(self.event_errors.iter().map(|e| div().text_xs().text_color(theme.danger).child(e.clone())))
            .when(empty, |d| d.child(div().text_sm().text_color(muted).child(empty_text)))
            .child(
                // Only the rows on screen are laid out: a long timeline stays smooth.
                uniform_list(
                    "timeline-list",
                    self.shown.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let shown = this.shown.clone();
                        range.filter_map(|i| shown.get(i).map(|&ix| this.row(i, ix, cx))).collect()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .when_some(picked, |d, e| d.child(self.detail(&e, cx)))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's `test` attribute would come with it.
    use std::sync::Arc;

    use super::{Entry, Events, Filter, merged, parse_filter, shown};
    use crate::k8s::{ClusterEvent, EventLine};
    use crate::timeline::{Record, What};

    fn change(ctx: &str, kind: &str, ns: &str, name: &str, secs: i64) -> Entry {
        Entry::Change(
            ctx.into(),
            Arc::new(Record {
                time: jiff::Timestamp::from_second(1_700_000_000 + secs).unwrap(),
                resource: format!("{}s", kind.to_lowercase()),
                kind: kind.into(),
                namespace: Some(ns.into()),
                name: name.into(),
                uid: "u".into(),
                what: What::Updated,
                summary: "changed data.a".into(),
                before: None,
                after: None,
            }),
        )
    }

    fn event(ctx: &str, kind: &str, secs: i64) -> Entry {
        Entry::Event(
            ctx.into(),
            Arc::new(ClusterEvent {
                line: EventLine {
                    namespace: Some("shop".into()),
                    name: "e".into(),
                    age: String::new(),
                    last: Some(jiff::Timestamp::from_second(1_700_000_000 + secs).unwrap()),
                    kind: kind.into(),
                    reason: "BackOff".into(),
                    message: "restarting".into(),
                    count: 3,
                },
                object_kind: "Pod".into(),
                object_namespace: Some("shop".into()),
                object_name: "web-1".into(),
            }),
        )
    }

    #[test]
    fn changes_and_events_newest_first() {
        let records = [change("a", "ConfigMap", "shop", "web", 10), change("b", "Service", "shop", "api", 30)];
        let events = [event("a", "Warning", 20)];
        let all = merged(&records, &events);
        let names: Vec<&str> = all.iter().map(Entry::name).collect();
        assert_eq!(names, ["api", "web-1", "web"]);
    }

    #[test]
    fn filters() {
        let e = change("prod", "ConfigMap", "shop", "web", 0);
        let ok = |f: &str, ctx: Option<&str>| shown(&e, &parse_filter(f), ctx, Events::All);
        assert!(ok("", None));
        assert!(ok("kind:configmap ns:shop ctx:prod", None));
        assert!(ok("kind:configmaps data.a", None));
        assert!(!ok("kind:service", None));
        assert!(!ok("ns:other", None));
        assert!(!ok("", Some("staging")));
        assert!(ok("WEB", Some("prod")));
        let normal = event("prod", "Normal", 0);
        let warning = event("prod", "Warning", 0);
        let f = Filter::default();
        assert!(!shown(&normal, &f, None, Events::Warnings) && shown(&warning, &f, None, Events::Warnings));
        assert!(!shown(&warning, &f, None, Events::Hidden));
        assert!(shown(&e, &f, None, Events::Hidden), "changes stay when events are hidden");
        assert_eq!(warning.summary(), "Warning BackOff: restarting (×3)");
    }
}
