//! The Sync dialog of the environment page: a sync running, or the last one, read at a glance.
//!
//! - its result, who started it, how long it took or has taken so far, against the usual
//!   duration (the median of the last deployments of the Application's history);
//! - what it waits for now, read from the operation's message (`waiting for healthy state of
//!   apps/Deployment/web`), or Argo CD's message when it failed;
//! - four phases in order, `PreSync`, `Sync`, `Rollout` and `PostSync`, each done, running, in trouble
//!   or not reached: the hooks of `PreSync` and `PostSync`, a count of the resources applied (the
//!   failed ones named), and the workloads not healthy yet with their replicas and their pods'
//!   reasons.
//!
//! Worked out once per read of the dashboard, off the UI thread, from the Application, its
//! last operation's steps and its resource tree. It reads nothing itself.

use std::collections::HashMap;

use jiff::Timestamp;
use serde_json::Value;

use super::diagnosis::Res;
use super::journal::Step;
use super::steps::{self, State};
use crate::json::{arr_at as arr, nonempty_at as s, time_at as time};
use crate::status::Tone;

/// Deployments of the history the usual duration is the median of.
pub const USUAL_OF: usize = 10;

/// The kinds of workloads whose rollout the Rollout phase follows.
const WORKLOADS: [(&str, &str); 4] =
    [("apps", "Deployment"), ("apps", "StatefulSet"), ("apps", "DaemonSet"), ("argoproj.io", "Rollout")];

/// How far up its owners a pod is followed to its workload (pod, ReplicaSet, Deployment...).
const OWNERS_UP: usize = 6;

fn is_workload(group: &str, kind: &str) -> bool {
    WORKLOADS.iter().any(|(g, k)| *g == group && *k == kind)
}

/// What the sync came to, as the dialog's header says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Syncing,
    Succeeded,
    Failed,
}

impl Outcome {
    /// Argo CD's phase of an operation (`Running`, `Terminating`, `Succeeded`, `Failed`,
    /// `Error`); one it doesn't know yet reads as running.
    pub fn of(phase: &str) -> Self {
        match phase {
            "Succeeded" => Self::Succeeded,
            "Failed" | "Error" => Self::Failed,
            _ => Self::Syncing,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Syncing => "Syncing",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Self::Syncing => Tone::Busy,
            Self::Succeeded => Tone::Good,
            Self::Failed => Tone::Bad,
        }
    }
}

/// The phases of a sync as the dialog shows them, in the order they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PhaseName {
    PreSync,
    Sync,
    /// The workloads applied becoming healthy: Argo CD waits for it in its Sync phase.
    Rollout,
    PostSync,
}

impl PhaseName {
    pub const ALL: [Self; 4] = [Self::PreSync, Self::Sync, Self::Rollout, Self::PostSync];

    pub fn title(self) -> &'static str {
        match self {
            Self::PreSync => "PreSync",
            Self::Sync => "Sync",
            Self::Rollout => "Rollout",
            Self::PostSync => "PostSync",
        }
    }

    /// The phase a hook runs in, from Argo CD's phase of its step: a `Sync` hook runs with the
    /// resources, a `SyncFail` one after a failure (shown with `PostSync`'s).
    fn of_hook(phase: &str) -> Self {
        match phase {
            "PreSync" => Self::PreSync,
            "Sync" => Self::Sync,
            _ => Self::PostSync,
        }
    }
}

/// Where a phase is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseState {
    Done,
    Running,
    /// Something in it failed, or isn't healthy.
    Problem,
    /// The sync runs and hasn't reached it yet.
    Pending,
    /// The sync ended before it.
    NotRun,
}

impl PhaseState {
    pub fn word(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Running => "running",
            Self::Problem => "problem",
            Self::Pending => "pending",
            Self::NotRun => "not run",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Self::Done => Tone::Good,
            Self::Running => Tone::Busy,
            Self::Problem => Tone::Bad,
            Self::Pending | Self::NotRun => Tone::Done,
        }
    }
}

/// A hook of the sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub res: Res,
    /// `Succeeded`, `Running`, `Failed`, `pending`…
    pub status: String,
    pub tone: Tone,
    pub message: Option<String>,
}

/// A resource that failed to apply, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    pub res: Res,
    pub message: Option<String>,
}

