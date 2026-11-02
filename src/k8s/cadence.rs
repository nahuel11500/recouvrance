//! When metrics-server has new numbers: how old its sample is, how often it measures, and
//! when to read it again. metrics-server measures every `--metric-resolution` (15s by
//! default, often 60s): reading it more often returns the same sample.

use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp};
use serde_json::Value;

/// Read again this long after a sample is due: metrics-server publishes a batch once it has
/// scraped every node, a little after its newest measure.
const SLACK: Duration = Duration::from_secs(3);
/// Never wait longer than this, whatever the interval seems to be.
const LONGEST: Duration = Duration::from_secs(300);

/// What one read of metrics-server says about time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// The newest measure in the list (each item has its own `timestamp`).
    pub newest: Option<Timestamp>,
    /// How long a measure covers, typically (the median `window`): about the interval.
    pub window: Option<Duration>,
    /// The API server's clock when it answered (its `Date` header), else this machine's:
    /// ages are counted on one clock.
    pub now: Timestamp,
}

/// The times of a metrics.k8s.io list's items, read at `now` (the API server's clock).
pub fn reading(items: &[Value], now: Option<Timestamp>) -> Reading {
    let mut newest: Option<Timestamp> = None;
    let mut windows = Vec::new();
    for item in items {
        if let Some(t) = item["timestamp"].as_str().and_then(|t| t.parse::<Timestamp>().ok())
            && newest.is_none_or(|n| t > n)
        {
            newest = Some(t);
        }
        if let Some(w) = item["window"].as_str().and_then(go_duration) {
            windows.push(w);
        }
    }
    let window = (!windows.is_empty()).then(|| {
        let mid = windows.len() / 2;
        *windows.select_nth_unstable(mid).1
    });
    Reading { newest, window, now: now.unwrap_or_else(Timestamp::now) }
}

/// A Go duration as metrics-server prints it: "45.5s", "1m0.025s", "500ms".
fn go_duration(text: &str) -> Option<Duration> {
    let mut rest = text;
    let mut secs = 0.0;
    while !rest.is_empty() {
        let num_len = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let num: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "µs" => 1e-6,
            "ns" => 1e-9,
            _ => return None,
        };
        rest = &rest[unit_len..];
        secs += num * scale;
    }
    (secs > 0.0 && secs.is_finite()).then(|| Duration::from_secs_f64(secs.min(LONGEST.as_secs_f64() * 10.0)))
}

/// Learns how often metrics-server measures, and says when its next sample is due.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cadence {
    /// The newest measure seen so far.
    last: Option<Timestamp>,
    /// When the previous read was answered (API server clock).
    read: Option<Timestamp>,
    every: Option<Duration>,
}

impl Cadence {
    /// How often metrics-server measures, once two samples were seen.
    pub fn every(&self) -> Option<Duration> {
        self.every
    }

    /// Take in a read; how long to wait before the next one. Never less than `min` (the
    /// `metrics_refresh` setting), never more than twice the interval or 5 minutes (or
    /// `min` when that is longer). `min` when the times say nothing: no timestamps, the
    /// same sample again past its due time, a measure stamped as it was read (an adapter
    /// that measures on request) or from the future (clocks apart).
    pub fn next(&mut self, r: &Reading, min: Duration) -> Duration {
        let previous_read = self.read.replace(r.now);
        let Some(newest) = r.newest else {
            *self = Self { read: Some(r.now), ..Self::default() };
            return min;
        };
        let age = r.now.duration_since(newest);
        let last = self.last.replace(newest);
        if age < SignedDuration::from_secs(1) {
            return min;
        }
        match last {
            Some(last) if newest > last => {
                let delta = unsigned(newest.duration_since(last));
                let gap = previous_read.map(|p| unsigned(r.now.duration_since(p)));
                // Several measures apart (a hidden list, a slow read) says how many were
                // missed, not how often: the window a measure covers says that better.
                // Without one, a long gap is only trusted when it shortens the interval
                // (a short guess corrects itself: the next read comes early and waits less).
                let learnt = match r.window {
                    Some(w) if delta > w * 3 / 2 => Some(w),
                    Some(_) => Some(delta),
                    None if gap.is_some_and(|g| g <= min.saturating_add(SLACK)) => Some(delta),
                    None if self.every.is_some_and(|e| delta < e) => Some(delta),
                    None => None,
                };
                if let Some(every) = learnt.filter(|e| !e.is_zero()) {
                    self.every = Some(every.min(LONGEST));
                }
            }
            // Earlier than before: metrics-server restarted or was replaced.
            Some(last) if newest < last => self.every = None,
            _ => {}
        }
        let Some(every) = self.every else { return min };
        // Due one interval after the newest measure, counted from the API server's now.
        let due = (every + SLACK).saturating_sub(unsigned(age));
        due.clamp(min, (every * 2).min(LONGEST).max(min))
    }
}

