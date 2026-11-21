//! The sidebar's problem badge: problems counted across the connected clusters, in the
//! background.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::*;
use kube::Client;

use super::AppView;
use crate::k8s::Kind;
use crate::problems::{Scan, Severity};

/// The longest the first count waits for the list on screen to show its rows.
const FIRST_COUNT_WAIT: Duration = Duration::from_secs(15);

/// How often a count that waits checks whether it may start.
const WAIT_POLL: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct ProblemBadge {
    /// (critical, warnings), once a scan has finished.
    counts: Option<(usize, usize)>,
    /// Kinds the scan could not read (refused or failed): the counts are partial.
    gaps: Option<SharedString>,
    /// When the counts were made, and how long until the next count.
    counted: Option<(Instant, Duration)>,
    counting: Option<Task<()>>,
}

impl ProblemBadge {
    /// Count problems on `clusters`, again and again; big clusters are scanned less often
    /// (the interval grows with how long a scan takes), each for the kinds it serves
    /// (`kinds`, by context). Replaces the running count. The list on screen gets the API
    /// first: the first count waits for its rows, and every count goes in the background
    /// lane. Nothing is counted while the window is hidden; shown again, it counts at once
    /// if the last count is older than its interval.
    pub(super) fn start(
        &mut self,
        clusters: Vec<(String, Client)>,
        kinds: HashMap<String, Vec<Kind>>,
        cx: &mut Context<AppView>,
    ) {
        *self = Self::default();
        self.counting = Some(cx.spawn(async move |this, cx| {
            let names: Vec<String> = clusters.iter().map(|(name, _)| name.clone()).collect();
            let mut first = Some(Instant::now());
            loop {
                if !wait_turn(&this, first.take(), cx).await {
                    break;
                }
                let started = Instant::now();
                // A cluster that may not list cluster-wide is read in its accessible namespaces.
                let scans = clusters.iter().map(|(name, c)| {
                    let accessible = crate::settings::cluster(name).namespaces;
                    let (c, kinds) = (c.clone(), kinds.get(name).map_or_default(Vec::as_slice));
                    async move { crate::alerts::scan(name, c, kinds, &accessible).await }
                });
                let results = crate::k8s::lane::background(futures::future::join_all(scans)).await;
                let (counts, gaps) = summarize(&names, &results);
                let every = (started.elapsed() * 8).max(Duration::from_secs(120));
                if this
                    .update(cx, |this, cx| {
                        this.problems.counts = Some(counts);
                        this.problems.gaps = gaps;
                        this.problems.counted = Some((Instant::now(), every));
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(every).await;
            }
        }));
    }

    /// Forget the counts and stop counting (they were for other clusters).
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// The number on the badge and whether it counts critical problems (otherwise
    /// warnings); `None` while counting or when there is nothing to show.
    pub(super) fn shown(&self) -> Option<(usize, bool)> {
        match self.counts? {
            (0, 0) => None,
            (0, warn) => Some((warn, false)),
            (crit, _) => Some((crit, true)),
        }
    }

    /// What the badge's tooltip says while counting is on: the counts and how old they
    /// are, what they leave out, and when they are counted again.
    pub(super) fn tooltip(&self) -> Option<SharedString> {
        let counted = self.counted.map(|(at, every)| (at.elapsed(), every));
        self.counting.is_some().then(|| tooltip_text(self.counts, counted, self.gaps.as_deref()))
    }
}

/// Wait until the badge may count (see `may_count`); `first`: when the badge started, for
/// its first count. False once the window is gone.
async fn wait_turn(this: &WeakEntity<AppView>, first: Option<Instant>, cx: &mut AsyncApp) -> bool {
    loop {
        let Ok((hidden, shown)) = this.update(cx, |this, _| (this.window_hidden, this.list.rows_shown())) else {
            return false;
        };
        if may_count(hidden, first.map(|started| (shown, started.elapsed()))) {
            return true;
        }
        cx.background_executor().timer(WAIT_POLL).await;
    }
}

/// Whether the badge may count now: never while the window is hidden. `first`, for the
/// first count: whether the list on screen has shown its rows, and how long the badge has
/// waited for them; it counts once they are shown, or after `FIRST_COUNT_WAIT`.
fn may_count(hidden: bool, first: Option<(bool, Duration)>) -> bool {
    !hidden && first.is_none_or(|(shown, waited)| shown || waited >= FIRST_COUNT_WAIT)
}

/// The tooltip for `counts`, made `counted` = (how long ago, how long until the next
/// count), and the `gaps` they leave.
fn tooltip_text(
    counts: Option<(usize, usize)>,
    counted: Option<(Duration, Duration)>,
    gaps: Option<&str>,
) -> SharedString {
    let (Some((crit, warn)), Some((ago, every))) = (counts, counted) else {
        return "Counting problems in the background, paused while the window is hidden".into();
    };
    let text = |d: Duration| crate::k8s::duration_text(i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let warnings = crate::plural::count(warn, "warning");
    let mut lines = vec![format!("{crit} critical, {warnings}, counted {} ago", text(ago))];
    lines.extend(gaps.map(String::from));
    lines.push(format!("Counted again every {}, paused while the window is hidden", text(every)));
    lines.join("\n").into()
}

/// (critical, warnings) over every cluster's scan, and what the counts leave out.
fn summarize(clusters: &[String], scans: &[Scan]) -> ((usize, usize), Option<SharedString>) {
    let found = scans.iter().flat_map(|s| &s.problems);
    let crit = found.clone().filter(|p| p.severity == Severity::Critical).count();
    let warn = found.filter(|p| p.severity == Severity::Warning).count();
    let skipped: Vec<String> =
        clusters.iter().zip(scans).filter_map(|(name, s)| Some(format!("{name}: {}", s.skipped_text()?))).collect();
    let gaps = (!skipped.is_empty()).then(|| format!("Partial count, could not read {}", skipped.join("; ")).into());
    ((crit, warn), gaps)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ProblemBadge, may_count, summarize, tooltip_text};
    use crate::problems::{Problem, Scan, Severity};

    fn problem(severity: Severity) -> Problem {
        Problem {
            severity,
            kind: "pods".into(),
            label: "Pods".into(),
            namespace: Some("default".into()),
            name: "web".into(),
            reason: "CrashLoopBackOff".into(),
            age: None,
        }
    }

    #[test]
    fn counts_every_cluster_and_says_what_it_missed() {
        let names = ["a".to_string(), "b".to_string()];
        let scans = [
            Scan { problems: vec![problem(Severity::Critical), problem(Severity::Warning)], skipped: vec![] },
            Scan {
                problems: vec![problem(Severity::Warning)],
                skipped: vec![("Jobs".into(), anyhow::anyhow!("timeout"))],
            },
        ];
        let ((crit, warn), gaps) = summarize(&names, &scans);
        assert_eq!((crit, warn), (1, 2));
        let gaps = gaps.unwrap();
        assert!(gaps.starts_with("Partial count, could not read b: Jobs (failed"), "{gaps}");
        assert!(!gaps.contains("a:"), "{gaps}");
        assert_eq!(summarize(&names, &[Scan { problems: vec![], skipped: vec![] }]).1, None);
    }

    #[test]
    fn badge_shows_critical_problems_first() {
        let badge = |counts| ProblemBadge { counts, ..ProblemBadge::default() };
        assert_eq!(badge(None).shown(), None);
        assert_eq!(badge(Some((0, 0))).shown(), None);
        assert_eq!(badge(Some((0, 4))).shown(), Some((4, false)));
        assert_eq!(badge(Some((2, 4))).shown(), Some((2, true)));
    }

    #[test]
    fn counts_after_the_list_on_screen_and_never_while_hidden() {
        let s = Duration::from_secs;
        // The first count waits for the list's rows, 15 s at most.
        assert!(!may_count(false, Some((false, s(3)))));
        assert!(may_count(false, Some((true, s(3)))));
        assert!(may_count(false, Some((false, s(15)))));
        // The next ones don't wait for the list.
        assert!(may_count(false, None));
        // Never while the window is hidden, however long it waited.
        assert!(!may_count(true, None));
        assert!(!may_count(true, Some((true, s(60)))));
    }

    #[test]
    fn the_tooltip_says_how_old_the_count_is_and_that_it_pauses() {
        let s = Duration::from_secs;
        assert_eq!(
            tooltip_text(None, None, None),
            "Counting problems in the background, paused while the window is hidden"
        );
        let gaps = "Partial count, could not read b: Jobs (failed: timeout)";
        assert_eq!(
            tooltip_text(Some((2, 1)), Some((s(180), s(120))), Some(gaps)),
            format!(
                "2 critical, 1 warning, counted 3m ago\n{gaps}\nCounted again every 2m, paused while the window is hidden"
            )
        );
        assert_eq!(
            tooltip_text(Some((0, 3)), Some((s(5), s(960))), None),
            "0 critical, 3 warnings, counted 5s ago\nCounted again every 16m, paused while the window is hidden"
        );
        // Not counting (not connected): no tooltip.
        assert_eq!(ProblemBadge::default().tooltip(), None);
    }
}
