//! The log viewer's model, without any UI: the lines of every stream merged in time
//! order, the search filter and find hits kept up to date as lines arrive (only new
//! lines are looked at), the oldest lines dropped past a limit, and stream states.

use std::cell::OnceCell;
use std::collections::VecDeque;

use gpui_kit::SharedString;

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub(crate) enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

/// Best-effort level of a log line: explicit fields first (JSON, logfmt,
/// klog), then keywords near the start.
pub(crate) fn level_of(line: &str) -> Option<Level> {
    let head: String = line.chars().take(400).collect::<String>().to_lowercase();
    let from_word = |w: &str| match w {
        w if w.starts_with("err") || w.starts_with("fatal") || w.starts_with("crit") || w.starts_with("panic") => {
            Some(Level::Error)
        }
        w if w.starts_with("warn") => Some(Level::Warn),
        w if w.starts_with("info") || w.starts_with("notice") => Some(Level::Info),
        w if w.starts_with("debug") || w.starts_with("trace") => Some(Level::Debug),
        _ => None,
    };
    for key in ["\"levelname\":\"", "\"level\":\"", "\"severity\":\"", "level=", "lvl=", "severity="] {
        if let Some(pos) = head.find(key) {
            let rest = head[pos + key.len()..].trim_start_matches('"');
            if let Some(level) = from_word(rest) {
                return Some(level);
            }
        }
    }
    // klog: "E1015 12:00:00 ..."
    let mut chars = line.chars();
    if let (Some(c), Some(d)) = (chars.next(), chars.next())
        && d.is_ascii_digit()
    {
        match c {
            'E' | 'F' => return Some(Level::Error),
            'W' => return Some(Level::Warn),
            'I' => return Some(Level::Info),
            _ => {}
        }
    }
    let early: String = head.chars().take(80).collect();
    ["error", "fatal", "panic", "warn", "info", "debug"].iter().find(|w| early.contains(*w)).and_then(|w| from_word(w))
}

pub(crate) struct LogLine {
    pub(crate) src: usize,
    pub(crate) time: Option<jiff::Timestamp>,
    /// Where the line sorts among other pods' lines: its time, or the time of the line
    /// before it in its stream (lines without one stay next to their neighbours).
    key: Option<jiff::Timestamp>,
    pub(crate) text: SharedString,
    pub(crate) level: Option<Level>,
    /// Computed once, when first searched or shown.
    lower: OnceCell<Box<str>>,
    readable: OnceCell<Option<(SharedString, usize)>>,
}

impl LogLine {
    /// Split the RFC3339 prefix that `timestamps=true` adds.
    pub(crate) fn parse(src: usize, raw: String) -> Self {
        let (time, text) = match raw.split_once(' ') {
            Some((ts, rest)) => match ts.parse::<jiff::Timestamp>() {
                Ok(t) => (Some(t), rest.to_string()),
                Err(_) => (None, raw),
            },
            None => (None, raw),
        };
        let level = level_of(&text);
        Self { src, time, key: time, text: text.into(), level, lower: OnceCell::new(), readable: OnceCell::new() }
    }

    /// The text in lowercase, for the search filter.
    pub(crate) fn lower(&self) -> &str {
        self.lower.get_or_init(|| self.text.to_lowercase().into())
    }

    /// The line as readable JSON ("message key=value…", where the fields start), if it is JSON.
    pub(crate) fn readable(&self) -> Option<(&SharedString, usize)> {
        self.readable
            .get_or_init(|| readable_json(&self.text).map(|(l, at)| (l.into(), at)))
            .as_ref()
            .map(|(l, at)| (l, *at))
    }

    /// The line as shown, which find searches.
    pub(crate) fn display(&self, pretty_json: bool) -> &str {
        match pretty_json.then(|| self.readable()).flatten() {
            Some((line, _)) => line.as_ref(),
            None => self.text.as_ref(),
        }
    }
}