/// Pods of a workload in one state: `2 CrashLoopBackOff`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reason {
    pub status: String,
    pub count: usize,
    pub tone: Tone,
}

impl Reason {
    pub fn text(&self) -> String {
        format!("{} {}", self.count, self.status)
    }
}

/// A workload not healthy yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    pub res: Res,
    /// Its health: `Progressing`, `Degraded`.
    pub health: String,
    /// Bad when it or one of its pods is in trouble, else busy.
    pub tone: Tone,
    /// Replicas ready of those wanted: `3 of 5 updated replicas are available`, else its pods.
    pub ready: Option<(u32, u32)>,
    /// Its pods not ready, by state, the worst first.
    pub reasons: Vec<Reason>,
    /// Its pod in the worst state, to open.
    pub pod: Option<Res>,
    /// What Argo CD says of its health.
    pub message: Option<String>,
}

/// One phase of the sync, and what it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub name: PhaseName,
    pub state: PhaseState,
    /// Its hooks (`PreSync`, `PostSync`; `Sync`'s run with the resources).
    pub hooks: Vec<Hook>,
    /// Sync: the resources applied (or pruned), those that failed, and those not done yet.
    pub synced: usize,
    pub failed: Vec<Failed>,
    pub unfinished: usize,
    /// Rollout: the workloads not healthy yet.
    pub workloads: Vec<Workload>,
}

impl Phase {
    fn new(name: PhaseName) -> Self {
        Self {
            name,
            state: PhaseState::Pending,
            hooks: Vec::new(),
            synced: 0,
            failed: Vec::new(),
            unfinished: 0,
            workloads: Vec::new(),
        }
    }

    /// Something in it failed or isn't healthy.
    fn troubled(&self) -> bool {
        !self.failed.is_empty()
            || self.hooks.iter().any(|h| h.tone == Tone::Bad)
            || self.workloads.iter().any(|w| w.tone == Tone::Bad)
    }

    /// Something ran in it.
    fn ran(&self) -> bool {
        !self.hooks.is_empty() || self.synced > 0 || !self.failed.is_empty() || self.unfinished > 0
    }

    /// Sync's count: `58 Synced · 0 SyncFailed`, and those not done yet.
    pub fn count(&self) -> String {
        let mut out = format!("{} Synced · {} SyncFailed", self.synced, self.failed.len());
        if self.unfinished > 0 {
            out.push_str(&format!(" · {} not done", self.unfinished));
        }
        out
    }
}

/// What a running sync waits for, from its message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// `healthy state of`, `completion of hook`, `deletion of`.
    pub what: String,
    pub res: Res,
    /// Others waited for too, and what they are (`resources`, `hooks`).
    pub more: usize,
    pub more_of: String,
}

impl Waiting {
    /// `and 2 more resources`, said after it.
    pub fn more_text(&self) -> Option<String> {
        (self.more > 0).then(|| format!("and {} more {}", self.more, self.more_of))
    }
}

/// What the waits Argo CD says start with, as it words them.
const WAITS: [&str; 3] = ["healthy state of ", "completion of hook ", "deletion of "];

/// What a sync waits for, read from its operation's message: `waiting for healthy state of
/// apps/Deployment/web and 2 more resources`. The object is `group/kind/name` (the group empty
/// for the core one); its namespace isn't said.
pub fn waiting(message: &str) -> Option<Waiting> {
    let lower = message.to_ascii_lowercase();
    let at = lower.find("waiting for ")? + "waiting for ".len();
    let rest = &message[at..];
    let what = WAITS.iter().find(|w| rest.get(..w.len()).is_some_and(|p| p.eq_ignore_ascii_case(w)))?;
    let rest = &rest[what.len()..];
    let object = rest.split_whitespace().next()?;
    let mut parts = object.splitn(3, '/');
    let (group, kind, name) = (parts.next()?, parts.next()?, parts.next()?);
    let name = name.trim_end_matches(['.', ',', ';', ')']);
    if kind.is_empty() || name.is_empty() {
        return None;
    }
    let after: Vec<&str> = rest[object.len()..].split_whitespace().collect();
    let (more, more_of) = match after.as_slice() {
        ["and", n, "more", of, ..] => {
            (n.parse().unwrap_or(0), of.trim_end_matches(|c: char| !c.is_alphanumeric()).to_string())
        }
        _ => (0, String::new()),
    };
    Some(Waiting {
        what: what.trim_end().to_string(),
        res: Res { group: group.to_string(), kind: kind.to_string(), name: name.to_string(), ..Default::default() },
        more,
        more_of,
    })
}

