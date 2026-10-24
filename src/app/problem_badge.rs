//! The sidebar's problem badge: problems counted across the connected clusters, in the
//! background.

use gpui_kit::*;
use kube::Client;

use super::AppView;
use crate::k8s::Kind;
use crate::problems::{Scan, Severity};

#[derive(Default)]
pub(super) struct ProblemBadge {
    /// (critical, warnings), once a scan has finished.
    counts: Option<(usize, usize)>,
    /// Kinds the scan could not read (refused or failed): the counts are partial.
    gaps: Option<SharedString>,
    _watch: Option<Task<()>>,
}

impl ProblemBadge {
    /// Count problems on `clusters`, again and again; big clusters are scanned less often
    /// (the interval grows with how long a scan takes). Replaces the running count.
    pub(super) fn start(&mut self, clusters: Vec<(String, Client)>, kinds: Vec<Kind>, cx: &mut Context<AppView>) {
        *self = Self::default();
        self._watch = Some(cx.spawn(async move |this, cx| {
            let names: Vec<String> = clusters.iter().map(|(name, _)| name.clone()).collect();
            loop {
                let started = std::time::Instant::now();
                // A cluster that may not list cluster-wide is read in its accessible namespaces.
                let scans = clusters.iter().map(|(name, c)| {
                    let accessible = crate::settings::cluster(name).namespaces;
                    let (c, kinds) = (c.clone(), &kinds);
                    async move { crate::problems::scan_namespaces(c, kinds, &accessible).await }
                });
                let results = futures::future::join_all(scans).await;
                let (counts, gaps) = summarize(&names, &results);
                if this
                    .update(cx, |this, cx| {
                        this.problems.counts = Some(counts);
                        this.problems.gaps = gaps;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                let wait = (started.elapsed() * 8).max(std::time::Duration::from_secs(120));
                cx.background_executor().timer(wait).await;
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

    pub(super) fn gaps(&self) -> Option<SharedString> {
        self.gaps.clone()
    }
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
    use super::{ProblemBadge, summarize};
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
    fn badge_shows_criticals_first() {
        let badge = |counts| ProblemBadge { counts, ..ProblemBadge::default() };
        assert_eq!(badge(None).shown(), None);
        assert_eq!(badge(Some((0, 0))).shown(), None);
        assert_eq!(badge(Some((0, 4))).shown(), Some((4, false)));
        assert_eq!(badge(Some((2, 4))).shown(), Some((2, true)));
    }
}
