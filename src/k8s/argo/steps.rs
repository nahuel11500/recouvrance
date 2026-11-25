//! A sync's steps (`operationState.syncResult.resources`) summed up so its cause reads first:
//! the steps that didn't succeed, by phase, then the succeeded ones (and many not started)
//! folded by kind. The sync journal and the dashboard's Last sync show the same.
//!
//! Argo CD's result says each step's phase (`syncPhase`, or a hook's type) but not its wave
//! (the `argocd.argoproj.io/sync-wave` annotation is on the live object, not in the result):
//! steps are grouped by phase only.
//!
//! A `Summary` is worked out once per state read (a journal read, a dashboard read), never per
//! frame: an operation can have thousands of steps.

use super::journal::{Seen, Step};
use crate::status::Tone;

/// What a step came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Applied, pruned, or a hook that succeeded.
    Succeeded,
    /// Failed to apply, or a hook that failed or errored.
    Failed,
    /// A hook running, or a step being terminated.
    Running,
    /// Not started yet: no result.
    Waiting,
    /// Anything else (`PruneSkipped`, `Unknown`…): ended, not as planned.
    Other,
}

impl State {
    pub fn of(s: &Step) -> Self {
        match s.result() {
            "Synced" | "Pruned" | "Succeeded" => Self::Succeeded,
            "SyncFailed" | "Failed" | "Error" => Self::Failed,
            "Running" | "Terminating" => Self::Running,
            "Pending" | "" => Self::Waiting,
            _ => Self::Other,
        }
    }

    /// Ended, whatever it came to.
    pub fn finished(self) -> bool {
        !matches!(self, Self::Running | Self::Waiting)
    }
}

/// A step's tone: a hook still running, or one not started, is on its way.
pub fn tone(s: &Step) -> Tone {
    match s.result() {
        "Running" | "Pending" | "Terminating" | "" => Tone::Busy,
        r => crate::status::tone(r).unwrap_or(Tone::Done),
    }
}

/// What a step's chip says: its result, `Waiting` before it has one.
pub fn result_text(s: &Step) -> &str {
    if s.result().is_empty() { "Waiting" } else { s.result() }
}

/// The phases in the order a sync runs them; one Argo CD doesn't know goes last.
const PHASES: [&str; 6] = ["PreSync", "Sync", "PostSync", "SyncFail", "PreDelete", "PostDelete"];

fn phase_rank(phase: &str) -> usize {
    PHASES.iter().position(|p| *p == phase).unwrap_or(PHASES.len())
}

/// The verbs `kubectl apply` ends a message with: a message of one name and one of them only
/// repeats the step (`configmap/web serverside-applied`, `web created`).
const BOILERPLATE: [&str; 9] = [
    "created",
    "configured",
    "serverside-applied",
    "unchanged",
    "pruned",
    "synced",
    "deleted",
    "applied",
    "serverside-configured",
];

/// The step's message when it says more than that the step was applied: None for
/// `configmap/web serverside-applied`, `web created`, `unchanged (dry run)`…
pub fn telling(s: &Step) -> Option<&str> {
    let m = s.message.trim();
    if m.is_empty() {
        return None;
    }
    let lower = m.to_lowercase();
    let lower = lower.strip_suffix("(server dry run)").or_else(|| lower.strip_suffix("(dry run)")).unwrap_or(&lower);
    let words: Vec<&str> = lower.split_whitespace().collect();
    let plain = match words.as_slice() {
        [verb] | [_, verb] => BOILERPLATE.contains(verb),
        _ => false,
    };
    (!plain).then_some(m)
}

/// Steps of one phase, in the order of the result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseGroup {
    pub phase: String,
    /// Indices in the steps.
    pub steps: Vec<usize>,
}

/// The folded steps of one kind, by phase, then in the order of the result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KindGroup {
    pub kind: String,
    pub steps: Vec<usize>,
}

/// Steps folded into one line, by kind, the most first: the succeeded ones, or the many not
/// started yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fold {
    /// `succeeded`, `not started`: what the line says of them, and its key to unfold.
    pub what: &'static str,
    pub count: usize,
    pub kinds: Vec<KindGroup>,
}