fn unsigned(d: SignedDuration) -> Duration {
    Duration::try_from(d).unwrap_or_default()
}

/// How old the numbers shown are, and how often metrics-server measures.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Freshness {
    /// When the newest measure was taken, on this machine's monotonic clock.
    pub measured: Option<Instant>,
    pub every: Option<Duration>,
}

impl Freshness {
    pub fn new(r: &Reading, every: Option<Duration>) -> Self {
        let measured = r.newest.and_then(|n| {
            let age = unsigned(r.now.duration_since(n));
            Instant::now().checked_sub(age)
        });
        Self { measured, every }
    }

    /// "as of 40s ago · metrics-server measures every 60s"; None when nothing is known.
    pub fn describe(&self) -> Option<String> {
        let age = self.measured.map(|m| format!("as of {} ago", short(m.elapsed())));
        let every = self.every.map(|e| format!("metrics-server measures every {}", short(e)));
        let parts: Vec<String> = [age, every].into_iter().flatten().collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// "45s", "3m", "2h".
fn short(d: Duration) -> String {
    match d.as_secs() {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s.div_euclid(60)),
        s => format!("{}h", s.div_euclid(3600)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + secs).unwrap()
    }

    fn read(newest: Option<i64>, now: i64, window: Option<u64>) -> Reading {
        Reading { newest: newest.map(at), window: window.map(Duration::from_secs), now: at(now) }
    }

    #[test]
    fn reads_times_from_a_list() {
        let items = [
            json!({"timestamp": "2027-01-15T08:00:07Z", "window": "47.5s"}),
            json!({"timestamp": "2027-01-15T08:00:27Z", "window": "1m0.025s"}),
            json!({"timestamp": "2027-01-15T08:00:12Z", "window": "1m3s"}),
            json!({"timestamp": "bad", "window": "bad"}),
            json!({}),
        ];
        let r = reading(&items, None);
        assert_eq!(r.newest, Some("2027-01-15T08:00:27Z".parse().unwrap()));
        assert_eq!(ms(r.window), Some(60_025));
        assert_eq!(reading(&[], None).newest, None);
    }

    fn ms(d: Option<Duration>) -> Option<u64> {
        d.map(|d| (d.as_secs_f64() * 1000.0).round() as u64)
    }

    #[test]
    fn go_durations() {
        assert_eq!(ms(go_duration("45.5s")), Some(45_500));
        assert_eq!(ms(go_duration("1m0.025s")), Some(60_025));
        assert_eq!(ms(go_duration("500ms")), Some(500));
        assert_eq!(ms(go_duration("1h2m")), Some(3_720_000));
        for bad in ["", "0s", "5", "s", "1x", "-1s", "1.2.3s"] {
            assert_eq!(go_duration(bad), None, "{bad}");
        }
    }

    /// metrics-server measuring every 60s, read with a 15s minimum: after two samples,
    /// each read comes just after a new sample, not four times per sample.
    #[test]
    fn follows_a_60s_resolution() {
        let min = Duration::from_secs(15);
        let mut c = Cadence::default();
        // Samples at 0, 60, 120…, published 2s after their newest measure.
        let sample_at = |now: i64| (now >= 2).then(|| (now - 2).div_euclid(60) * 60);
        let (mut now, mut reads, mut seen) = (5, 0, Vec::new());
        while now < 1200 {
            let newest = sample_at(now);
            let wait = c.next(&read(newest, now, Some(58)), min);
            if let Some(n) = newest
                && !seen.contains(&n)
            {
                seen.push(n);
            }
            reads += 1;
            now += wait.as_secs() as i64;
        }
        assert_eq!(c.every(), Some(Duration::from_secs(60)));
        // Every sample was seen, with about one read each (a few early on, learning).
        assert!(seen.len() >= 19, "{seen:?}");
        assert!(reads <= seen.len() + 4, "{reads} reads for {} samples", seen.len());
    }

    #[test]
    fn falls_back_when_times_say_nothing() {
        let min = Duration::from_secs(15);
        // No timestamps.
        assert_eq!(Cadence::default().next(&read(None, 100, None), min), min);
        // Stamped as read, every time (an adapter measuring on request).
        let mut c = Cadence::default();
        for now in [0, 15, 30, 45, 60] {
            assert_eq!(c.next(&read(Some(now), now, None), min), min);
        }
        assert_eq!(c.every(), None);
        // From the future (clocks apart).
        let mut c = Cadence::default();
        for now in [0, 60, 120] {
            assert_eq!(c.next(&read(Some(now + 30), now, Some(60)), min), min);
        }
        // The same sample, long overdue.
        let mut c = Cadence::default();
        c.next(&read(Some(0), 5, Some(60)), min);
        c.next(&read(Some(60), 65, Some(60)), min);
        assert_eq!(c.every(), Some(Duration::from_secs(60)));
        assert_eq!(c.next(&read(Some(60), 200, Some(60)), min), min);
    }

    /// A list hidden for ten minutes: the gap between samples is not the interval.
    #[test]
    fn a_long_gap_is_not_the_interval() {
        let min = Duration::from_secs(15);
        let mut c = Cadence::default();
        c.next(&read(Some(0), 5, Some(60)), min);
        c.next(&read(Some(600), 605, Some(60)), min);
        assert_eq!(c.every(), Some(Duration::from_secs(60)));
        // Without a window: not learnt from a long gap.
        let mut c = Cadence::default();
        c.next(&read(Some(0), 5, None), min);
        assert_eq!(c.next(&read(Some(600), 605, None), min), min);
        assert_eq!(c.every(), None);
        // Learnt from reads at the fast pace, then shortened by a shorter gap.
        for now in [620, 635, 650] {
            c.next(&read(Some(600), now, None), min);
        }
        c.next(&read(Some(660), 665, None), min);
        assert_eq!(c.every(), Some(Duration::from_secs(60)));
        c.next(&read(Some(690), 700, None), min);
        assert_eq!(c.every(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn describes_freshness() {
        assert_eq!(Freshness::default().describe(), None);
        let f = Freshness::new(&read(Some(0), 40, None), Some(Duration::from_secs(60)));
        let text = f.describe().unwrap();
        assert!(text.starts_with("as of 4"), "{text}");
        assert!(text.ends_with(" ago · metrics-server measures every 60s"), "{text}");
        assert_eq!(short(Duration::from_secs(150)), "2m");
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Whatever the times say, the next read is never sooner than `min`, never
            /// later than 5 minutes (or `min` when longer) or twice the interval learnt.
            #[test]
            fn next_read_stays_within_bounds(
                min in 1..900u64,
                reads in prop::collection::vec(
                    (prop::option::of(-400..4000i64), 0..4000i64, prop::option::of(0..400u64)),
                    1..40,
                ),
            ) {
                let min = Duration::from_secs(min);
                let mut c = Cadence::default();
                for (newest, now, window) in reads {
                    let wait = c.next(&read(newest, now, window), min);
                    prop_assert!(wait >= min);
                    prop_assert!(wait <= LONGEST.max(min));
                    if let Some(every) = c.every() {
                        prop_assert!(every <= LONGEST);
                        prop_assert!(wait <= (every * 2).max(min));
                    }
                }
            }

            /// Reads in order of a steady metrics-server: the interval learnt is its own.
            #[test]
            fn learns_a_steady_interval(every in 5..300i64, min in 1..60u64, lag in 1..5i64) {
                let min = Duration::from_secs(min);
                let mut c = Cadence::default();
                let mut now = lag;
                while now < 10 * every + 100 {
                    let newest = (now - lag).div_euclid(every) * every;
                    let wait = c.next(&read(Some(newest), now, Some(every as u64)), min);
                    now += wait.as_secs() as i64;
                }
                prop_assert_eq!(c.every(), Some(Duration::from_secs(every as u64)));
            }
        }
    }
}