/// `{"level":"info","msg":"started","port":80}` -> ("started  port=80", where fields start).
/// None when the line is not a JSON object.
pub(crate) fn readable_json(text: &str) -> Option<(String, usize)> {
    let t = text.trim();
    if !t.starts_with('{') {
        return None;
    }
    let obj: serde_json::Map<String, serde_json::Value> = serde_json::from_str(t).ok()?;
    let skip = ["level", "levelname", "severity", "lvl", "time", "timestamp", "ts", "@timestamp"];
    let msg_key = ["msg", "message", "event", "log"].into_iter().find(|k| obj.contains_key(*k));
    let mut line = msg_key
        .and_then(|k| obj.get(k))
        .map(|m| m.as_str().map(String::from).unwrap_or_else(|| m.to_string()))
        .unwrap_or_default();
    let fields_at = line.len();
    // Sorted, so the same fields line up from one line to the next.
    let mut fields: Vec<_> = obj.iter().collect();
    fields.sort_by_key(|(k, _)| k.as_str());
    for (k, v) in fields {
        if Some(k.as_str()) == msg_key || skip.contains(&k.as_str()) {
            continue;
        }
        let v = match v {
            serde_json::Value::String(s) => s.replace(' ', "\u{00a0}"),
            other => other.to_string(),
        };
        line.push(' ');
        line.push_str(&format!("{k}={v}"));
    }
    Some((line, fields_at))
}

/// Search words (all must appear; "!word" lines are left out) and a minimum level.
#[derive(Default, PartialEq)]
struct LineFilter {
    include: Vec<String>,
    exclude: Vec<String>,
    min: Option<Level>,
}

impl LineFilter {
    fn new(query: &str, min: Option<Level>) -> Self {
        let query = query.to_lowercase();
        let (include, exclude): (Vec<&str>, Vec<&str>) = query.split_whitespace().partition(|w| !w.starts_with('!'));
        Self {
            include: include.into_iter().map(String::from).collect(),
            exclude: exclude.iter().map(|w| w[1..].to_string()).filter(|w| !w.is_empty()).collect(),
            min,
        }
    }

    /// Whether every line is shown (no filter at all).
    fn passes_all(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty() && self.min.is_none()
    }

    fn matches(&self, l: &LogLine) -> bool {
        if self.min.is_some_and(|m| l.level.is_none_or(|lv| lv < m)) {
            return false;
        }
        if self.include.is_empty() && self.exclude.is_empty() {
            return true;
        }
        let t = l.lower();
        self.include.iter().all(|w| t.contains(w.as_str())) && !self.exclude.iter().any(|w| t.contains(w.as_str()))
    }
}

pub(crate) const HISTOGRAM_BUCKETS: usize = 80;

/// (total, errors) per bucket, and the time range covered.
pub(crate) type Histogram = (Vec<(usize, usize)>, jiff::Timestamp, jiff::Timestamp);

/// What a batch of new lines did to the shown rows.
#[derive(Debug, PartialEq)]
pub(crate) struct Pushed {
    /// Rows from here on are new or changed (after the drop below).
    pub(crate) first_row: usize,
    /// Shown rows that went away from the top (the line limit).
    pub(crate) dropped_rows: usize,
    /// Lines went in among older ones (several pods): rows moved.
    pub(crate) merged: bool,
}

pub(crate) struct LogBuffer {
    lines: VecDeque<LogLine>,
    /// Lines dropped from the front so far: `lines[i]` is line number `offset + i`.
    offset: usize,
    max: usize,
    /// Several streams: keep their lines in one timeline.
    merge: bool,
    filter: LineFilter,
    /// Numbers of the lines that pass the filter; None when it passes everything.
    matches: Option<VecDeque<usize>>,
    find: Option<regex::Regex>,
    pretty_json: bool,
    /// Shown rows holding a find match, in order.
    hits: Vec<usize>,
    /// The histogram of the shown lines, until they change.
    histogram: Option<Option<Histogram>>,
}