/// Replicas ready of those wanted, from what Argo CD says of a workload's health: `3 of 5
/// updated replicas are available`, `1 out of 3 new pods have been updated`, `Deployment has 6
/// of 8 replicas available`. The first `N of M` (or `N out of M`) in it.
pub fn replicas(message: &str) -> Option<(u32, u32)> {
    let words: Vec<&str> = message.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).collect();
    for (i, w) in words.iter().enumerate() {
        let Ok(n) = w.parse::<u32>() else { continue };
        let total = match words.get(i + 1..i + 3) {
            Some(["of", m]) => m.parse::<u32>().ok(),
            _ => match words.get(i + 1..i + 4) {
                Some(["out", "of", m]) => m.parse::<u32>().ok(),
                _ => None,
            },
        };
        if let Some(m) = total.filter(|m| *m >= n) {
            return Some((n, m));
        }
    }
    None
}

/// The median of `values`; None when there are none.
pub fn median(mut values: Vec<i64>) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) { (values[mid - 1] + values[mid]) / 2 } else { values[mid] })
}

/// How long a sync of `app` usually takes: the median of its last [`USUAL_OF`] deployments'
/// durations (`deployStartedAt` to `deployedAt`), those that say both.
pub fn usual(app: &Value) -> Option<i64> {
    let took: Vec<i64> = arr(app, "/status/history")
        .iter()
        .filter_map(|h| Some(time(h, "/deployedAt")?.as_second() - time(h, "/deployStartedAt")?.as_second()))
        .filter(|secs| *secs >= 0)
        .collect();
    median(took[took.len().saturating_sub(USUAL_OF)..].to_vec())
}

/// A duration to the second: `42 s`, `4 min 12 s`, `1 h 5 min`.
pub fn clock(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => match s.rem_euclid(60) {
            0 => format!("{} min", s.div_euclid(60)),
            r => format!("{} min {r} s", s.div_euclid(60)),
        },
        s => match s.rem_euclid(3600).div_euclid(60) {
            0 => format!("{} h", s.div_euclid(3600)),
            m => format!("{} h {m} min", s.div_euclid(3600)),
        },
    }
}

/// A duration about: `~40 s`, `~5 min`, `~1 h 10 min`, rounded to the minute past one.
pub fn about(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("~{secs} s");
    }
    let mins = (secs + 30).div_euclid(60);
    if mins < 60 {
        return format!("~{mins} min");
    }
    match mins.rem_euclid(60) {
        0 => format!("~{} h", mins.div_euclid(60)),
        m => format!("~{} h {m} min", mins.div_euclid(60)),
    }
}

/// The Sync dialog's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncPhases {
    pub outcome: Outcome,
    pub by: Option<String>,
    pub started: Option<Timestamp>,
    pub finished: Option<Timestamp>,
    /// How long a sync usually takes, in seconds.
    pub usual: Option<i64>,
    /// What it waits for (or waited for, when it stopped there).
    pub waiting: Option<Waiting>,
    /// Argo CD's message, whole.
    pub message: Option<String>,
    /// `PreSync`, `Sync`, `Rollout`, `PostSync`.
    pub phases: Vec<Phase>,
}

impl SyncPhases {
    /// How long it took, or has taken so far at `now`.
    pub fn elapsed(&self, now: Timestamp) -> Option<i64> {
        let end = self.finished.unwrap_or(now);
        self.started.map(|s| (end.as_second() - s.as_second()).max(0))
    }

    /// Its time so far against the usual duration, at most 1.
    pub fn share(&self, now: Timestamp) -> Option<f32> {
        let (took, usual) = (self.elapsed(now)?, self.usual.filter(|u| *u > 0)?);
        Some((took as f32 / usual as f32).min(1.))
    }

    /// It has taken longer than usual.
    pub fn slow(&self, now: Timestamp) -> bool {
        matches!((self.elapsed(now), self.usual), (Some(took), Some(usual)) if took > usual)
    }

