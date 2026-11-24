//! The sidebar's problem badge: problems counted across the connected clusters, from the
//! scans every window, the Problems page and the agents share (`problems::shared`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::*;
use kube::Client;

use super::AppView;
use crate::k8s::Kind;
use crate::k8s::lane::Lane;
use crate::problems::shared::{self, Ask, Found, Fresh, Scope};

/// The longest the first count waits for the list on screen to show its rows.
const FIRST_COUNT_WAIT: Duration = Duration::from_secs(15);

/// How often a count that waits checks whether it may start, and the least time between two
/// counts.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// How long the badge waits at most for a scan's findings before looking again.
const LOOK_AGAIN: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct ProblemBadge {
    /// (critical, warnings), once every cluster was scanned.
    counts: Option<(usize, usize)>,
    /// Kinds the scans could not read (refused or failed): the counts are partial.
    gaps: Option<SharedString>,
    /// When the oldest of the counts was made, and how long until the next count.
    counted: Option<(Instant, Duration)>,
    counting: Option<Task<()>>,
}

impl ProblemBadge {
    /// Count problems on `clusters`, each in the namespaces it may be read in (see
    /// `Scope::home`) for the kinds it serves (`kinds`, by context), from the scans shared
    /// with the other windows, the Problems page and the agents. A cluster is scanned again
    /// once its findings are older than `shared::every` (longer for a big cluster), by
    /// whichever window asks first, and the counts follow each scan as its checks finish.
    /// Replaces the running count. The list on screen gets the API first: the first scan
    /// waits for its rows, and the badge's scans go in the background lane. A hidden window
    /// starts no scan: while every window is hidden, nothing is scanned.
    pub(super) fn start(
        &mut self,
        clusters: Vec<(String, Client)>,
        kinds: HashMap<String, Vec<Kind>>,
        cx: &mut Context<AppView>,
    ) {
        *self = Self::default();
        let scopes: Vec<(Scope, Client, Vec<Kind>)> = (clusters.into_iter())
            .map(|(name, client)| {
                let kinds = kinds.get(&name).cloned().unwrap_or_default();
                (Scope::home(&name), client, kinds)
            })
            .collect();
        self.counting = Some(cx.spawn(async move |this, cx| {
            let mut changes = shared::changes();
            let started = Instant::now();
            let mut first = true;
            loop {
                let Ok((hidden, shown)) = this.update(cx, |this, _| (this.window_hidden, this.list.rows_shown()))
                else {
                    break;
                };
                if !may_count(hidden, first.then(|| (shown, started.elapsed()))) {
                    // Nothing counted either: a hidden window's badge isn't seen.
                    cx.background_executor().timer(WAIT_POLL).await;
                    continue;
                }
                first = false;
                for (scope, client, kinds) in &scopes {
                    // Not waited for: the counts follow the scan as its checks finish.
                    let _ = shared::ensure(scope, Fresh::Paced, Lane::Background, || Ask {
                        client: client.clone(),
                        kinds: kinds.clone(),
                        record: true,
                    });
                }
                let found: Vec<(&Scope, Option<Arc<Found>>)> =
                    scopes.iter().map(|(scope, ..)| (scope, shared::latest(scope))).collect();
                let tally = tally(&found);
                if this
                    .update(cx, |this, cx| {
                        this.problems.counts = tally.counts;
                        this.problems.gaps = tally.gaps;
                        this.problems.counted = tally.counted;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                // Until some findings change, or the next scan is due. Once it is due, a scan
                // runs, and each of its checks is a change.
                let now = Instant::now();
                let wait =
                    tally.due.filter(|due| *due > now).map_or(LOOK_AGAIN, |due| due.saturating_duration_since(now));
                let timer = cx.background_executor().timer(wait.max(WAIT_POLL));
                futures::future::select(Box::pin(changes.changed()), Box::pin(timer)).await;
                // One count every `WAIT_POLL` at most: a scan read namespace by namespace has
                // thousands of checks, each a change.
                cx.background_executor().timer(WAIT_POLL).await;
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

    /// The badge's tooltip while counting is on.
    pub(super) fn tooltip(&self) -> Option<Tip> {
        self.counting.is_some().then(|| Tip { counts: self.counts, counted: self.counted, gaps: self.gaps.clone() })
    }
}

/// What the badge's tooltip says: the counts and how old they are, what they leave out, and
/// when they are counted again. Worked out when it shows, so the age is the one at that time.
pub(super) struct Tip {
    counts: Option<(usize, usize)>,
    counted: Option<(Instant, Duration)>,
    gaps: Option<SharedString>,
}

impl Tip {
    pub(super) fn text(&self) -> SharedString {
        let counted = self.counted.map(|(at, every)| (at.elapsed(), every));
        tooltip_text(self.counts, counted, self.gaps.as_deref())
    }
}

/// Whether the badge may start scans now: never while the window is hidden. `first`, for the
/// first time: whether the list on screen has shown its rows, and how long the badge has
/// waited for them; it may once they are shown, or after `FIRST_COUNT_WAIT`.
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
        return "Counting problems in the background, paused while the windows are hidden".into();
    };
    let text = |d: Duration| crate::k8s::duration_text(i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let warnings = crate::plural::count(warn, "warning");
    let mut lines = vec![format!("{crit} critical, {warnings}, counted {} ago", text(ago))];
    lines.extend(gaps.map(String::from));
    lines.push(format!("Counted again every {}, paused while the windows are hidden", text(every)));
    lines.join("\n").into()
}

/// What the badge shows of the clusters' latest findings.
struct Tally {
    /// (critical, warnings), once every cluster was scanned to the end.
    counts: Option<(usize, usize)>,
    gaps: Option<SharedString>,
    /// When the oldest findings were made, and the longest time between two scans.
    counted: Option<(Instant, Duration)>,
    /// When the next scan is due: the soonest of the clusters'.
    due: Option<Instant>,
}

/// The counts over each cluster's latest findings (by its scope), what they leave out, how
/// old they are, and when the next scan is due.
fn tally(found: &[(&Scope, Option<Arc<Found>>)]) -> Tally {
    let done: Vec<shared::Done> = found.iter().filter_map(|(_, f)| f.as_ref()?.done).collect();
    let complete = done.len() == found.len();
    let mut counts = (0, 0);
    let mut skipped = Vec::new();
    for (scope, f) in found {
        let Some(f) = f else { continue };
        let (crit, warn) = f.counts(&scope.namespaces);
        counts = (counts.0 + crit, counts.1 + warn);
        if let Some(s) = f.skipped() {
            skipped.push(format!("{}: {s}", scope.context));
        }
    }
    let gaps = (!skipped.is_empty()).then(|| format!("Partial count, could not read {}", skipped.join("; ")).into());
    let counted = done
        .iter()
        .map(|d| d.ended)
        .min()
        .map(|oldest| (oldest, done.iter().map(|d| shared::every(d.took())).max().unwrap_or_default()));
    Tally {
        counts: complete.then_some(counts),
        gaps,
        counted: counted.filter(|_| complete),
        due: done.iter().map(|d| d.due()).min(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{ProblemBadge, may_count, tally, tooltip_text};
    use crate::problems::shared::{Done, Found, Scope};
    use crate::problems::{Problem, Severity};

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
        let (a, b) = (Scope::new("a", []), Scope::new("b", []));
        let started = Instant::now();
        let done = Some(Done { started, ended: started + Duration::from_secs(10) });
        let found_a = Found::of(vec![problem(Severity::Critical), problem(Severity::Warning)], vec![], done);
        let found_b = Found::of(vec![problem(Severity::Warning)], vec!["Jobs (failed: timeout)".into()], done);
        let t = tally(&[(&a, Some(Arc::new(found_a))), (&b, Some(Arc::new(found_b)))]);
        assert_eq!(t.counts, Some((1, 2)));
        assert_eq!(t.gaps.unwrap(), "Partial count, could not read b: Jobs (failed: timeout)");
        // 8 times 10 s is less than 2 minutes.
        assert_eq!(t.counted, Some((started + Duration::from_secs(10), Duration::from_secs(120))));
        assert_eq!(t.due, Some(started + Duration::from_secs(130)));
        // A cluster still in its first scan: no counts yet.
        let running = Found::of(vec![problem(Severity::Critical)], vec![], None);
        let t = tally(&[(&a, Some(Arc::new(running))), (&b, None)]);
        assert_eq!((t.counts, t.counted, t.due), (None, None, None));
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
            "Counting problems in the background, paused while the windows are hidden"
        );
        let gaps = "Partial count, could not read b: Jobs (failed: timeout)";
        assert_eq!(
            tooltip_text(Some((2, 1)), Some((s(180), s(120))), Some(gaps)),
            format!(
                "2 critical, 1 warning, counted 3m ago\n{gaps}\nCounted again every 2m, paused while the windows are hidden"
            )
        );
        assert_eq!(
            tooltip_text(Some((0, 3)), Some((s(5), s(960))), None),
            "0 critical, 3 warnings, counted 5s ago\nCounted again every 16m, paused while the windows are hidden"
        );
        // Not counting (not connected): no tooltip.
        assert!(ProblemBadge::default().tooltip().is_none());
        // Counting: the badge's own counts, aged when the tooltip shows.
        let badge = ProblemBadge {
            counts: Some((1, 0)),
            counted: Some((Instant::now(), s(120))),
            counting: Some(gpui_kit::Task::ready(())),
            ..ProblemBadge::default()
        };
        let text = badge.tooltip().unwrap().text();
        assert!(text.starts_with("1 critical, 0 warnings, counted "), "{text}");
        assert!(text.ends_with("\nCounted again every 2m, paused while the windows are hidden"), "{text}");
    }
}