impl LogBuffer {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            offset: 0,
            max,
            merge: false,
            filter: LineFilter::default(),
            matches: None,
            find: None,
            pretty_json: true,
            hits: Vec::new(),
            histogram: None,
        }
    }

    /// Forget every line (filters and find stay).
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.offset = 0;
        self.matches = self.matches.as_ref().map(|_| VecDeque::new());
        self.hits.clear();
        self.histogram = None;
    }

    /// Lines come from several streams and are merged by time.
    pub(crate) fn set_merge(&mut self, merge: bool) {
        self.merge = merge;
    }

    pub(crate) fn shown_len(&self) -> usize {
        self.matches.as_ref().map_or(self.lines.len(), VecDeque::len)
    }

    /// The line on a shown row.
    pub(crate) fn shown(&self, row: usize) -> Option<&LogLine> {
        let ix = match &self.matches {
            Some(m) => m.get(row)?.checked_sub(self.offset)?,
            None => row,
        };
        self.lines.get(ix)
    }

    /// Every shown line, in order.
    pub(crate) fn shown_lines(&self) -> impl Iterator<Item = &LogLine> {
        (0..self.shown_len()).filter_map(|r| self.shown(r))
    }

    /// Shown rows holding a find match.
    pub(crate) fn hits(&self) -> &[usize] {
        &self.hits
    }

    /// A new search or level: every line is looked at again.
    pub(crate) fn set_filter(&mut self, query: &str, min: Option<Level>) {
        self.filter = LineFilter::new(query, min);
        self.refilter();
    }

    /// A new find pattern, or the shown text changed (readable JSON): rows are searched again.
    pub(crate) fn set_find(&mut self, find: Option<regex::Regex>, pretty_json: bool) {
        self.find = find;
        self.pretty_json = pretty_json;
        self.hits.clear();
        self.search_hits(0);
    }

    fn refilter(&mut self) {
        self.matches = (!self.filter.passes_all()).then(|| {
            let (filter, offset) = (&self.filter, self.offset);
            self.lines.iter().enumerate().filter(|(_, l)| filter.matches(l)).map(|(i, _)| offset + i).collect()
        });
        self.hits.clear();
        self.search_hits(0);
        self.histogram = None;
    }

    /// Find hits on shown rows from `from` on (the ones before are known).
    fn search_hits(&mut self, from: usize) {
        let Some(re) = &self.find else { return };
        let found: Vec<usize> = (from..self.shown_len())
            .filter(|&r| self.shown(r).is_some_and(|l| re.is_match(l.display(self.pretty_json))))
            .collect();
        self.hits.extend(found);
    }

    /// Add a batch of raw lines from stream `src`. Only the new lines (and, when they
    /// go in among older ones, the lines after them) are filtered and searched.
    #[cfg(test)]
    pub(crate) fn push(&mut self, src: usize, raw: Vec<String>) -> Pushed {
        self.push_keeping(src, raw, &mut [])
    }

    /// The index in `lines` of the line on a shown row.
    fn line_ix(&self, row: usize) -> Option<usize> {
        match &self.matches {
            Some(m) => m.get(row)?.checked_sub(self.offset),
            None => (row < self.lines.len()).then_some(row),
        }
    }

    /// `push`, and `rows` (shown rows from before) become the rows of the same lines
    /// after it: lines merged in above them move them down, dropped lines up. None
    /// when the line itself went away.
    pub(crate) fn push_keeping(&mut self, src: usize, raw: Vec<String>, rows: &mut [Option<usize>]) -> Pushed {
        let mut kept: Vec<Option<usize>> = rows.iter().map(|r| r.and_then(|r| self.line_ix(r))).collect();
        let mut prev = self.lines.back().and_then(|l| l.key);
        let new: Vec<LogLine> = raw
            .into_iter()
            .map(|r| {
                let mut l = LogLine::parse(src, r);
                l.key = l.time.or(prev);
                prev = l.key;
                l
            })
            .collect();
        let old_len = self.lines.len();
        // Where the batch goes: after every line not later than its first.
        let pos = match new.first() {
            Some(first) if self.merge => self.lines.partition_point(|l| l.key <= first.key),
            _ => old_len,
        };
        let merged = pos < old_len;
        if merged {
            // Where each line from `pos` on ends up.
            let mut moved = Vec::with_capacity(old_len - pos);
            let tail: Vec<LogLine> = self.lines.drain(pos..).collect();
            let (mut old, mut new) = (tail.into_iter().peekable(), new.into_iter().peekable());
            // A stable merge: on equal times, lines already there stay first.
            loop {
                let take_new = match (old.peek(), new.peek()) {
                    (Some(o), Some(n)) => n.key < o.key,
                    (None, Some(_)) => true,
                    (Some(_), None) => false,
                    (None, None) => break,
                };
                if !take_new {
                    moved.push(self.lines.len());
                }
                let line = if take_new { new.next() } else { old.next() };
                self.lines.extend(line);
            }
            for ix in kept.iter_mut().flatten() {
                if *ix >= pos {
                    *ix = moved[*ix - pos];
                }
            }
        } else {
            self.lines.extend(new);
        }
        // Filter the lines from `pos` on; shown rows before it stay as they are.
        let first_row = match &mut self.matches {
            Some(m) => {
                let from = self.offset + pos;
                while m.back().is_some_and(|&i| i >= from) {
                    m.pop_back();
                }
                let row = m.len();
                let (filter, offset) = (&self.filter, self.offset);
                m.extend(
                    self.lines
                        .range(pos..)
                        .enumerate()
                        .filter(|(_, l)| filter.matches(l))
                        .map(|(i, _)| offset + pos + i),
                );
                row
            }
            None => pos,
        };
        self.hits.truncate(self.hits.partition_point(|&h| h < first_row));
        // Past the limit, the oldest lines go.
        let mut dropped_rows = 0;
        if self.lines.len() > self.max {
            let drop = self.lines.len() - self.max;
            self.lines.drain(..drop);
            self.offset += drop;
            kept.iter_mut().for_each(|ix| *ix = ix.and_then(|i| i.checked_sub(drop)));
            dropped_rows = match &mut self.matches {
                Some(m) => {
                    let gone = m.partition_point(|&i| i < self.offset);
                    m.drain(..gone);
                    gone
                }
                None => drop,
            };
            let gone = self.hits.partition_point(|&h| h < dropped_rows);
            self.hits.drain(..gone);
            self.hits.iter_mut().for_each(|h| *h -= dropped_rows);
        }
        let first_row = first_row.saturating_sub(dropped_rows);
        self.search_hits(first_row);
        self.histogram = None;
        for (row, ix) in rows.iter_mut().zip(kept) {
            *row = ix.and_then(|i| match &self.matches {
                Some(m) => m.binary_search(&(self.offset + i)).ok(),
                None => Some(i),
            });
        }
        Pushed { first_row, dropped_rows, merged }
    }

    /// Line counts over time for the shown lines: (total, errors) per bucket. Kept
    /// until the lines or the filter change.
    pub(crate) fn histogram(&mut self) -> Option<&Histogram> {
        if self.histogram.is_none() {
            self.histogram = Some(self.compute_histogram());
        }
        self.histogram.as_ref().and_then(Option::as_ref)
    }

    fn compute_histogram(&self) -> Option<Histogram> {
        let times = || self.shown_lines().filter_map(|l| l.time);
        let (first, last) = (times().min()?, times().max()?);
        let span = (last.as_millisecond() - first.as_millisecond()).max(1) as f64;
        let mut buckets = vec![(0usize, 0usize); HISTOGRAM_BUCKETS];
        for l in self.shown_lines() {
            let Some(t) = l.time else { continue };
            let pos =
                ((t.as_millisecond() - first.as_millisecond()) as f64 / span * (HISTOGRAM_BUCKETS - 1) as f64) as usize;
            let b = &mut buckets[pos.min(HISTOGRAM_BUCKETS - 1)];
            b.0 += 1;
            if l.level == Some(Level::Error) {
                b.1 += 1;
            }
        }
        Some((buckets, first, last))
    }
}