impl Fold {
    fn new(what: &'static str, steps: &[Step], mut picked: Vec<usize>) -> Self {
        // Stable: within a phase, in the order of the result.
        picked.sort_by_key(|&i| phase_rank(steps[i].phase()));
        let mut kinds: Vec<KindGroup> = Vec::new();
        for i in picked {
            match kinds.iter_mut().find(|k| k.kind == steps[i].kind) {
                Some(k) => k.steps.push(i),
                None => kinds.push(KindGroup { kind: steps[i].kind.clone(), steps: vec![i] }),
            }
        }
        kinds.sort_by(|a, b| b.steps.len().cmp(&a.steps.len()).then_with(|| a.kind.cmp(&b.kind)));
        Self { what, count: kinds.iter().map(|k| k.steps.len()).sum(), kinds }
    }

    /// `438 succeeded: 120 ConfigMaps, 40 Secrets, 3 Jobs, 2 Roles and 9 more kinds`.
    pub fn line(&self) -> String {
        let mut named: Vec<String> =
            self.kinds.iter().take(KINDS_NAMED).map(|k| kind_count(k.steps.len(), &k.kind)).collect();
        let more = self.kinds.len().saturating_sub(KINDS_NAMED);
        let list = if more > 0 {
            format!("{} and {}", named.join(", "), crate::plural::count(more, "more kind"))
        } else if named.len() > 1 {
            let last = named.pop().unwrap_or_default();
            format!("{} and {last}", named.join(", "))
        } else {
            named.join("")
        };
        format!("{} {}: {list}", self.count, self.what)
    }
}

/// Steps not started listed with the cause, at most: past that, they fold by kind (a sync
/// that just started has hundreds).
const WAITING_LISTED: usize = 10;

/// An operation's steps, summed up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub total: usize,
    /// Ended, whatever they came to.
    pub done: usize,
    pub succeeded: usize,
    /// The steps that didn't succeed (failed, running, skipped, and those not started when
    /// they are few), by phase in the order they run: the cause of a failed sync, what a
    /// running one waits on.
    pub cause: Vec<PhaseGroup>,
    /// The rest folded: the succeeded steps, then the many not started.
    pub folds: Vec<Fold>,
    /// The phase of the first step not finished, in the order phases run: where a running
    /// sync is, or where one that ended (a timeout) stopped.
    pub current: Option<String>,
    /// Several phases among the steps: their groups are worth naming.
    pub phased: bool,
    /// Each step's words in lowercase (result, kind, namespace/name, phase, hook), to filter
    /// them without lowering each again.
    hay: Vec<String>,
}

/// `kind` in the plural: `ConfigMaps`, `NetworkPolicies`, `Ingresses`, `Endpoints`.
pub fn kinds_word(kind: &str) -> String {
    let lower = kind.to_ascii_lowercase();
    if ["ss", "x", "ch", "sh"].iter().any(|e| lower.ends_with(e)) {
        format!("{kind}es")
    } else if lower.ends_with('s') {
        kind.to_string()
    } else if lower.ends_with('y') && !lower.ends_with("ay") && !lower.ends_with("ey") && !lower.ends_with("oy") {
        format!("{}ies", &kind[..kind.len() - 1])
    } else {
        format!("{kind}s")
    }
}

/// `n kind` in words: `1 ConfigMap`, `3 NetworkPolicies`.
pub fn kind_count(n: usize, kind: &str) -> String {
    let kind = if kind.is_empty() { "step" } else { kind };
    if n == 1 { format!("1 {kind}") } else { format!("{n} {}", kinds_word(kind)) }
}

/// Add the step `i` to its phase's group.
fn add_to(groups: &mut Vec<PhaseGroup>, phase: &str, i: usize) {
    match groups.iter_mut().find(|g| g.phase == phase) {
        Some(g) => g.steps.push(i),
        None => groups.push(PhaseGroup { phase: phase.to_string(), steps: vec![i] }),
    }
}

/// Kinds named in the folded line, at most.
const KINDS_NAMED: usize = 4;

