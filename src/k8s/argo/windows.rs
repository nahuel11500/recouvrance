//! Argo CD's sync windows: the times a project lets its Applications sync, and when an
//! Application can sync next.
//!
//! A window is `allow` or `deny`, opens on a cron `schedule` (in its `timeZone`, UTC when
//! none) and stays open for its `duration`. It applies to the Applications whose name, whose
//! destination namespace, or whose destination cluster (server or name) one of its patterns
//! matches (all of the lists given, with `andOperator`). Argo CD's rule for a sync at a time:
//! an open deny window blocks it (a manual sync goes when every open deny window has
//! `manualSync`); else an open allow window lets it; else, when the Application has allow
//! windows, none being open blocks it (a manual sync goes when one of them has `manualSync`);
//! else it can sync.

use std::collections::HashMap;
use std::sync::Arc;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;

use crate::cron::Cron;

/// How far ahead the next time an Application can sync is looked for.
pub const HORIZON_DAYS: i64 = 8;
/// The most times asked of a window's schedule (see `openings`).
const MAX_STEPS: usize = 50_000;

/// One sync window of a project.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SyncWindow {
    /// `allow`; anything else is read as `deny`, the stricter.
    pub allow: bool,
    pub schedule: String,
    /// As Argo CD writes it: `1h`, `30m`, `1h30m`.
    pub duration: String,
    pub applications: Vec<String>,
    pub namespaces: Vec<String>,
    pub clusters: Vec<String>,
    pub manual_sync: bool,
    pub time_zone: String,
    pub and_operator: bool,
}

