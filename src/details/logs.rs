//! Log viewer: merged streams of one or more pods, filters, find, histogram.

#[allow(unused_imports)]
use super::*;

mod model;
use model::*;

pub(super) const MAX_LOG_LINES: usize = 50_000;

/// Distinct colors for pod prefixes in multi-pod logs.
pub(super) fn pod_color(ix: usize, cx: &App) -> Hsla {
    let t = cx.theme();
    [t.blue, t.success, t.warning, t.danger, hsla(0.78, 0.6, 0.6, 1.), hsla(0.5, 0.6, 0.45, 1.)][ix % 6]
}

/// One row's height in a uniform list whose content is `rows` rows of `contents` in all.
fn row_height(contents: Pixels, rows: usize) -> Option<Pixels> {
    (rows > 0 && contents > px(0.)).then(|| contents * (1. / rows as f32))
}

pub(super) const SINCE: [(&str, Option<i64>); 6] = [
    ("Tail", None),
    ("5m", Some(300)),
    ("15m", Some(900)),
    ("1h", Some(3600)),
    ("6h", Some(21600)),
    ("24h", Some(86400)),
];

pub struct LogsView {
    /// Minimum level shown; None shows every line.
    pub(super) min_level: Option<Level>,
    pub(super) since: Option<i64>,
    pub(super) client: Client,
    pub(super) ns: String,
    /// One pod or all pods of a workload.
    pub(super) pods: Vec<String>,
    /// The first pod's containers; the chosen one is read from every pod.
    pub(super) containers: Option<Entity<SelectState<SearchableVec<SharedString>>>>,
    pub(super) container: Option<String>,
    pub(super) previous: bool,
    pub(super) follow: bool,
    pub(super) search: Entity<InputState>,
    /// Find: highlights matches in the shown lines, Enter / Shift+Enter move between them.
    pub(super) find: Entity<InputState>,
    pub(super) find_case: bool,
    pub(super) find_regex: bool,
    pub(super) find_re: Option<regex::Regex>,
    pub(super) find_error: Option<SharedString>,
    /// The current find hit (index into the buffer's hits).
    pub(super) find_cur: usize,
    /// The lines, the shown rows and the find hits.
    pub(super) buf: LogBuffer,
    /// Where each pod's stream is (connecting, reconnecting, ended…).
    pub(super) pod_states: Vec<StreamState>,
    /// Why the container list could not be read; it is read again when streams restart.
    pub(super) status: Option<SharedString>,
    pub(super) reload_containers: bool,
    pub(super) scroll: UniformListScrollHandle,
    /// Rows in the unwrapped list at its last layout (its content height is this many rows).
    pub(super) uniform_rows: usize,
    /// Wrap long lines (variable-height list).
    pub(super) wrap: bool,
    pub(super) wrap_list: ListState,
    /// Show JSON lines as "message key=value…".
    pub(super) pretty_json: bool,
    /// Not streaming: never shown yet, or its page is hidden.
    pub(super) paused: bool,
    pub(super) streams: Vec<TaskGuard>,
    pub(super) _merge: Option<Task<()>>,
    pub(super) _subs: Vec<Subscription>,
    pub(super) _load: Option<Task<()>>,
}