impl Summary {
    pub fn of(steps: &[Step]) -> Self {
        let mut cause: Vec<PhaseGroup> = Vec::new();
        let mut ok: Vec<usize> = Vec::new();
        let mut waiting: Vec<usize> = Vec::new();
        let mut done = 0;
        let mut current: Option<&str> = None;
        let mut phases: Vec<&str> = Vec::new();
        let mut hay = Vec::with_capacity(steps.len());
        for (i, s) in steps.iter().enumerate() {
            let phase = s.phase();
            if !phases.contains(&phase) {
                phases.push(phase);
            }
            let state = State::of(s);
            if state.finished() {
                done += 1;
            } else if current.is_none_or(|c| phase_rank(phase) < phase_rank(c)) {
                current = Some(phase);
            }
            match state {
                State::Succeeded => ok.push(i),
                State::Waiting => waiting.push(i),
                _ => add_to(&mut cause, phase, i),
            }
            hay.push(
                format!(
                    "{} {} {}/{} {} {}",
                    result_text(s),
                    s.kind,
                    s.namespace,
                    s.name,
                    phase,
                    if s.is_hook() { "hook" } else { "" }
                )
                .to_lowercase(),
            );
        }
        let succeeded = ok.len();
        let mut folds = Vec::new();
        if !ok.is_empty() {
            folds.push(Fold::new("succeeded", steps, ok));
        }
        if waiting.len() > WAITING_LISTED {
            folds.push(Fold::new("not started", steps, waiting));
        } else {
            for i in waiting {
                add_to(&mut cause, steps[i].phase(), i);
            }
        }
        for g in &mut cause {
            g.steps.sort_unstable();
        }
        cause.sort_by_key(|g| phase_rank(&g.phase));
        Self {
            total: steps.len(),
            done,
            succeeded,
            cause,
            folds,
            current: current.map(String::from),
            phased: phases.len() > 1,
            hay,
        }
    }

    /// Steps not finished.
    pub fn unfinished(&self) -> usize {
        self.total - self.done
    }

    /// The steps whose words hold every word of `query` (lowercase), by phase: one line each,
    /// whatever they came to. All of them for an empty query.
    pub fn filter(&self, steps: &[Step], query: &str) -> Vec<PhaseGroup> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut groups: Vec<PhaseGroup> = Vec::new();
        for (i, s) in steps.iter().enumerate().take(self.hay.len()) {
            if !words.iter().all(|w| self.hay[i].contains(w.as_str())) {
                continue;
            }
            add_to(&mut groups, s.phase(), i);
        }
        groups.sort_by_key(|g| phase_rank(&g.phase));
        groups
    }
}

/// How long a step that didn't succeed ran, as this machine saw it (`seen`, from the
/// journal), in an operation `running` or that ended at `ended`, at `now` (seconds): None when
/// not known (a step seen once has no length).
pub fn how_long(s: &Step, seen: Option<Seen>, running: bool, ended: Option<i64>, now: i64) -> Option<String> {
    let seen = seen.filter(|w| w.first > 0)?;
    let span = |secs: i64| (secs > 0).then(|| super::diagnosis::span_text(secs));
    match State::of(s) {
        State::Succeeded => None,
        State::Running if running => span(now - seen.first).map(|t| format!("running for {t}")),
        State::Waiting if running => span(now - seen.first).map(|t| format!("waiting for {t}")),
        State::Running | State::Waiting => span(ended? - seen.first).map(|t| format!("not finished after {t}")),
        State::Failed | State::Other => span(seen.settled - seen.first).map(|t| format!("after {t}")),
    }
}