impl SyncWindow {
    pub fn of(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let list = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        };
        Self {
            allow: v.get("kind").and_then(Value::as_str) == Some("allow"),
            schedule: s("schedule"),
            duration: s("duration"),
            applications: list("applications"),
            namespaces: list("namespaces"),
            clusters: list("clusters"),
            manual_sync: v.get("manualSync").and_then(Value::as_bool).unwrap_or(false),
            time_zone: s("timeZone"),
            and_operator: v.get("andOperator").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    /// Whether it applies to the Application `name` deploying to `namespace` on the cluster
    /// `server` (or named `cluster`). A window naming nothing applies to nothing.
    pub fn applies(&self, name: &str, namespace: &str, server: &str, cluster: &str) -> bool {
        let any = |patterns: &[String], f: &dyn Fn(&str) -> bool| patterns.iter().any(|p| f(p));
        let lists: [(&[String], bool); 3] = [
            (&self.applications, any(&self.applications, &|p| crate::glob::glob(p, name))),
            (&self.namespaces, any(&self.namespaces, &|p| crate::glob::glob(p, namespace))),
            (
                &self.clusters,
                any(&self.clusters, &|p| {
                    (!server.is_empty() && crate::glob::glob(p, server))
                        || (!cluster.is_empty() && crate::glob::glob(p, cluster))
                }),
            ),
        ];
        let given: Vec<bool> = lists.iter().filter(|(l, _)| !l.is_empty()).map(|(_, m)| *m).collect();
        if given.is_empty() {
            return false;
        }
        if self.and_operator { given.iter().all(|m| *m) } else { given.contains(&true) }
    }
}

/// A project's sync windows, from the project (`spec.syncWindows`) or from Argo CD's
/// `syncwindows` answer (`windows`).
pub fn windows_of(v: &Value) -> Vec<SyncWindow> {
    v.pointer("/spec/syncWindows")
        .or_else(|| v.get("windows"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(SyncWindow::of)
        .collect()
}

/// A Go duration (`1h30m`, `45m`, `90s`, `1.5h`) in seconds; None when it doesn't read or
/// is not positive.
pub fn duration_secs(text: &str) -> Option<i64> {
    let text = text.trim();
    let mut rest = text;
    let mut total = 0f64;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let n: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let secs = match &rest[..unit] {
            "h" => 3600.,
            "m" => 60.,
            "s" => 1.,
            "ms" => 0.001,
            "us" | "µs" => 0.000_001,
            "ns" => 0.000_000_001,
            _ => return None,
        };
        total += n * secs;
        rest = &rest[unit..];
    }
    let secs = total.round() as i64;
    (secs > 0).then_some(secs)
}

/// When an Application can sync next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextSync {
    /// No window applies: any time.
    NoWindow,
    Now,
    At(Timestamp),
    /// Not within `HORIZON_DAYS`.
    NotSoon,
    /// A window that applies doesn't read, or its project couldn't be read.
    Unknown(String),
}

impl NextSync {
    /// As a row shows it.
    pub fn text(&self, now: Timestamp) -> String {
        match self {
            Self::NoWindow => "no window".into(),
            Self::Now => "now".into(),
            Self::At(t) => format!("in {}", crate::k8s::duration_text(t.as_second() - now.as_second())),
            Self::NotSoon => format!("not within {HORIZON_DAYS} days"),
            Self::Unknown(_) => "unknown".into(),
        }
    }
}

/// When a window is open, as `[start, end)` in seconds, sorted and merged.
#[derive(Debug)]
struct Openings {
    allow: bool,
    manual_sync: bool,
    spans: Vec<(i64, i64)>,
}

impl Openings {
    fn open_at(&self, t: i64) -> bool {
        let i = self.spans.partition_point(|(start, _)| *start <= t);
        i > 0 && t < self.spans[i - 1].1
    }
}

/// When `w` is open from `from` to `to` (seconds). Each step asks the schedule for one
/// time: the first opening from `from`, then the last one before it closes (a later opening
/// keeps it open longer), then the first one after: openings inside a span are jumped over,
/// so a window opening every minute for a day costs a few steps per day, not one per minute.
/// At most `MAX_STEPS`, then it says so.
fn openings(w: &SyncWindow, from: i64, to: i64) -> Result<Openings, String> {
    let cron = Cron::parse(&w.schedule).map_err(|e| format!("sync window schedule: {e}"))?;
    let duration =
        duration_secs(&w.duration).ok_or_else(|| format!("sync window duration `{}` doesn't read", w.duration))?;
    let tz = match w.time_zone.trim() {
        "" | "UTC" | "Etc/UTC" => TimeZone::UTC,
        name => TimeZone::get(name).map_err(|_| format!("sync window time zone `{name}` is unknown"))?,
    };
    let zoned = |t: i64| Timestamp::from_second(t).map(|t| t.to_zoned_datetime(tz.clone())).map_err(|e| e.to_string());
    let too_many = || format!("sync window `{}` opens too often to work out", w.schedule);
    let mut spans: Vec<(i64, i64)> = Vec::new();
    let mut steps = 0usize;
    // The first opening still open at `from`, or after it.
    let mut at = zoned(from - duration)?;
    while let Some(first) = cron.next_after(&at) {
        let start = first.timestamp().as_second();
        if start > to {
            break;
        }
        let mut end = start + duration;
        while end <= to {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(too_many());
            }
            match cron.prev_before(&zoned(end)?).map(|p| p.timestamp().as_second() + duration) {
                Some(later) if later > end => end = later,
                _ => break,
            }
        }
        match spans.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => spans.push((start, end)),
        }
        if end > to {
            break;
        }
        steps += 1;
        if steps > MAX_STEPS {
            return Err(too_many());
        }
        // Every opening before `end` is in the span: the next starts at `end` or after.
        at = zoned(end - 1)?;
    }
    Ok(Openings { allow: w.allow, manual_sync: w.manual_sync, spans })
}