impl LogsView {
    pub(super) fn new(
        client: Client,
        ns: String,
        pods: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Find in logs… (several words; !word excludes)"));
        let find = cx.new(|cx| InputState::new(window, cx).placeholder("Find…"));
        let subs = vec![
            cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.recompute_matches(cx);
                }
            }),
            cx.subscribe_in(&find, window, |this, _, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => {
                    this.find_cur = 0;
                    this.compile_find(cx);
                    this.jump_to_hit(cx);
                }
                InputEvent::PressEnter { shift, .. } => this.step_find(if *shift { -1 } else { 1 }, cx),
                _ => {}
            }),
        ];
        let mut buf = LogBuffer::new(MAX_LOG_LINES);
        buf.set_merge(pods.len() > 1);
        let mut this = Self {
            min_level: None,
            since: None,
            client,
            ns,
            pod_states: vec![StreamState::Connecting; pods.len()],
            pods,
            containers: None,
            container: None,
            previous: false,
            follow: true,
            search,
            find,
            find_case: false,
            find_regex: false,
            find_re: None,
            find_error: None,
            find_cur: 0,
            buf,
            status: None,
            reload_containers: false,
            scroll: UniformListScrollHandle::new(),
            uniform_rows: 0,
            wrap: cfg!(feature = "dev") && std::env::var("RECOUVRANCE_LOG_WRAP").is_ok(),
            wrap_list: ListState::new(0, ListAlignment::Top, px(400.)),
            pretty_json: true,
            paused: true,
            streams: Vec::new(),
            _merge: None,
            _subs: subs,
            _load: None,
        };
        // Streams start when the Logs tab is first shown (`resume`).
        this.load_containers(window, cx);
        this
    }

    /// Stop streaming (lines are kept).
    pub fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            self.streams.clear();
            self._merge = None;
        }
    }

    /// Start or restart streaming.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if self.paused {
            self.restart(cx);
        }
    }

    /// Follow another set of pods (a workload's after a rollout or a scale); streams
    /// restart when they were running. Same pods in another order change nothing.
    pub(super) fn set_pods(&mut self, mut pods: Vec<String>, cx: &mut Context<Self>) {
        pods.sort();
        let mut current = self.pods.clone();
        current.sort();
        if pods == current {
            return;
        }
        self.pods = pods;
        self.buf.set_merge(self.pods.len() > 1);
        self.pod_states = vec![StreamState::Connecting; self.pods.len()];
        // Lines point at pods by index: they belong to the old list.
        self.buf.clear();
        self.wrap_list.reset(0);
        if self.paused {
            self.recompute_matches(cx);
        } else {
            self.restart(cx);
        }
    }

    pub(super) fn load_containers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fut = k8s::containers(self.client.clone(), self.ns.clone(), self.pods[0].clone());
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(names) => {
                    this.status = None;
                    this.container = names.first().cloned();
                    if names.len() > 1 {
                        let items: Vec<SharedString> = names.into_iter().map(Into::into).collect();
                        let select = cx.new(|cx| {
                            SelectState::new(SearchableVec::new(items), Some(IndexPath::default()), window, cx)
                        });
                        this._subs.push(cx.subscribe_in(
                            &select,
                            window,
                            |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _, cx| {
                                let SelectEvent::Confirm(Some(name)) = ev else { return };
                                this.container = Some(name.to_string());
                                this.restart(cx);
                            },
                        ));
                        this.containers = Some(select);
                    }
                    if !this.paused {
                        this.restart(cx);
                    }
                }
                Err(e) => {
                    this.status = Some(format!("Containers not listed: {}", k8s::short_error(&e)).into());
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn restart(&mut self, cx: &mut Context<Self>) {
        self.paused = false;
        self.buf.clear();
        // Rows start over: remeasure the wrapped list.
        self.wrap_list.reset(0);
        // The next render reads the container list again (it needs the window).
        self.reload_containers = self.status.is_some();
        self.pod_states = vec![StreamState::Connecting; self.pods.len()];
        self.streams.clear();
        let mut receivers = Vec::new();
        // A workload's pods share their containers: the chosen one is read from each.
        for (ix, pod) in self.pods.iter().enumerate() {
            let (guard, rx) = k8s::stream_logs(
                self.client.clone(),
                self.ns.clone(),
                pod.clone(),
                self.container.clone(),
                self.previous,
                self.since,
            );
            self.streams.push(guard);
            receivers.push(rx.map(move |m| (ix, m)).boxed());
        }
        let mut merged = futures::stream::select_all(receivers);
        let task = cx.spawn(async move |this, cx| {
            while let Some((ix, msg)) = merged.next().await {
                let alive = this.update(cx, |this, cx| {
                    let state = match msg {
                        k8s::LogMsg::Lines(lines) => {
                            this.add_lines(ix, lines, cx);
                            StreamState::Live
                        }
                        k8s::LogMsg::Reconnecting(why) => StreamState::Reconnecting(why),
                        k8s::LogMsg::End => StreamState::Ended,
                        k8s::LogMsg::Error(e) => StreamState::Failed(e),
                    };
                    if let Some(s) = this.pod_states.get_mut(ix) {
                        *s = state;
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self._merge = Some(task);
        cx.notify();
    }

    /// New lines from pod `ix`: only they are filtered and searched.
    fn add_lines(&mut self, ix: usize, lines: Vec<String>, cx: &mut Context<Self>) {
        // Where the reader is, to keep it on the same lines when rows move: the top row
        // of each list and the current hit's row.
        let top = self.wrap_list.logical_scroll_top();
        let uniform_top = self.uniform_top();
        let hit = self.buf.hits().get(self.find_cur).copied();
        let mut rows = [Some(top.item_ix), uniform_top, hit];
        let before = self.buf.shown_len();
        let pushed = self.buf.push_keeping(ix, lines, &mut rows);
        let count = self.buf.shown_len();
        if pushed.dropped_rows > 0 || pushed.merged {
            // Rows moved: their measured heights no longer apply; put the view back.
            self.wrap_list.reset(count);
            let [wrap_row, uniform_row, hit_row] = rows;
            if hit.is_some() {
                self.find_cur = hit_at(self.buf.hits(), hit_row.unwrap_or(0));
            }
            self.keep_view(top, wrap_row, uniform_top.map(|before| (before, uniform_row.unwrap_or(0))));
        } else {
            // Appended (or rows after `first_row` refiltered): measured rows above stay.
            let from = pushed.first_row.min(before);
            self.wrap_list.splice(from..before, count - from);
        }
        self.after_rows_changed(cx);
    }

    /// The row at the top of the (unwrapped) list, from its scroll offset.
    fn uniform_top(&self) -> Option<usize> {
        let h = self.uniform_row_height()?;
        let offset = self.scroll.0.borrow().base_handle.offset();
        Some((-offset.y / h).max(0.) as usize)
    }

    /// One row's height in the unwrapped list, from its last layout.
    fn uniform_row_height(&self) -> Option<Pixels> {
        let contents = self.scroll.0.borrow().last_item_size?.contents.height;
        row_height(contents, self.uniform_rows)
    }

    /// After rows moved (lines merged in or dropped): keep the view on the same lines.
    /// `top` is the wrapped list's scroll top from before and `wrap_row` its row now
    /// (None when that line went away); `uniform` the unwrapped list's top row before
    /// and now.
    pub(super) fn keep_view(&mut self, top: ListOffset, wrap_row: Option<usize>, uniform: Option<(usize, usize)>) {
        if self.follow {
            return;
        }
        if let (Some(h), Some((before, now))) = (self.uniform_row_height(), uniform) {
            let base = self.scroll.0.borrow().base_handle.clone();
            let mut offset = base.offset();
            offset.y = (offset.y - h * (now as f32 - before as f32)).min(px(0.));
            base.set_offset(offset);
        }
        let (item_ix, offset_in_item) = match wrap_row {
            Some(ix) => (ix, top.offset_in_item),
            None => (0, px(0.)),
        };
        self.wrap_list.scroll_to(ListOffset { item_ix, offset_in_item });
    }

    /// The search or level changed: every line is filtered again.
    pub(super) fn recompute_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_string();
        self.buf.set_filter(&query, self.min_level);
        self.wrap_list.reset(self.buf.shown_len());
        self.after_rows_changed(cx);
    }

    /// Keep the current hit in range and, when following, the last line in view.
    fn after_rows_changed(&mut self, cx: &mut Context<Self>) {
        let hits = self.buf.hits().len();
        if self.find_cur >= hits {
            self.find_cur = hits.saturating_sub(1);
        }
        let count = self.buf.shown_len();
        if self.follow && count > 0 {
            self.scroll.scroll_to_item(count - 1, ScrollStrategy::Bottom);
            self.wrap_list.scroll_to_reveal_item(count - 1);
        }
        cx.notify();
    }

    /// Build the find matcher from the input and toggles, then locate the hits.
    pub(super) fn compile_find(&mut self, cx: &mut Context<Self>) {
        let q = self.find.read(cx).value().to_string();
        self.find_error = None;
        self.find_re = if q.is_empty() {
            None
        } else {
            // Find searches the shown text, where readable JSON keeps value spaces non-breaking.
            let pattern = nbsp_tolerant(&if self.find_regex { q } else { regex::escape(&q) });
            match regex::RegexBuilder::new(&pattern).case_insensitive(!self.find_case).build() {
                Ok(re) => Some(re),
                Err(e) => {
                    self.find_error =
                        Some(e.to_string().lines().last().unwrap_or("invalid regex").trim().to_string().into());
                    None
                }
            }
        };
        self.buf.set_find(self.find_re.clone(), self.pretty_json);
        if self.find_cur >= self.buf.hits().len() {
            self.find_cur = self.buf.hits().len().saturating_sub(1);
        }
        cx.notify();
    }

    pub(super) fn step_find(&mut self, by: isize, cx: &mut Context<Self>) {
        let n = self.buf.hits().len();
        if n == 0 {
            return;
        }
        self.find_cur = (self.find_cur as isize + by).rem_euclid(n as isize) as usize;
        self.jump_to_hit(cx);
    }

    /// Scroll to the current hit; following stops so the view stays there.
    pub(super) fn jump_to_hit(&mut self, cx: &mut Context<Self>) {
        if let Some(&row) = self.buf.hits().get(self.find_cur) {
            self.follow = false;
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
            self.wrap_list.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// Find matches as highlight ranges over `text` (the current hit stronger).
    pub(super) fn find_highlights(
        &self,
        text: &str,
        row: usize,
        cx: &App,
    ) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
        let Some(re) = &self.find_re else { return Vec::new() };
        let current = self.buf.hits().get(self.find_cur) == Some(&row);
        let bg = if current { cx.theme().warning.opacity(0.75) } else { cx.theme().warning.opacity(0.3) };
        re.find_iter(text)
            .filter(|m| !m.is_empty())
            .map(|m| (m.range(), HighlightStyle { background_color: Some(bg), ..Default::default() }))
            .collect()
    }

    /// One log row: time, pod, then the line (JSON made readable when enabled).
    pub(super) fn render_line(&self, i: usize, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let Some(l) = self.buf.shown(i) else { return div().into_any_element() };
        let time = l
            .time
            .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M:%S").to_string())
            .unwrap_or_default();
        let pod = self.pods.get(l.src).map(String::as_str).unwrap_or_default();
        let suffix = pod.rsplit('-').next().unwrap_or(pod).to_string();
        let base = match l.level {
            Some(Level::Error) => Some(theme.danger),
            Some(Level::Warn) => Some(theme.warning),
            _ => None,
        };
        let text: AnyElement = match self.pretty_json.then(|| l.readable()).flatten() {
            Some((line, fields_at)) => {
                let mut highlights = Vec::new();
                if let Some(c) = base {
                    highlights.push((0..fields_at, HighlightStyle { color: Some(c), ..Default::default() }));
                }
                // Keys in blue.
                let mut pos = fields_at;
                for part in line[fields_at..].split(' ') {
                    if let Some(eq) = part.find('=') {
                        highlights.push((
                            pos..pos + eq + 1,
                            HighlightStyle { color: Some(theme.blue), ..Default::default() },
                        ));
                    }
                    pos += part.len() + 1;
                }
                let found = self.find_highlights(line, i, cx);
                StyledText::new(line.clone()).with_highlights(merge_highlights(highlights, found)).into_any_element()
            }
            None => {
                let found = self.find_highlights(&l.text, i, cx);
                if found.is_empty() {
                    div().when_some(base, |d, c| d.text_color(c)).child(l.text.clone()).into_any_element()
                } else {
                    div()
                        .when_some(base, |d, c| d.text_color(c))
                        .child(StyledText::new(l.text.clone()).with_highlights(found))
                        .into_any_element()
                }
            }
        };
        h_flex()
            .id(i)
            .gap_2()
            .items_start()
            .when(!self.wrap, |d| d.whitespace_nowrap())
            .when(crate::settings::get().log_timestamps, |d| {
                d.child(div().w(px(56.)).flex_shrink_0().text_color(theme.muted_foreground).child(time))
            })
            .when(self.pods.len() > 1, |d| {
                d.child(div().w(px(48.)).flex_shrink_0().text_color(pod_color(l.src, cx)).child(suffix))
            })
            .child(div().when(self.wrap, |d| d.flex_1().min_w_0()).child(text))
            .into_any_element()
    }
}

/// Combine color highlights with find backgrounds into sorted, non-overlapping ranges.
pub(super) fn merge_highlights(
    base: Vec<(std::ops::Range<usize>, HighlightStyle)>,
    extra: Vec<(std::ops::Range<usize>, HighlightStyle)>,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    if extra.is_empty() {
        return base;
    }
    let mut cuts: Vec<usize> = base.iter().chain(&extra).flat_map(|(r, _)| [r.start, r.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut style = HighlightStyle::default();
        let mut any = false;
        for (r, s) in base.iter().chain(&extra) {
            if r.start <= a && b <= r.end {
                style = style.highlight(*s);
                any = true;
            }
        }
        if any {
            out.push((a..b, style));
        }
    }
    out
}

impl LogsView {
    /// The lines currently shown (after search and level filters), with time and pod.
    pub(super) fn shown_text(&self) -> String {
        let multi = self.pods.len() > 1;
        self.buf
            .shown_lines()
            .map(|l| {
                let time = l.time.map(|t| t.to_string()).unwrap_or_default();
                let pod = self.pods.get(l.src).map(String::as_str).unwrap_or_default();
                if multi { format!("{time} {pod} {}", l.text) } else { format!("{time} {}", l.text) }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn save_to_downloads(&self) -> std::io::Result<std::path::PathBuf> {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let dir = if home.join("Downloads").is_dir() { home.join("Downloads") } else { home };
        let stamp = jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S");
        let name =
            if self.pods.len() == 1 { self.pods[0].clone() } else { format!("{}-{}pods", self.ns, self.pods.len()) };
        let path = dir.join(format!("{name}-{stamp}.log"));
        std::fs::write(&path, self.shown_text())?;
        Ok(path)
    }

    /// Line counts over time (computed once per change of the shown lines).
    pub(super) fn render_histogram(&mut self, cx: &App) -> AnyElement {
        let Some((buckets, first, last)) = self.buf.histogram() else {
            return div().into_any_element();
        };
        let max = buckets.iter().map(|b| b.0).max().unwrap_or(1).max(1) as f32;
        let theme = cx.theme();
        let fmt = |t: jiff::Timestamp| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M:%S").to_string();
        v_flex()
            .gap_0p5()
            .child(h_flex().h(px(36.)).items_end().gap(px(1.)).children(buckets.iter().map(|&(total, errors)| {
                let h = total as f32 / max;
                let err = if total > 0 { errors as f32 / total as f32 } else { 0. };
                v_flex()
                    .flex_1()
                    .h(relative(h.max(if total > 0 { 0.04 } else { 0. })))
                    .rounded_t_sm()
                    .overflow_hidden()
                    .bg(theme.blue.opacity(0.35))
                    .child(div().w_full().h(relative(1. - err)))
                    .child(div().w_full().flex_1().bg(theme.danger.opacity(0.8)))
            })))
            .child(
                h_flex()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(fmt(*first))
                    .child(div().flex_1())
                    .child(fmt(*last)),
            )
            .into_any_element()
    }

    /// The status line: the streams' state and a container list error.
    fn status_text(&self) -> Option<SharedString> {
        match (stream_status(&self.pods, &self.pod_states), &self.status) {
            (Some(streams), Some(e)) => Some(format!("{streams} · {e}").into()),
            (streams, e) => streams.map(Into::into).or_else(|| e.clone()),
        }
    }
}

impl Render for LogsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.reload_containers) {
            self.load_containers(window, cx);
        }
        let count = self.buf.shown_len();
        if !self.wrap {
            self.uniform_rows = count;
        }
        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let multi = self.pods.len() > 1;
        let histogram = self.render_histogram(cx);
        let hits = self.buf.hits().len();
        v_flex()
            .size_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .when_some(self.containers.clone(), |d, s| {
                        d.child(div().w(px(180.)).child(Select::new(&s).small()))
                    })
                    .when(multi, |d| {
                        d.child(div().text_xs().text_color(muted).child(format!("{} pods", self.pods.len())))
                    })
                    .child(div().flex_1().child(Input::new(&self.search).small().cleanable(true)))
                    .child(Checkbox::new("follow").label("Follow").checked(self.follow).on_click(cx.listener(
                        |this, checked: &bool, _, cx| {
                            this.follow = *checked;
                            this.after_rows_changed(cx);
                        },
                    )))
                    .child(Checkbox::new("previous").label("Previous").checked(self.previous).on_click(cx.listener(
                        |this, checked: &bool, _, cx| {
                            this.previous = *checked;
                            this.restart(cx);
                        },
                    )))
                    .child(Checkbox::new("wrap").label("Wrap").checked(self.wrap).on_click(cx.listener(
                        |this, v: &bool, _, cx| {
                            this.wrap = *v;
                            this.wrap_list.reset(this.buf.shown_len());
                            this.after_rows_changed(cx);
                        },
                    )))
                    .child(Checkbox::new("pretty-json").label("Readable JSON").checked(self.pretty_json).on_click(
                        cx.listener(|this, v: &bool, _, cx| {
                            this.pretty_json = *v;
                            this.wrap_list.reset(this.buf.shown_len());
                            this.compile_find(cx);
                            this.after_rows_changed(cx);
                        }),
                    ))
                    .child(
                        Button::new("reload")
                            .small()
                            .ghost()
                            .label("Reload")
                            .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
                    )
                    .child(
                        Button::new("copy-logs")
                            .small()
                            .ghost()
                            .icon(assets::IconName::Copy)
                            .tooltip("Copy the shown lines")
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(this.shown_text()));
                            })),
                    )
                    .child(
                        Button::new("save-logs")
                            .small()
                            .ghost()
                            .icon(assets::IconName::Download)
                            .tooltip("Save the shown lines to ~/Downloads")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let note = match this.save_to_downloads() {
                                    Ok(path) => Notification::success(format!("Saved {}", path.display())),
                                    Err(e) => Notification::error(format!("Could not save logs: {e}")),
                                };
                                window.push_notification(note, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().text_color(muted).child("Level"))
                    .children(
                        [
                            ("All", None),
                            ("Debug+", Some(Level::Debug)),
                            ("Info+", Some(Level::Info)),
                            ("Warn+", Some(Level::Warn)),
                            ("Error", Some(Level::Error)),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(i, (label, lv))| {
                            Button::new(("lvl", i))
                                .xsmall()
                                .label(label)
                                .when(self.min_level == lv, |b| b.primary())
                                .when(self.min_level != lv, |b| b.ghost())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.min_level = lv;
                                    this.recompute_matches(cx);
                                }))
                        }),
                    )
                    .child(div().w(px(16.)))
                    .child(div().text_color(muted).child("Since"))
                    .children(SINCE.into_iter().enumerate().map(|(i, (label, secs))| {
                        Button::new(("since", i))
                            .xsmall()
                            .label(label)
                            .when(self.since == secs, |b| b.primary())
                            .when(self.since != secs, |b| b.ghost())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.since = secs;
                                this.restart(cx);
                            }))
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().w(px(260.)).child(
                        Input::new(&self.find).small().cleanable(true).prefix(Icon::new(Lucide::Search).size_3p5()),
                    ))
                    .child(
                        Button::new("find-case")
                            .xsmall()
                            .label("Aa")
                            .tooltip("Match case")
                            .when(self.find_case, |b| b.primary())
                            .when(!self.find_case, |b| b.ghost())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.find_case = !this.find_case;
                                this.compile_find(cx);
                            })),
                    )
                    .child(
                        Button::new("find-regex")
                            .xsmall()
                            .label(".*")
                            .tooltip("Regular expression")
                            .when(self.find_regex, |b| b.primary())
                            .when(!self.find_regex, |b| b.ghost())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.find_regex = !this.find_regex;
                                this.compile_find(cx);
                            })),
                    )
                    .child(
                        Button::new("find-prev")
                            .xsmall()
                            .ghost()
                            .icon(Lucide::ChevronUp)
                            .tooltip("Previous match (Shift+Enter)")
                            .on_click(cx.listener(|this, _, _, cx| this.step_find(-1, cx))),
                    )
                    .child(
                        Button::new("find-next")
                            .xsmall()
                            .ghost()
                            .icon(Lucide::ChevronDown)
                            .tooltip("Next match (Enter)")
                            .on_click(cx.listener(|this, _, _, cx| this.step_find(1, cx))),
                    )
                    .child(div().text_color(if self.find_error.is_some() { cx.theme().danger } else { muted }).child(
                        match (&self.find_error, &self.find_re) {
                            (Some(e), _) => e.clone(),
                            (None, Some(_)) if hits == 0 => "No matches".into(),
                            (None, Some(_)) => format!("{} of {hits}", self.find_cur + 1).into(),
                            _ => SharedString::default(),
                        },
                    )),
            )
            .when_some(self.status_text(), |d, s| d.child(div().text_xs().text_color(muted).child(s)))
            .child(histogram)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .p_1()
                    .font_family(mono)
                    .text_xs()
                    .map(|d| {
                        if self.wrap {
                            let this = cx.entity().downgrade();
                            d.child(
                                list(self.wrap_list.clone(), move |i, _, cx| {
                                    this.upgrade()
                                        .map(|t| t.read(cx).render_line(i, cx))
                                        .unwrap_or_else(|| div().into_any_element())
                                })
                                .size_full(),
                            )
                        } else {
                            d.child(
                                uniform_list(
                                    "log-lines",
                                    count,
                                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                        range.map(|i| this.render_line(i, cx)).collect()
                                    }),
                                )
                                .track_scroll(&self.scroll)
                                .size_full(),
                            )
                        }
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{px, row_height};

    #[test]
    fn row_height_is_the_content_over_its_rows() {
        assert_eq!(row_height(px(2000.), 100), Some(px(20.)));
        assert_eq!(row_height(px(0.), 0), None);
        assert_eq!(row_height(px(2000.), 0), None);
    }
}
