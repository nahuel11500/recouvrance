//! Timeline page: the changes recorded while the app watched (see `crate::timeline`) on the
//! connected clusters, their events interleaved, newest first. The events are listed once,
//! then watched (`k8s::watch_events`). A change's diff is worked out when its row is picked.

use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt as _, StreamExt as _};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::{OpenObject, diff_block};
use crate::history::DiffLine;
use crate::k8s::{self, ClusterEvent, EventsMsg};
use crate::timeline::{self, Record, What};
use crate::ui::page_filter::PageFilter;
use crate::ui::page_header::{page_header, scope_text};

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
                let count = match (l.count, e.events) {
                    (_, n) if n > 1 => format!(" (×{}, {n} events)", l.count),
                    (c, _) if c > 1 => format!(" (×{c})"),
                    _ => String::new(),
                };
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

    /// The same row: the same change, or the same object's events of one type and reason, so
    /// that the row picked stays picked when newer events of it come.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Change(_, a), Self::Change(_, b)) => Arc::ptr_eq(a, b),
            (Self::Event(c, a), Self::Event(d, b)) => {
                c == d
                    && (&a.object_kind, &a.object_namespace, &a.object_name, &a.line.kind, &a.line.reason)
                        == (&b.object_kind, &b.object_namespace, &b.object_name, &b.line.kind, &b.line.reason)
            }
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

/// Whether a row is shown: on `context` (None: all), in one of `namespaces` (none chosen:
/// any, cluster-scoped objects too), with the events asked for, matching every term of the
/// filter.
fn shown(e: &Entry, f: &PageFilter, context: Option<&str>, namespaces: &[String], events: Events) -> bool {
    if context.is_some_and(|c| c != e.context()) {
        return false;
    }
    if !namespaces.is_empty() && !e.namespace().is_some_and(|ns| namespaces.iter().any(|n| n == ns)) {
        return false;
    }
    match events {
        Events::Hidden if e.is_event() => return false,
        Events::Warnings if e.is_event() && !e.warning() => return false,
        _ => {}
    }
    // The text is put together only for a filter that searches it: the rows are filtered
    // again each time the events come.
    let text = if f.text.is_empty() {
        String::new()
    } else {
        format!("{} {} {}/{} {}", e.context(), e.kind(), e.namespace().unwrap_or_default(), e.name(), e.summary())
    };
    f.matches(e.context(), e.namespace(), e.kind(), &text)
}

/// Changes and events in one list, newest first (no time last). Each source's events come
/// newest first, so the sort mostly merges them.
fn merged<'a>(parts: impl IntoIterator<Item = &'a [Entry]>) -> Vec<Entry> {
    let mut all: Vec<Entry> = parts.into_iter().flatten().cloned().collect();
    all.sort_by_key(|e| std::cmp::Reverse(e.time()));
    all
}

/// The events a cluster's namespace read on its own may hold: a cluster's share
/// (`k8s::EVENTS_KEPT`) split between its reads.
fn cap_per_read(reads: usize) -> usize {
    (k8s::EVENTS_KEPT / reads.max(1)).max(1)
}

/// The events of one cluster, in one namespace or all: what its watch sent last.
struct Source {
    context: Arc<str>,
    namespace: Option<String>,
    /// Its rows, newest first.
    rows: Vec<Entry>,
    /// How many events they are, of how many the cluster keeps there, and when the oldest of
    /// them last happened.
    kept: usize,
    total: usize,
    since: Option<jiff::Timestamp>,
    error: Option<String>,
    /// Waiting for its watch's first rows.
    loading: bool,
}

impl Source {
    fn new(context: Arc<str>, namespace: Option<String>, rows: Vec<Entry>) -> Self {
        Self { context, namespace, rows, kept: 0, total: 0, since: None, error: None, loading: true }
    }

    /// What a line about it names: its context, and its namespace when read on its own.
    fn place(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}/{ns}", self.context),
            None => self.context.to_string(),
        }
    }

    /// What its rows cover, when the cluster keeps more events there than are held.
    fn coverage(&self) -> Option<String> {
        (self.kept < self.total).then(|| {
            let since = self.since.map_or_default(|t| format!(", from the last {}", k8s::age(t)));
            format!("{}: the newest {} of {} events{since}", self.place(), self.kept, self.total)
        })
    }
}