/// The find hit to make current: the first one at or after `row`.
pub(crate) fn hit_at(hits: &[usize], row: usize) -> usize {
    hits.partition_point(|&h| h < row).min(hits.len().saturating_sub(1))
}

/// A find pattern where a space also matches the non-breaking spaces that
/// readable JSON puts in values (outside escapes, so `\ ` stays literal).
pub(crate) fn nbsp_tolerant(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut escaped = false;
    for c in pattern.chars() {
        if c == ' ' && !escaped {
            out.push_str("[ \u{a0}]");
        } else {
            out.push(c);
        }
        escaped = c == '\\' && !escaped;
    }
    out
}

/// The container to read from a pod: the chosen one when the pod has it (or its
/// containers are unknown), else the pod's default one (also when none is chosen: the
/// API server does not pick one for a pod with several).
pub(crate) fn container_for(chosen: Option<&str>, containers: &[String]) -> Option<String> {
    let Some(chosen) = chosen else { return containers.first().cloned() };
    if containers.is_empty() || containers.iter().any(|c| c == chosen) {
        Some(chosen.to_string())
    } else {
        containers.first().cloned()
    }
}

/// Where one pod's stream is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StreamState {
    Connecting,
    Live,
    /// Cut off while following; trying again (why).
    Reconnecting(String),
    Ended,
    Failed(String),
}