    /// `4 min 12 s, usually ~5 min`.
    pub fn duration_text(&self, now: Timestamp) -> Option<String> {
        let took = clock(self.elapsed(now)?);
        Some(match self.usual {
            Some(u) => format!("{took}, usually {}", about(u)),
            None => took,
        })
    }
}

#[cfg(test)]
impl SyncPhases {
    /// The phase that runs now, or the one it stopped at.
    fn current(&self) -> Option<PhaseName> {
        self.phases.iter().rev().find(|p| matches!(p.state, PhaseState::Running | PhaseState::Problem)).map(|p| p.name)
    }

    fn phase(&self, name: PhaseName) -> Option<&Phase> {
        self.phases.iter().find(|p| p.name == name)
    }
}

/// The Sync dialog of the last operation of `app`, from its steps (`syncResult.resources`, as
/// the dashboard keeps them) and its resource tree when read (else the health its status
/// says). None when it has never synced.
pub fn of(app: &Value, steps: &[Step], tree: Option<&Value>) -> Option<SyncPhases> {
    let op = app.pointer("/status/operationState")?;
    let outcome = Outcome::of(s(op, "/phase")?);
    let message = s(op, "/message").map(str::trim).filter(|m| !m.is_empty()).map(String::from);
    let mut phases: Vec<Phase> = PhaseName::ALL.iter().map(|n| Phase::new(*n)).collect();
    let at = |name: PhaseName| name as usize;
    for st in steps {
        if st.is_hook() {
            let name = PhaseName::of_hook(st.phase());
            phases[at(name)].hooks.push(hook(st));
            continue;
        }
        let p = &mut phases[at(PhaseName::Sync)];
        match State::of(st) {
            State::Succeeded => p.synced += 1,
            State::Failed => p
                .failed
                .push(Failed { res: res_of_step(st), message: Some(st.message.clone()).filter(|m| !m.is_empty()) }),
            State::Running | State::Waiting => p.unfinished += 1,
            // Skipped (`PruneSkipped`…): neither applied nor failed.
            State::Other => {}
        }
    }
    phases[at(PhaseName::Rollout)].workloads = tree.map_or_else(|| workloads_from_status(app), workloads_from_tree);
    let mut wait = message.as_deref().and_then(waiting);
    if let Some(w) = &mut wait {
        w.res.namespace = namespace_of(app, steps, &w.res);
    }
    let reached = match outcome {
        Outcome::Succeeded => None,
        Outcome::Syncing => Some(running_phase(&phases, wait.as_ref(), steps)),
        Outcome::Failed => Some(failed_phase(&phases, wait.as_ref(), steps)),
    };
    for p in &mut phases {
        p.state = match reached {
            // Ended well: the live workloads may still be rolling, or broken since.
            None if p.name == PhaseName::Rollout && p.troubled() => PhaseState::Problem,
            None if p.name == PhaseName::Rollout && !p.workloads.is_empty() => PhaseState::Running,
            None => PhaseState::Done,
            Some(r) if p.name < r => {
                if p.troubled() {
                    PhaseState::Problem
                } else {
                    PhaseState::Done
                }
            }
            Some(r) if p.name == r => {
                if p.troubled() || outcome == Outcome::Failed {
                    PhaseState::Problem
                } else {
                    PhaseState::Running
                }
            }
            Some(_) if outcome == Outcome::Failed => PhaseState::NotRun,
            Some(_) => PhaseState::Pending,
        };
    }
    Some(SyncPhases {
        outcome,
        by: op.pointer("/operation/initiatedBy").and_then(super::dashboard::who),
        started: time(op, "/startedAt"),
        finished: time(op, "/finishedAt"),
        usual: usual(app),
        waiting: wait,
        message,
        phases,
    })
}

/// The phase a hook waited for runs in: its step's.
fn hook_phase(w: &Waiting, steps: &[Step]) -> Option<PhaseName> {
    steps
        .iter()
        .find(|st| st.is_hook() && st.kind == w.res.kind && st.name == w.res.name)
        .map(|st| PhaseName::of_hook(st.phase()))
}