/// Where a sync is, or where one that ended stopped: None when every step finished.
pub fn progress(sum: &Summary, running: bool) -> Option<String> {
    let phase = sum.current.as_deref()?;
    Some(if running {
        format!("{} of {} steps done, in {phase} (Argo CD's result doesn't say the wave).", sum.done, sum.total)
    } else {
        format!(
            "It ended in {phase} with {} not finished (Argo CD's result doesn't say the wave).",
            crate::plural::count(sum.unfinished(), "step")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn step(kind: &str, name: &str, status: &str, phase: &str) -> Step {
        Step {
            kind: kind.into(),
            namespace: "shop".into(),
            name: name.into(),
            status: status.into(),
            sync_phase: phase.into(),
            ..Default::default()
        }
    }

    fn hook(name: &str, hook_phase: &str, hook_type: &str) -> Step {
        Step {
            kind: "Job".into(),
            namespace: "shop".into(),
            name: name.into(),
            hook_phase: hook_phase.into(),
            hook_type: hook_type.into(),
            ..Default::default()
        }
    }

    /// A timed-out sync: its `PreSync` hook still running leads, the succeeded steps fold by
    /// kind, the most first.
    #[test]
    fn the_cause_comes_first_and_the_rest_folds_by_kind() {
        let mut steps = vec![hook("migrate", "Running", "PreSync"), hook("seed", "Succeeded", "PreSync")];
        steps.extend((0..5).map(|i| step("ConfigMap", &format!("c{i}"), "Synced", "Sync")));
        steps.extend((0..2).map(|i| step("NetworkPolicy", &format!("n{i}"), "Synced", "Sync")));
        steps.push(step("Deployment", "web", "SyncFailed", "Sync"));
        steps.push(step("Service", "web", "", "Sync"));
        let s = Summary::of(&steps);
        assert_eq!((s.total, s.done, s.succeeded, s.unfinished()), (11, 9, 8, 2));
        assert_eq!(s.current.as_deref(), Some("PreSync"));
        assert!(s.phased);
        let cause: Vec<(&str, Vec<usize>)> = s.cause.iter().map(|g| (g.phase.as_str(), g.steps.clone())).collect();
        assert_eq!(cause, [("PreSync", vec![0]), ("Sync", vec![9, 10])]);
        let kinds: Vec<(&str, usize)> = s.folds[0].kinds.iter().map(|k| (k.kind.as_str(), k.steps.len())).collect();
        assert_eq!(kinds, [("ConfigMap", 5), ("NetworkPolicy", 2), ("Job", 1)]);
        assert_eq!(s.folds[0].line(), "8 succeeded: 5 ConfigMaps, 2 NetworkPolicies and 1 Job");
        assert_eq!(s.folds.len(), 1, "one step not started is listed with the cause");
        // A sync that just started: its many steps not started fold too.
        let fresh: Vec<Step> = (0..12).map(|i| step("Secret", &format!("s{i}"), "", "Sync")).collect();
        let f = Summary::of(&fresh);
        assert!(f.cause.is_empty());
        assert_eq!(f.folds[0].line(), "12 not started: 12 Secrets");
        let many = Summary::of(&["A", "B", "C", "D", "E", "F"].map(|k| step(k, "x", "Synced", "Sync")));
        assert_eq!(many.folds[0].line(), "6 succeeded: 1 A, 1 B, 1 C, 1 D and 2 more kinds");
        assert!(!many.phased);
    }

    #[test]
    fn a_message_that_only_repeats_the_step_is_dropped() {
        let with = |m: &str| Step { message: m.into(), ..step("ConfigMap", "web", "Synced", "Sync") };
        for m in [
            "configmap/web serverside-applied",
            "web created",
            "networkpolicy.networking.k8s.io/web configured",
            "unchanged",
            "secret synced",
            "deployment.apps/web configured (dry run)",
            "",
        ] {
            assert_eq!(telling(&with(m)), None, "{m}");
        }
        for m in [
            "one or more objects failed to apply, reason: invalid",
            "Job has reached the specified backoff limit",
            "configmap/web configured. Warning: deprecated",
        ] {
            assert_eq!(telling(&with(m)), Some(m), "{m}");
        }
    }

    #[test]
    fn kinds_read_in_the_plural() {
        let words: Vec<String> =
            ["ConfigMap", "NetworkPolicy", "Ingress", "Endpoints", "Gateway", "Patch"].map(kinds_word).to_vec();
        assert_eq!(words, ["ConfigMaps", "NetworkPolicies", "Ingresses", "Endpoints", "Gateways", "Patches"]);
        assert_eq!(kind_count(1, "Secret"), "1 Secret");
    }

    #[test]
    fn the_filter_keeps_steps_by_status_kind_or_name() {
        let steps = vec![
            hook("migrate", "Failed", "PreSync"),
            step("ConfigMap", "web-config", "Synced", "Sync"),
            step("Deployment", "web", "", "Sync"),
        ];
        let s = Summary::of(&steps);
        let kept = |q: &str| s.filter(&steps, q).into_iter().flat_map(|g| g.steps).collect::<Vec<_>>();
        assert_eq!(kept(""), [0, 1, 2]);
        assert_eq!(kept("failed"), [0]);
        assert_eq!(kept("configmap"), [1]);
        assert_eq!(kept("WEB"), [1, 2]);
        assert_eq!(kept("waiting web"), [2]);
        assert_eq!(kept("presync hook"), [0]);
    }

    /// An old journal line has no `syncPhase`: a hook's type stands for it, else Sync.
    #[test]
    fn a_step_without_its_phase_takes_its_hooks_or_sync() {
        assert_eq!(hook("m", "Running", "PostSync").phase(), "PostSync");
        assert_eq!(step("ConfigMap", "c", "Synced", "").phase(), "Sync");
        assert_eq!(step("ConfigMap", "c", "Synced", "PreSync").phase(), "PreSync");
    }

    #[test]
    fn how_long_a_step_ran_and_where_a_sync_stopped() {
        let seen = Some(Seen { first: 100, settled: 160 });
        let job = |p: &str| hook("migrate", p, "PreSync");
        assert_eq!(how_long(&job("Running"), seen, true, None, 400).as_deref(), Some("running for 5 min"));
        assert_eq!(how_long(&job(""), seen, false, Some(1900), 4000).as_deref(), Some("not finished after 30 min"));
        assert_eq!(how_long(&job("Failed"), seen, false, Some(1900), 4000).as_deref(), Some("after 60 s"));
        assert_eq!(how_long(&job("Succeeded"), seen, false, Some(1900), 4000), None);
        assert_eq!(how_long(&job("Failed"), Some(Seen { first: 100, settled: 100 }), false, None, 0), None);
        assert_eq!(how_long(&job("Failed"), None, false, None, 0), None);
        let steps = [job("Running"), step("ConfigMap", "c", "Synced", "Sync"), step("Service", "s", "", "Sync")];
        let s = Summary::of(&steps);
        assert_eq!(
            progress(&s, true).as_deref(),
            Some("1 of 3 steps done, in PreSync (Argo CD's result doesn't say the wave).")
        );
        assert_eq!(
            progress(&s, false).as_deref(),
            Some("It ended in PreSync with 2 steps not finished (Argo CD's result doesn't say the wave).")
        );
        assert_eq!(progress(&Summary::of(&steps[1..2]), false), None);
    }

    fn any_step() -> impl Strategy<Value = Step> {
        (
            prop::sample::select(vec!["ConfigMap", "Secret", "Job", "Deployment", ""]),
            "[a-z]{0,3}",
            prop::sample::select(vec!["", "Synced", "SyncFailed", "Pruned", "PruneSkipped", "Unknown"]),
            prop::sample::select(vec!["", "Running", "Succeeded", "Failed", "Error", "Pending", "Terminating"]),
            prop::sample::select(vec!["", "PreSync", "Sync", "PostSync", "SyncFail", "Skip"]),
            prop::sample::select(vec!["", "PreSync", "Sync", "PostSync", "Weird"]),
        )
            .prop_map(|(kind, name, status, hook_phase, hook_type, sync_phase)| Step {
                kind: kind.into(),
                name,
                status: status.into(),
                hook_phase: hook_phase.into(),
                hook_type: hook_type.into(),
                sync_phase: sync_phase.into(),
                ..Default::default()
            })
    }

    proptest! {
        /// Every step shows exactly once: in the cause or in its kind's folded group; the
        /// counts add up; an empty filter lists each once too.
        #[test]
        fn every_step_appears_exactly_once(steps in prop::collection::vec(any_step(), 0..200)) {
            let s = Summary::of(&steps);
            let mut seen = vec![0usize; steps.len()];
            let folded = s.folds.iter().flat_map(|f| &f.kinds).flat_map(|k| &k.steps);
            for i in s.cause.iter().flat_map(|g| &g.steps).chain(folded) {
                seen[*i] += 1;
            }
            prop_assert!(seen.iter().all(|&n| n == 1), "{:?}", seen);
            for f in &s.folds {
                prop_assert_eq!(f.count, f.kinds.iter().map(|k| k.steps.len()).sum::<usize>());
                let want = if f.what == "succeeded" { State::Succeeded } else { State::Waiting };
                for k in &f.kinds {
                    prop_assert!(k.steps.iter().all(|&i| steps[i].kind == k.kind && State::of(&steps[i]) == want));
                }
            }
            prop_assert_eq!(s.folds.iter().find(|f| f.what == "succeeded").map_or(0, |f| f.count), s.succeeded);
            for g in &s.cause {
                prop_assert!(g.steps.iter().all(|&i| steps[i].phase() == g.phase && State::of(&steps[i]) != State::Succeeded));
                prop_assert!(g.steps.is_sorted());
            }
            prop_assert_eq!(s.current.is_some(), s.unfinished() > 0);
            let mut all: Vec<usize> = s.filter(&steps, "").into_iter().flat_map(|g| g.steps).collect();
            all.sort_unstable();
            prop_assert_eq!(all, (0..steps.len()).collect::<Vec<_>>());
        }
    }
}