/// Whether a sync can start at `t`, by Argo CD's rule (see the module).
fn can_sync(all: &[Arc<Openings>], manual: bool, t: i64) -> bool {
    let open: Vec<&Arc<Openings>> = all.iter().filter(|o| o.open_at(t)).collect();
    let denies: Vec<&&Arc<Openings>> = open.iter().filter(|o| !o.allow).collect();
    if !denies.is_empty() {
        return manual && denies.iter().all(|o| o.manual_sync);
    }
    if open.iter().any(|o| o.allow) {
        return true;
    }
    let allows: Vec<&Arc<Openings>> = all.iter().filter(|o| o.allow).collect();
    if !allows.is_empty() {
        return manual && allows.iter().any(|o| o.manual_sync);
    }
    true
}

/// Works out when Applications can sync next, as of one moment: each window's openings once,
/// and each answer once per set of windows and kind of sync, however many Applications share
/// them. Made anew when that moment is too old (the page does each minute); a project's
/// windows changing are other windows, so they are worked out anew.
pub struct Planner {
    now: Timestamp,
    openings: HashMap<SyncWindow, Result<Arc<Openings>, String>>,
    answers: HashMap<(Vec<SyncWindow>, bool), NextSync>,
}

impl Planner {
    pub fn new(now: Timestamp) -> Self {
        Self { now, openings: HashMap::new(), answers: HashMap::new() }
    }

    /// The moment its answers are for.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// When a sync can start next under `windows` (those that apply to the Application): a
    /// manual one (`manual`), or one auto-sync starts. Looked for within `HORIZON_DAYS`.
    pub fn next_sync(&mut self, windows: &[&SyncWindow], manual: bool) -> NextSync {
        if windows.is_empty() {
            return NextSync::NoWindow;
        }
        let key = (windows.iter().map(|w| (*w).clone()).collect::<Vec<_>>(), manual);
        if let Some(known) = self.answers.get(&key) {
            return known.clone();
        }
        let from = self.now.as_second();
        let to = from + HORIZON_DAYS * 86_400;
        let mut all = Vec::with_capacity(windows.len());
        let mut failed = None;
        for w in windows {
            let o = self.openings.entry((*w).clone()).or_insert_with(|| openings(w, from, to).map(Arc::new));
            match o {
                Ok(o) => all.push(o.clone()),
                Err(e) => failed = Some(e.clone()),
            }
        }
        let next = match failed {
            Some(e) => NextSync::Unknown(e),
            None => earliest(&all, manual, from, to),
        };
        self.answers.insert(key, next.clone());
        next
    }
}

/// The first time from `from` to `to` a sync can start.
fn earliest(all: &[Arc<Openings>], manual: bool, from: i64, to: i64) -> NextSync {
    if can_sync(all, manual, from) {
        return NextSync::Now;
    }
    // Whether it can sync only changes where a window opens or closes.
    let mut times: Vec<i64> =
        all.iter().flat_map(|o| o.spans.iter().flat_map(|(s, e)| [*s, *e])).filter(|t| *t > from && *t <= to).collect();
    times.sort_unstable();
    times.dedup();
    times
        .into_iter()
        .find(|t| can_sync(all, manual, *t))
        .and_then(|t| Timestamp::from_second(t).ok())
        .map_or(NextSync::NotSoon, NextSync::At)
}

/// When a sync can start next, from `now`, under `windows` (see `Planner::next_sync`).
#[cfg(test)]
pub fn next_sync(windows: &[&SyncWindow], manual: bool, now: Timestamp) -> NextSync {
    Planner::new(now).next_sync(windows, manual)
}

#[cfg(test)]
mod tests {
    use super::{NextSync, Planner, SyncWindow, duration_secs, next_sync, openings, windows_of};
    use jiff::Timestamp;
    use serde_json::json;