/// The phase a running sync is in: what it waits for says it, else the last phase that ran.
fn running_phase(phases: &[Phase], waiting: Option<&Waiting>, steps: &[Step]) -> PhaseName {
    let post_ran = phases[PhaseName::PostSync as usize].ran();
    if let Some(w) = waiting {
        match w.what.as_str() {
            "completion of hook" => {
                if let Some(p) = hook_phase(w, steps) {
                    return p;
                }
            }
            "healthy state of" if post_ran => return PhaseName::PostSync,
            "healthy state of" => return PhaseName::Rollout,
            _ => return PhaseName::Sync,
        }
    }
    last_ran(phases)
}

/// The phase a failed sync stopped at: the first with a hook or a resource that failed, else
/// the one it waited for, else the workloads when one isn't healthy, else the last that ran.
fn failed_phase(phases: &[Phase], waiting: Option<&Waiting>, steps: &[Step]) -> PhaseName {
    let failed = |p: &Phase| !p.failed.is_empty() || p.hooks.iter().any(|h| h.tone == Tone::Bad);
    if let Some(p) = phases.iter().find(|p| failed(p)) {
        return p.name;
    }
    if waiting.is_some() {
        return running_phase(phases, waiting, steps);
    }
    if phases[PhaseName::Rollout as usize].troubled() {
        return PhaseName::Rollout;
    }
    last_ran(phases)
}

/// The last phase something ran in; Sync when nothing did (the sync as a whole failed).
fn last_ran(phases: &[Phase]) -> PhaseName {
    let sync = &phases[PhaseName::Sync as usize];
    if phases[PhaseName::PostSync as usize].ran() {
        PhaseName::PostSync
    } else if sync.ran() && sync.unfinished == 0 && !phases[PhaseName::Rollout as usize].workloads.is_empty() {
        PhaseName::Rollout
    } else if sync.ran() || !phases[PhaseName::PreSync as usize].ran() {
        PhaseName::Sync
    } else {
        PhaseName::PreSync
    }
}

fn res_of_step(st: &Step) -> Res {
    Res {
        group: st.group.clone(),
        kind: st.kind.clone(),
        namespace: st.namespace.clone(),
        name: st.name.clone(),
        version: String::new(),
    }
}

fn hook(st: &Step) -> Hook {
    let status = if st.result().is_empty() { "pending".to_string() } else { st.result().to_string() };
    Hook {
        res: res_of_step(st),
        status,
        // A hook that succeeded is good, not only done.
        tone: match State::of(st) {
            State::Succeeded => Tone::Good,
            State::Failed => Tone::Bad,
            State::Running | State::Waiting => Tone::Busy,
            State::Other => steps::tone(st),
        },
        message: steps::telling(st).map(String::from),
    }
}

/// The namespace of `res`, which a sync's message doesn't say: its step's, else its entry's in
/// the Application's status, else where the Application deploys.
fn namespace_of(app: &Value, steps: &[Step], res: &Res) -> String {
    let same = |group: &str, kind: &str, name: &str| group == res.group && kind == res.kind && name == res.name;
    if let Some(st) = steps.iter().find(|st| same(&st.group, &st.kind, &st.name)) {
        return st.namespace.clone();
    }
    let status = arr(app, "/status/resources")
        .iter()
        .find(|r| same(s(r, "/group").unwrap_or(""), s(r, "/kind").unwrap_or(""), s(r, "/name").unwrap_or("")));
    if let Some(r) = status {
        return s(r, "/namespace").unwrap_or_default().to_string();
    }
    s(app, "/spec/destination/namespace").unwrap_or_default().to_string()
}

fn res_of_node(node: &Value) -> Res {
    let t = |p: &str| s(node, p).unwrap_or_default().to_string();
    Res { group: t("/group"), kind: t("/kind"), namespace: t("/namespace"), name: t("/name"), version: t("/version") }
}

/// A workload's tone from its health and its pods' reasons.
fn workload_tone(health: &str, reasons: &[Reason]) -> Tone {
    if health == "Degraded" || reasons.iter().any(|r| r.tone == Tone::Bad) { Tone::Bad } else { Tone::Busy }
}

/// A workload is followed while it is rolling out or broken.
fn followed(health: Option<&str>) -> Option<&str> {
    health.filter(|h| matches!(*h, "Progressing" | "Degraded"))
}