pub struct TimelineView {
    /// The connected clusters: their records, and their events.
    clusters: Vec<(String, Client)>,
    /// Each cluster with the namespaces its events are read in (see `crate::ui::Scope`).
    scope: crate::ui::Scope,
    /// The namespaces picked (none: all): only their rows are shown.
    namespaces: Vec<String>,
    /// The connected contexts' records, read at `version` (see `timeline::version`).
    records: Vec<Entry>,
    version: u64,
    /// The events of each cluster, by namespace read, in the scope's order.
    sources: Vec<Source>,
    /// The sources' watches, and the task that takes what they send: replaced to stop them,
    /// so nothing from before lands.
    watches: Option<(Vec<k8s::TaskGuard>, Task<()>)>,
    /// Set while the page is off screen: the watches close until it is shown again.
    events_pause: k8s::Pause,
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
    _subs: Vec<Subscription>,
    _poll: Task<()>,
}

impl EventEmitter<OpenObject> for TimelineView {}

/// How often an open page looks for new records (cheap: a counter).
const POLL: Duration = Duration::from_secs(2);

impl TimelineView {
    pub fn new(scope: crate::ui::Scope, namespaces: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let clusters = scope.iter().map(|(ctx, client, _)| (ctx.clone(), client.clone())).collect();
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter: text, ns:shop, kind:ConfigMap, ctx:prod"));
        let subs = vec![cx.subscribe(&filter, |this: &mut Self, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.refilter(cx);
                cx.notify();
            }
        })];
        // While shown: new records every few seconds. Off screen, the events' watches close
        // until the page is shown again.
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                let off =
                    this.update(cx, |this, cx| this.events_pause.set(!crate::app::is_visible(cx.entity_id(), cx)));
                if off.is_err() || !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                let alive = this.update(cx, |this, cx| {
                    this.events_pause.set(false);
                    if timeline::version() != this.version {
                        this.read_records(cx);
                        cx.notify();
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
            scope,
            namespaces,
            records: Vec::new(),
            version: 0,
            sources: Vec::new(),
            watches: None,
            events_pause: k8s::Pause::default(),
            entries: Vec::new(),
            shown: Vec::new(),
            filter,
            context: None,
            events_shown: Events::Warnings,
            picked: None,
            diff: None,
            _subs: subs,
            _poll: poll,
        };
        this.read_records(cx);
        this.watch_events(false, cx);
        this
    }

    /// Read the records again and list the events again (F5), keeping the filters; the
    /// events shown stay until the new ones come.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.read_records(cx);
        self.watch_events(true, cx);
        cx.notify();
    }

    /// The records of the connected contexts, as the recorder has them now.
    fn read_records(&mut self, cx: &App) {
        self.version = timeline::version();
        let connected = |c: &str| self.clusters.iter().any(|(n, _)| n == c);
        self.records =
            timeline::all().into_iter().filter(|(c, _)| connected(c)).map(|(c, r)| Entry::Change(c, r)).collect();
        self.rebuild(cx);
    }

    /// Show the namespaces picked now: their rows, and their events watched instead.
    pub fn set_scope(&mut self, scope: crate::ui::Scope, namespaces: Vec<String>, cx: &mut Context<Self>) {
        self.clusters = scope.iter().map(|(ctx, client, _)| (ctx.clone(), client.clone())).collect();
        (self.scope, self.namespaces) = (scope, namespaces);
        if self.context.as_ref().is_some_and(|c| !self.clusters.iter().any(|(n, _)| n == c)) {
            self.context = None;
        }
        self.read_records(cx);
        self.watch_events(false, cx);
        cx.notify();
    }

    /// Watch each cluster's events in the namespaces of the scope: all at once, or each one
    /// (the ones picked, or those set as accessible for it). With `keep`, the rows shown stay
    /// until their watch sends its own; the events the choice leaves out go at once.
    fn watch_events(&mut self, keep: bool, cx: &mut Context<Self>) {
        // The watches from before stop, and their task with them: nothing of theirs lands.
        self.watches = None;
        let before = std::mem::take(&mut self.sources);
        if self.events_shown == Events::Hidden {
            self.rebuild(cx);
            return;
        }
        let normal = self.events_shown == Events::All;
        // The rows change as often as the overview pages refresh: each change costs a sort and
        // a filter of every row.
        let every = crate::settings::secs(crate::settings::get().overview_refresh);
        let (mut guards, mut streams) = (Vec::new(), Vec::new());
        for (ctx, client, namespaces) in &self.scope {
            let (context, cap): (Arc<str>, _) = (ctx.as_str().into(), cap_per_read(namespaces.len()));
            for ns in namespaces {
                let ix = self.sources.len();
                let rows = (before.iter())
                    .find(|s| *s.context == **ctx && s.namespace == *ns)
                    .filter(|_| keep)
                    .map_or_default(|s| s.rows.clone());
                self.sources.push(Source::new(context.clone(), ns.clone(), rows));
                let (guard, rx) =
                    k8s::watch_events(client.clone(), ns.clone(), normal, cap, every, self.events_pause.clone());
                guards.push(guard);
                streams.push(rx.map(move |msg| (ix, msg)));
            }
        }
        let mut sent = futures::stream::select_all(streams);
        let task = cx.spawn(async move |this, cx| {
            while let Some(first) = sent.next().await {
                // What already waits goes in the same update: one sort and filter of the rows.
                let mut batch = vec![first];
                while let Some(Some(msg)) = sent.next().now_or_never() {
                    batch.push(msg);
                }
                if this.update(cx, |this, cx| this.on_events(batch, cx)).is_err() {
                    break;
                }
            }
        });
        self.watches = Some((guards, task));
        self.rebuild(cx);
    }

    /// What the watches sent: each source's rows replace its own, its error is shown with them.
    fn on_events(&mut self, batch: Vec<(usize, EventsMsg)>, cx: &mut Context<Self>) {
        for (ix, msg) in batch {
            let Some(s) = self.sources.get_mut(ix) else { continue };
            match msg {
                EventsMsg::Rows(r) => {
                    let context = s.context.clone();
                    s.rows = r.rows.into_iter().map(|e| Entry::Event(context.clone(), e)).collect();
                    (s.kept, s.total, s.since, s.error, s.loading) = (r.kept, r.total, r.since, None, false);
                }
                EventsMsg::Error(e) => s.error = Some(e),
            }
        }
        self.rebuild(cx);
        cx.notify();
    }

    /// Some watch has neither sent its first rows nor failed yet.
    fn events_loading(&self) -> bool {
        self.sources.iter().any(|s| s.loading && s.error.is_none())
    }

    /// Show these events, with a watch of their own: Normal events are read only when shown.
    fn show_events(&mut self, v: Events, cx: &mut Context<Self>) {
        if self.events_shown != v {
            self.events_shown = v;
            self.watch_events(true, cx);
            cx.notify();
        }
    }

    /// Show these events (`all`, `warnings`, `none`), as their button does, in dev runs.
    #[cfg(feature = "dev")]
    pub fn demo_events(&mut self, which: &str, cx: &mut Context<Self>) {
        let v = match which {
            "all" => Events::All,
            "none" => Events::Hidden,
            _ => Events::Warnings,
        };
        self.show_events(v, cx);
    }

    fn rebuild(&mut self, cx: &App) {
        #[cfg(feature = "dev")]
        let _t = crate::dev::timing::span("timeline rebuild (ui)");
        let events = self.sources.iter().map(|s| s.rows.as_slice());
        self.entries = merged(std::iter::once(self.records.as_slice()).chain(events));
        // The row picked stays picked while it is there, as it is now.
        if let Some(p) = self.picked.take() {
            match self.entries.iter().find(|e| e.same(&p)) {
                Some(e) => self.picked = Some(e.clone()),
                None => self.diff = None,
            }
        }
        self.refilter(cx);
    }

    fn refilter(&mut self, cx: &App) {
        #[cfg(feature = "dev")]
        let _t = crate::dev::timing::span("timeline filter (ui)");
        let f = PageFilter::parse(&self.filter.read(cx).value());
        let (context, namespaces, events) = (self.context.as_deref(), &self.namespaces, self.events_shown);
        self.shown =
            (0..self.entries.len()).filter(|&i| shown(&self.entries[i], &f, context, namespaces, events)).collect();
    }

    fn pick(&mut self, e: Entry, cx: &mut Context<Self>) {
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
        // The row itself, not its place: the rows can change between the frame and the click.
        let clicked = e.clone();
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
            .on_click(cx.listener(move |this, _, _, cx| this.pick(clicked.clone(), cx)))
            .child(cell(56.).text_sm().text_color(muted).child(e.time().map_or_default(k8s::age)))
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
        let when = e.time().map_or_default(|t| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string());
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
                        e.namespace().map_or_default(|ns| format!("{ns}/")),
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
        let what = if recording {
            "changes recorded while the app watched, and events"
        } else {
            "recording is off (Settings → Timeline): what was recorded, and events"
        };
        let subtitle = format!("{what}, {}", scope_text(&self.namespaces));
        let header = page_header(Lucide::ClockArrowDown, None, "Timeline", subtitle, cx).child(div().flex_1()).child(
            Button::new("timeline-refresh")
                .small()
                .ghost()
                .icon(Lucide::RefreshCw)
                .label(if self.events_loading() { "Reading events…" } else { "Refresh" })
                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
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
                            .on_click(cx.listener(move |this, _, _, cx| this.show_events(v, cx))),
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
        // Events that could not be read, by cluster (the others are shown), and what is shown
        // where a cluster keeps more events than are held.
        let errors = (self.sources.iter())
            .filter_map(|s| s.error.as_ref().map(|e| format!("{}: events: {e}", s.place())))
            .map(|e| div().text_xs().text_color(theme.danger).child(e));
        let coverage =
            self.sources.iter().filter_map(Source::coverage).map(|c| div().text_xs().text_color(muted).child(c));
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(header)
            .child(controls)
            .children(problems.into_iter().map(|p| div().text_xs().text_color(theme.danger).child(p)))
            .children(errors)
            .children(coverage)
            .when(empty, |d| d.child(div().text_sm().text_color(muted).child(empty_text)))
            .child(
                // Only the rows on screen are laid out: a long timeline stays smooth.
                uniform_list(
                    "timeline-list",
                    self.shown.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range.filter_map(|i| this.shown.get(i).map(|&ix| this.row(i, ix, cx))).collect()
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

    use super::{Entry, Events, Source, cap_per_read, merged, shown};
    use crate::k8s::{ClusterEvent, EventLine};
    use crate::timeline::{Record, What};
    use crate::ui::page_filter::PageFilter as Filter;

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
                    first: None,
                    last: Some(jiff::Timestamp::from_second(1_700_000_000 + secs).unwrap()),
                    kind: kind.into(),
                    reason: "BackOff".into(),
                    message: "restarting".into(),
                    count: 3,
                    source: String::new(),
                },
                object_kind: "Pod".into(),
                object_namespace: Some("shop".into()),
                object_name: "web-1".into(),
                events: 1,
            }),
        )
    }

    #[test]
    fn changes_and_events_newest_first() {
        let records = [change("a", "ConfigMap", "shop", "web", 10), change("b", "Service", "shop", "api", 30)];
        let events = [event("a", "Warning", 20)];
        let all = merged([&records[..], &events[..]]);
        let names: Vec<&str> = all.iter().map(Entry::name).collect();
        assert_eq!(names, ["api", "web-1", "web"]);
        // Each cluster's events come newest first: merged with the others, ties in the order
        // they came.
        let (a, b) = ([event("a", "Warning", 40), event("a", "Normal", 5)], [event("b", "Warning", 40)]);
        let all = merged([&records[..], &a[..], &b[..]]);
        let order: Vec<(&str, Option<i64>)> =
            all.iter().map(|e| (e.context(), e.time().map(|t| t.as_second() - 1_700_000_000))).collect();
        assert_eq!(order, [("a", Some(40)), ("b", Some(40)), ("b", Some(30)), ("a", Some(10)), ("a", Some(5))]);
    }

    /// A newer read of an object's events of one type and reason is the same row: picked, it
    /// stays picked. A change is the one recorded.
    #[test]
    fn the_row_picked_is_found_in_a_newer_read() {
        let (old, new) = (event("prod", "Warning", 0), event("prod", "Warning", 60));
        assert!(old.same(&new));
        assert!(!old.same(&event("staging", "Warning", 0)), "another cluster");
        assert!(!old.same(&event("prod", "Normal", 0)), "another type");
        let c = change("prod", "ConfigMap", "shop", "web", 0);
        assert!(c.same(&c.clone()) && !c.same(&change("prod", "ConfigMap", "shop", "web", 0)));
    }

    /// A cluster's namespaces read one by one share what it may hold, and none holds nothing.
    #[test]
    fn a_cluster_s_reads_share_what_it_holds() {
        assert_eq!(cap_per_read(1), crate::k8s::EVENTS_KEPT);
        assert_eq!(cap_per_read(0), crate::k8s::EVENTS_KEPT);
        for reads in 1..=300 {
            assert!(reads * cap_per_read(reads) <= crate::k8s::EVENTS_KEPT.max(reads));
            assert!(cap_per_read(reads) >= 1);
        }
        assert_eq!(cap_per_read(usize::MAX), 1);
    }

    /// Fewer events held than the cluster keeps: the page says how many of how many, and how
    /// far back they go. All of them: nothing to say.
    #[test]
    fn what_the_events_cover() {
        let mut s = Source::new("prod".into(), None, Vec::new());
        (s.kept, s.total) = (3, 3);
        assert_eq!(s.coverage(), None);
        (s.kept, s.total, s.since) = (2, 5, Some(jiff::Timestamp::now() - jiff::SignedDuration::from_secs(600)));
        assert_eq!(s.coverage().as_deref(), Some("prod: the newest 2 of 5 events, from the last 10m"));
        s.namespace = Some("shop".into());
        s.since = None;
        assert_eq!(s.coverage().as_deref(), Some("prod/shop: the newest 2 of 5 events"));
    }

    #[test]
    fn filters() {
        let e = change("prod", "ConfigMap", "shop", "web", 0);
        let ok = |f: &str, ctx: Option<&str>| shown(&e, &Filter::parse(f), ctx, &[], Events::All);
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
        assert!(!shown(&normal, &f, None, &[], Events::Warnings) && shown(&warning, &f, None, &[], Events::Warnings));
        assert!(!shown(&warning, &f, None, &[], Events::Hidden));
        assert!(shown(&e, &f, None, &[], Events::Hidden), "changes stay when events are hidden");
        assert_eq!(warning.summary(), "Warning BackOff: restarting (×3)");
    }

    #[test]
    fn only_the_namespaces_picked() {
        let f = Filter::default();
        let ns = |l: &[&str]| l.iter().map(ToString::to_string).collect::<Vec<_>>();
        let shop = change("prod", "ConfigMap", "shop", "web", 0);
        let shopping = change("prod", "ConfigMap", "shopping", "web", 0);
        let mut node = change("prod", "Node", "", "n1", 0);
        if let Entry::Change(_, r) = &mut node {
            Arc::make_mut(r).namespace = None;
        }
        for e in [&shop, &shopping, &node] {
            assert!(shown(e, &f, None, &[], Events::All), "no namespace picked: every row");
        }
        assert!(shown(&shop, &f, None, &ns(&["shop"]), Events::All));
        assert!(!shown(&shopping, &f, None, &ns(&["shop"]), Events::All), "the name exactly");
        assert!(!shown(&node, &f, None, &ns(&["shop"]), Events::All), "cluster-scoped rows are left out");
        assert!(shown(&shopping, &f, None, &ns(&["shop", "shopping"]), Events::All));
        // An event is placed by its object's namespace.
        assert!(shown(&event("prod", "Warning", 0), &f, None, &ns(&["shop"]), Events::All));
    }
}