    fn window(kind: &str, schedule: &str, duration: &str) -> SyncWindow {
        SyncWindow::of(&json!({"kind": kind, "schedule": schedule, "duration": duration, "applications": ["*"]}))
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn durations_read_like_go() {
        assert_eq!(duration_secs("1h"), Some(3600));
        assert_eq!(duration_secs("1h30m"), Some(5400));
        assert_eq!(duration_secs("1.5h"), Some(5400));
        assert_eq!(duration_secs("90s"), Some(90));
        for bad in ["", "1", "h", "1d", "-1h", "0s"] {
            assert_eq!(duration_secs(bad), None, "{bad}");
        }
    }

    #[test]
    fn with_no_window_it_syncs_any_time() {
        assert_eq!(next_sync(&[], false, at("2026-11-24T10:00:00Z")), NextSync::NoWindow);
    }

    #[test]
    fn an_allow_window_says_when_it_opens() {
        let nightly = window("allow", "0 22 * * *", "1h");
        assert_eq!(next_sync(&[&nightly], false, at("2026-11-24T10:00:00Z")), NextSync::At(at("2026-11-24T22:00:00Z")));
        // Open now, from the start to the end of its hour.
        assert_eq!(next_sync(&[&nightly], false, at("2026-11-24T22:30:00Z")), NextSync::Now);
        assert_eq!(next_sync(&[&nightly], false, at("2026-11-24T23:00:00Z")), NextSync::At(at("2026-11-25T22:00:00Z")));
    }

    #[test]
    fn a_deny_window_blocks_until_it_closes_and_wins_over_an_allow() {
        let freeze = window("deny", "0 * * * *", "30m");
        assert_eq!(next_sync(&[&freeze], false, at("2026-11-24T10:10:00Z")), NextSync::At(at("2026-11-24T10:30:00Z")));
        assert_eq!(next_sync(&[&freeze], false, at("2026-11-24T10:40:00Z")), NextSync::Now);
        let always = window("allow", "* * * * *", "1h");
        assert_eq!(
            next_sync(&[&freeze, &always], false, at("2026-11-24T10:10:00Z")),
            NextSync::At(at("2026-11-24T10:30:00Z"))
        );
        // A deny window open every day of the horizon.
        let blackout = window("deny", "0 0 * * *", "24h");
        assert_eq!(next_sync(&[&blackout], false, at("2026-11-24T10:00:00Z")), NextSync::NotSoon);
    }

    #[test]
    fn a_manual_sync_goes_through_windows_that_allow_it() {
        let mut freeze = window("deny", "0 * * * *", "30m");
        freeze.manual_sync = true;
        let now = at("2026-11-24T10:10:00Z");
        assert_eq!(next_sync(&[&freeze], true, now), NextSync::Now);
        assert_eq!(next_sync(&[&freeze], false, now), NextSync::At(at("2026-11-24T10:30:00Z")));
        let mut nightly = window("allow", "0 22 * * *", "1h");
        assert_eq!(next_sync(&[&nightly], true, now), NextSync::At(at("2026-11-24T22:00:00Z")));
        nightly.manual_sync = true;
        assert_eq!(next_sync(&[&nightly], true, now), NextSync::Now);
    }

    #[test]
    fn a_window_that_does_not_read_is_unknown() {
        let bad = window("allow", "every day", "1h");
        assert!(matches!(next_sync(&[&bad], false, at("2026-11-24T10:00:00Z")), NextSync::Unknown(_)));
        let bad = window("allow", "0 22 * * *", "an hour");
        assert!(matches!(next_sync(&[&bad], false, at("2026-11-24T10:00:00Z")), NextSync::Unknown(_)));
        let mut bad = window("allow", "0 22 * * *", "1h");
        bad.time_zone = "Nowhere/Never".into();
        assert!(matches!(next_sync(&[&bad], false, at("2026-11-24T10:00:00Z")), NextSync::Unknown(_)));
    }

    #[test]
    fn windows_apply_by_name_namespace_or_cluster() {
        let project = json!({"spec": {"syncWindows": [
            {"kind": "deny", "schedule": "0 0 * * *", "duration": "1h", "applications": ["shop-*"]},
            {"kind": "allow", "schedule": "0 0 * * *", "duration": "1h", "namespaces": ["prod"], "clusters": ["prod-eu"], "andOperator": true},
            {"kind": "allow", "schedule": "0 0 * * *", "duration": "1h", "clusters": ["https://prod.example"]},
            {"kind": "allow", "schedule": "0 0 * * *", "duration": "1h"},
        ]}});
        let w = windows_of(&project);
        assert_eq!(w.len(), 4);
        assert!(w[0].applies("shop-prod", "x", "", ""));
        assert!(!w[0].applies("web-prod", "x", "", ""));
        // Both lists must match with the and operator.
        assert!(w[1].applies("web", "prod", "", "prod-eu"));
        assert!(!w[1].applies("web", "prod", "", "prod-us"));
        assert!(w[2].applies("web", "x", "https://prod.example", ""));
        // A window naming nothing applies to nothing.
        assert!(!w[3].applies("web", "prod", "https://prod.example", "prod"));
        assert_eq!(windows_of(&json!({"windows": [{"kind": "deny"}]})).len(), 1);
    }

    #[test]
    fn a_window_opening_every_minute_is_worked_out_in_few_steps() {
        // Open for 1000 hours from every minute: one span, not one step per minute.
        let always = window("deny", "* * * * *", "1000h");
        let now = at("2026-11-24T10:00:00Z").as_second();
        let o = openings(&always, now, now + 8 * 86_400).unwrap();
        assert_eq!(o.spans.len(), 1);
        assert!(o.spans[0].0 <= now && o.spans[0].1 > now + 8 * 86_400);
        assert_eq!(next_sync(&[&always], false, at("2026-11-24T10:00:00Z")), NextSync::NotSoon);
        // Every other minute for one minute: separate spans, each closed.
        let blink = window("deny", "*/2 * * * *", "1m");
        let o = openings(&blink, now, now + 3600).unwrap();
        assert_eq!(o.spans.len(), 31);
        assert!(o.spans.iter().all(|(s, e)| e - s == 60));
        assert_eq!(next_sync(&[&blink], false, at("2026-11-24T10:00:30Z")), NextSync::At(at("2026-11-24T10:01:00Z")));
    }

    #[test]
    fn a_window_follows_its_time_zone_across_a_clock_change() {
        if jiff::tz::TimeZone::get("Europe/Paris").is_err() {
            eprintln!("no time zone database: skipped");
            return;
        }
        let mut nine = window("allow", "0 9 * * *", "1h");
        nine.time_zone = "Europe/Paris".into();
        // 09:00 in Paris: 08:00 UTC before 2026-03-29, 07:00 after.
        assert_eq!(next_sync(&[&nine], false, at("2026-03-28T00:00:00Z")), NextSync::At(at("2026-03-28T08:00:00Z")));
        assert_eq!(next_sync(&[&nine], false, at("2026-03-29T00:00:00Z")), NextSync::At(at("2026-03-29T07:00:00Z")));
        assert_eq!(next_sync(&[&nine], false, at("2026-03-29T07:30:00Z")), NextSync::Now);
    }

    #[test]
    fn a_planner_answers_once_per_set_of_windows() {
        let nightly = window("allow", "0 22 * * *", "1h");
        let mut plan = Planner::new(at("2026-11-24T10:00:00Z"));
        let first = plan.next_sync(&[&nightly], false);
        assert_eq!(first, NextSync::At(at("2026-11-24T22:00:00Z")));
        assert_eq!(plan.next_sync(&[&nightly], false), first);
        assert_eq!((plan.openings.len(), plan.answers.len()), (1, 1));
        // A manual sync is another question; the window's openings are reused.
        plan.next_sync(&[&nightly], true);
        assert_eq!((plan.openings.len(), plan.answers.len()), (1, 2));
        assert_eq!(plan.next_sync(&[], false), NextSync::NoWindow);
    }
}