/// The workload `pod` is under, by its uid: up its owners in the tree.
fn workload_of<'a>(by_uid: &HashMap<&'a str, &'a Value>, pod: &'a Value) -> Option<&'a str> {
    let mut at = pod;
    for _ in 0..OWNERS_UP {
        at = arr(at, "/parentRefs").iter().find_map(|p| by_uid.get(s(p, "/uid")?).copied())?;
        if is_workload(s(at, "/group").unwrap_or(""), s(at, "/kind").unwrap_or("")) {
            return s(at, "/uid");
        }
    }
    None
}

/// The workloads of the resource tree not healthy yet, each with its pods' reasons: the
/// worst first.
fn workloads_from_tree(tree: &Value) -> Vec<Workload> {
    let nodes = arr(tree, "/nodes");
    let by_uid: HashMap<&str, &Value> = nodes.iter().filter_map(|n| Some((s(n, "/uid")?, n))).collect();
    // By workload: its pods' states.
    let mut pods: HashMap<&str, Vec<(Res, String, Tone)>> = HashMap::new();
    for n in nodes.iter().filter(|n| s(n, "/kind") == Some("Pod") && s(n, "/group").unwrap_or("").is_empty()) {
        if let Some(w) = workload_of(&by_uid, n) {
            let (status, tone) = super::dashboard::pod_state(n);
            pods.entry(w).or_default().push((res_of_node(n), status, tone));
        }
    }
    let mut out: Vec<Workload> = nodes
        .iter()
        .filter(|n| is_workload(s(n, "/group").unwrap_or(""), s(n, "/kind").unwrap_or("")))
        .filter_map(|n| {
            let health = followed(s(n, "/health/status"))?;
            let message = s(n, "/health/message").map(String::from);
            let mine = s(n, "/uid").and_then(|u| pods.get(u)).map_or(&[][..], Vec::as_slice);
            let troubled: Vec<&(Res, String, Tone)> =
                mine.iter().filter(|(_, _, t)| matches!(t, Tone::Bad | Tone::Busy)).collect();
            let mut reasons: Vec<Reason> = Vec::new();
            for (_, status, tone) in &troubled {
                match reasons.iter_mut().find(|r| r.status == *status) {
                    Some(r) => r.count += 1,
                    None => reasons.push(Reason { status: status.clone(), count: 1, tone: *tone }),
                }
            }
            reasons.sort_by_key(|r| (r.tone != Tone::Bad, std::cmp::Reverse(r.count)));
            let pod = troubled.iter().find(|(_, _, t)| *t == Tone::Bad).or(troubled.first()).map(|(r, _, _)| r.clone());
            let ready = message.as_deref().and_then(replicas).or_else(|| {
                let total = u32::try_from(mine.len()).ok().filter(|t| *t > 0)?;
                let bad = u32::try_from(troubled.len()).unwrap_or(total);
                Some((total.saturating_sub(bad), total))
            });
            Some(Workload {
                res: res_of_node(n),
                health: health.to_string(),
                tone: workload_tone(health, &reasons),
                ready,
                reasons,
                pod,
                message,
            })
        })
        .collect();
    sort_workloads(&mut out);
    out
}

/// The workloads the Application's status says aren't healthy (without its tree: no pods).
fn workloads_from_status(app: &Value) -> Vec<Workload> {
    let mut out: Vec<Workload> = arr(app, "/status/resources")
        .iter()
        .filter(|r| is_workload(s(r, "/group").unwrap_or(""), s(r, "/kind").unwrap_or("")))
        .filter_map(|r| {
            let health = followed(s(r, "/health/status"))?;
            let message = s(r, "/health/message").map(String::from);
            Some(Workload {
                res: Res::of(r)?,
                health: health.to_string(),
                tone: workload_tone(health, &[]),
                ready: message.as_deref().and_then(replicas),
                reasons: Vec::new(),
                pod: None,
                message,
            })
        })
        .collect();
    sort_workloads(&mut out);
    out
}

/// The broken first, then by name.
fn sort_workloads(w: &mut [Workload]) {
    w.sort_by(|a, b| (a.tone != Tone::Bad, &a.res.name).cmp(&(b.tone != Tone::Bad, &b.res.name)));
}

#[cfg(test)]
mod tests;