/// What the status line says about the streams (None when they all flow).
pub(crate) fn stream_status(pods: &[String], states: &[StreamState]) -> Option<String> {
    if let [state] = states {
        return match state {
            StreamState::Connecting => Some("Connecting…".into()),
            StreamState::Live => None,
            StreamState::Reconnecting(why) => Some(format!("Reconnecting… ({why})")),
            StreamState::Ended => Some("Stream ended".into()),
            StreamState::Failed(e) => Some(e.clone()),
        };
    }
    if !states.is_empty() && states.iter().all(|s| *s == StreamState::Connecting) {
        return Some("Connecting…".into());
    }
    let short = |i: usize| pods.get(i).map(|p| p.rsplit('-').next().unwrap_or(p).to_string()).unwrap_or_default();
    let group = |label: &str, pick: &dyn Fn(&StreamState) -> Option<String>| {
        let names: Vec<String> =
            states.iter().enumerate().filter_map(|(i, s)| pick(s).map(|n| n + &short(i))).collect();
        (!names.is_empty()).then(|| format!("{} {label} ({})", names.len(), names.join(", ")))
    };
    let parts: Vec<String> = [
        group("reconnecting", &|s| matches!(s, StreamState::Reconnecting(_)).then(String::new)),
        group("ended", &|s| (*s == StreamState::Ended).then(String::new)),
        group("failed", &|s| match s {
            StreamState::Failed(e) => Some(format!("{e}: ")),
            _ => None,
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64, text: &str) -> String {
        format!("{} {text}", jiff::Timestamp::from_second(1_700_000_000 + secs).unwrap())
    }

    fn texts(b: &LogBuffer) -> Vec<String> {
        b.shown_lines().map(|l| l.text.to_string()).collect()
    }

    #[test]
    fn reads_the_chosen_container_only_from_pods_that_have_it() {
        let pod = |cs: &[&str]| cs.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        assert_eq!(container_for(Some("api"), &pod(&["api", "proxy"])).as_deref(), Some("api"));
        assert_eq!(container_for(Some("api"), &pod(&["worker", "proxy"])).as_deref(), Some("worker"));
        assert_eq!(container_for(Some("api"), &[]).as_deref(), Some("api"));
        // Nothing chosen (the container list could not be read): the pod's default one.
        assert_eq!(container_for(None, &pod(&["worker", "proxy"])).as_deref(), Some("worker"));
        assert_eq!(container_for(None, &[]), None);
    }

    #[test]
    fn detects_levels() {
        assert!(level_of(r#"{"levelname":"INFO","msg":"no error here"}"#) == Some(Level::Info));
        assert!(level_of("time=1 level=warn msg=x") == Some(Level::Warn));
        assert!(level_of("E1015 12:00:00.1 1 foo.go:1] boom") == Some(Level::Error));
        assert!(level_of("2025/07/22 08:56:50 [error] 34#34: open()") == Some(Level::Error));
        assert!(level_of("127.0.0.1 - - GET /a 200").is_none());
    }

    #[test]
    fn readable_json_lines() {
        let (line, at) =
            readable_json(r#"{"level":"info","msg":"started server","port":80,"addr":"0.0.0.0"}"#).unwrap();
        assert_eq!(&line[..at], "started server");
        assert!(line.contains("port=80") && line.contains("addr=0.0.0.0") && !line.contains("level="));
        assert!(readable_json("plain text").is_none());
        // Cached on the line, as shown.
        let l = LogLine::parse(0, r#"{"msg":"hi","n":1}"#.into());
        assert_eq!(l.display(true), "hi n=1");
        assert_eq!(l.display(false), r#"{"msg":"hi","n":1}"#);
    }

    #[test]
    fn find_matches_spaces_in_readable_json() {
        let (line, _) = readable_json(r#"{"msg":"x","err":"connection refused by peer"}"#).unwrap();
        let re = |p: &str| regex::Regex::new(&nbsp_tolerant(p)).unwrap();
        assert!(re(&regex::escape("refused by")).is_match(&line));
        assert!(re("refused[a ]by").is_match(&line));
        assert!(re(r"refused\ by").is_match("refused by"));
        assert!(!re(r"refused\ by").is_match(&line));
    }

    #[test]
    fn find_hit_at_a_row() {
        assert_eq!(hit_at(&[0, 5, 15], 5), 1);
        assert_eq!(hit_at(&[0, 5, 15], 6), 2);
        assert_eq!(hit_at(&[0, 5, 15], 99), 2);
        assert_eq!(hit_at(&[], 3), 0);
    }

    #[test]
    fn rows_follow_merged_and_dropped_lines() {
        let mut b = LogBuffer::new(100);
        b.set_merge(true);
        b.push(0, vec![at(1, "a1"), at(3, "a3"), at(5, "a5")]);
        // A lagging pod's lines go in above "a3" and "a5": they move down.
        let mut rows = [Some(0), Some(1), Some(2)];
        b.push_keeping(1, vec![at(2, "b2"), at(4, "b4")], &mut rows);
        assert_eq!(texts(&b), ["a1", "b2", "a3", "b4", "a5"]);
        assert_eq!(rows, [Some(0), Some(2), Some(4)]);
        // Filtered: rows are shown rows.
        b.set_filter("a", None);
        let mut rows = [Some(1), Some(2)];
        b.push_keeping(1, vec![at(0, "a0"), at(4, "b4b")], &mut rows);
        assert_eq!(texts(&b), ["a0", "a1", "a3", "a5"]);
        assert_eq!(rows, [Some(2), Some(3)]);
        // Past the limit: rows move up; a dropped line's row is gone.
        let mut small = LogBuffer::new(3);
        small.push(0, vec![at(0, "l0"), at(1, "l1"), at(2, "l2")]);
        let mut rows = [Some(0), Some(2)];
        small.push_keeping(0, vec![at(3, "l3")], &mut rows);
        assert_eq!(rows, [None, Some(1)]);
    }

    #[test]
    fn merges_pods_out_of_order() {
        let mut b = LogBuffer::new(100);
        b.set_merge(true);
        let p = b.push(0, vec![at(1, "a1"), at(3, "a3"), at(5, "a5")]);
        assert_eq!(p, Pushed { first_row: 0, dropped_rows: 0, merged: false });
        let p = b.push(1, vec![at(2, "b2"), at(4, "b4"), at(6, "b6")]);
        assert_eq!(p, Pushed { first_row: 1, dropped_rows: 0, merged: true });
        assert_eq!(texts(&b), ["a1", "b2", "a3", "b4", "a5", "b6"]);
        // Same time: the line already there stays first; no time: stays with its stream.
        b.push(0, vec![at(6, "a6"), "no time".into()]);
        assert_eq!(texts(&b)[5..], ["b6", "a6", "no time"]);
        // One pod: lines stay in arrival order.
        let mut one = LogBuffer::new(100);
        one.push(0, vec![at(5, "late")]);
        one.push(0, vec![at(1, "early")]);
        assert_eq!(texts(&one), ["late", "early"]);
    }

    #[test]
    fn drops_the_oldest_past_the_limit() {
        let mut b = LogBuffer::new(5);
        let p = b.push(0, (0..8).map(|i| at(i, &format!("l{i}"))).collect());
        assert_eq!(p, Pushed { first_row: 0, dropped_rows: 3, merged: false });
        assert_eq!(texts(&b), ["l3", "l4", "l5", "l6", "l7"]);
        let p = b.push(0, vec![at(8, "l8")]);
        assert_eq!(p, Pushed { first_row: 4, dropped_rows: 1, merged: false });
        assert_eq!(texts(&b)[0], "l4");
    }

    #[test]
    fn filter_and_find_follow_drops() {
        let mut b = LogBuffer::new(6);
        b.set_filter("err", None);
        b.set_find(Some(regex::Regex::new("x").unwrap()), true);
        b.push(0, vec![at(0, "err a"), at(1, "ok"), at(2, "err x"), at(3, "err b"), at(4, "err x2")]);
        assert_eq!(texts(&b), ["err a", "err x", "err b", "err x2"]);
        assert_eq!(b.hits(), [1, 3]);
        // Two lines go ("err a", "ok"), one shown row with them.
        let p = b.push(0, vec![at(5, "ok"), at(6, "err x3"), at(7, "fine")]);
        assert_eq!(p, Pushed { first_row: 3, dropped_rows: 1, merged: false });
        assert_eq!(texts(&b), ["err x", "err b", "err x2", "err x3"]);
        assert_eq!(b.hits(), [0, 2, 3]);
        assert_eq!(b.shown(2).map(|l| l.text.to_string()).as_deref(), Some("err x2"));
    }

    #[test]
    fn incremental_filter_equals_full_recompute() {
        let mut b = LogBuffer::new(40);
        b.set_merge(true);
        b.set_filter("err !skip", Some(Level::Info));
        b.set_find(Some(regex::Regex::new("[13579]$").unwrap()), true);
        let mut t = 0;
        for round in 0..30 {
            let src = round % 3;
            // Each pod lags a little: its batch lands among the others' lines.
            let lines: Vec<String> = (0..(round % 7 + 1))
                .map(|i| {
                    t += 1;
                    let word = ["error", "info", "warn err", "err skip", "debug err"][(t + i) % 5];
                    at((t as i64) - (src as i64) * 3, &format!("level={word} n{t}"))
                })
                .collect();
            b.push(src, lines);
            let (rows, hits) = (texts(&b), b.hits().to_vec());
            b.refilter();
            assert_eq!(texts(&b), rows, "round {round}");
            assert_eq!(b.hits(), hits, "round {round}");
        }
        assert!(b.shown_len() > 0 && b.lines.len() == 40);
    }

    #[test]
    fn histogram_is_cached_until_lines_change() {
        let mut b = LogBuffer::new(100);
        assert!(b.histogram().is_none());
        b.push(0, vec![at(0, "error boom"), at(10, "ok")]);
        let (buckets, ..) = b.histogram().cloned().unwrap();
        assert_eq!(buckets.iter().map(|b| b.0).sum::<usize>(), 2);
        assert_eq!(buckets.iter().map(|b| b.1).sum::<usize>(), 1);
        b.set_filter("ok", None);
        assert_eq!(b.histogram().unwrap().0.iter().map(|b| b.0).sum::<usize>(), 1);
    }

    #[test]
    fn stream_statuses() {
        let pods: Vec<String> = ["web-a1", "web-b2", "web-c3"].map(String::from).to_vec();
        let one = &pods[..1];
        assert_eq!(stream_status(one, &[Live]), None);
        assert_eq!(stream_status(one, &[Ended]).as_deref(), Some("Stream ended"));
        assert_eq!(stream_status(one, &[Reconnecting("EOF".into())]).as_deref(), Some("Reconnecting… (EOF)"));
        use StreamState::*;
        assert_eq!(stream_status(&pods, &[Connecting, Connecting, Connecting]).as_deref(), Some("Connecting…"));
        assert_eq!(stream_status(&pods, &[Live, Live, Live]), None);
        assert_eq!(
            stream_status(&pods, &[Reconnecting("x".into()), Ended, Failed("gone".into())]).as_deref(),
            Some("1 reconnecting (a1) · 1 ended (b2) · 1 failed (gone: c3)")
        );
    }
}
