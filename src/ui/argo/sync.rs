//! Sync selected: the Applications picked (those the filter keeps) synced together, in full
//! with hooks or their resources out of sync only without hooks, with prune and dry run (see
//! `k8s::argo::grouped_sync`).
//!
//! The page reads each Application first, a few at a time: one already syncing, being
//! deleted, or held by a sync window now is left out, saying why; with prune on, what it
//! deletes on each is listed, and so are the resources out of sync and the hooks the sync
//! skips or runs. Sync… asks a second click when production environments are among them,
//! then a confirmation (their count typed, or the wait set in Settings), then sends the
//! syncs at most four at a time, write mode checked as each is sent, and follows each Application until its operation ends: its phase, message and
//! resources done, with Terminate while it runs, then how many succeeded and failed.
//!
//! Leaving the page while a sync runs only hides it: the syncs go on, and the selection bar
//! shows it again.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::dashboard::{chip, link, sync_table};
use super::grouped::{labels, some};
use super::{ArgoView, Key, key};
use crate::details::{ConfirmButtons as _, Guard, confirm_extras, guard_ok};
use crate::k8s::argo::dashboard::SyncRow;
use crate::k8s::argo::diagnosis::Res;
use crate::k8s::argo::grouped_diff::MAX_APPS;
use crate::k8s::argo::grouped_sync::{self, Checked, Options, PARALLEL, Progress, Target};
use crate::k8s::argo::ops::{self, Op, SyncRequest};
use crate::k8s::argo::windows::{NextSync, Planner};
use crate::k8s::argo::{FleetApp, short_revision};
use crate::k8s::argo::{operation_tone, steps};
use crate::status::Tone;
use crate::ui::selectable::selectable;

/// Applications whose resources out of sync can be picked one by one, at most.
const PICK_ROWS: usize = 5;
/// How often the Applications synced are read while their operations run.
const READ_EVERY: Duration = Duration::from_secs(2);
/// How often they are read while the page is hidden, or when more than `MANY` run.
const READ_SLOWLY: Duration = Duration::from_secs(10);
const MANY: usize = 50;
/// They are followed this long at most.
const FOLLOW_FOR: Duration = Duration::from_secs(30 * 60);
/// Applications named in the confirmation, at most.
const NAMED: usize = 10;
/// Resources out of sync, and hooks, named under an Application, at most (the others counted).
const RES_NAMED: usize = 8;
/// Applications synced up to this many show their resources' list unfolded.
const OPS_UNFOLDED: usize = 3;
/// Resources listed under one Application, at most.
const OPS_ROWS: usize = 300;
const TERMINATE_NOTE: &str =
    "Argo CD stops the operation running, hooks included. What it applied so far stays as it is.";

/// Every one of `res`, named (what prune deletes is never cut short).
fn all_named(res: &[Res]) -> String {
    res.iter().map(Res::label).collect::<Vec<_>>().join(", ")
}

/// The first `RES_NAMED` of `names`, the others counted.
fn some_named(names: impl IntoIterator<Item = String>) -> String {
    let names: Vec<String> = names.into_iter().collect();
    let mut s = names.iter().take(RES_NAMED).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > RES_NAMED {
        s.push_str(&format!(" and {} more", names.len() - RES_NAMED));
    }
    s
}

/// One Application of the sync.
struct Row {
    key: Key,
    label: String,
    /// Its Argo CD API server: what a terminate's confirmation names.
    source: String,
    target: Target,
    /// When a manual sync of it can start, under its project's sync windows.
    window: NextSync,
    /// What its read before the sync found.
    checked: Option<Result<Checked, String>>,
    /// Its environment's type, when it is a production one (see
    /// `environments::is_production`): the sync then asks a second click.
    production: Option<String>,
    /// Part of the sync sent.
    in_sync: bool,
    /// The answer to its sync's request; None while it is sent.
    sent: Option<Result<String, String>>,
    /// When its last operation started, as read right before its sync was written: the
    /// operation followed is a later one (an auto-sync in between is not taken for it).
    before: Option<String>,
    /// How its operation goes, once sent; an error when it couldn't be sent.
    progress: Option<Result<Progress, String>>,
    /// Why the last read while following it failed.
    note: Option<String>,
}

impl Row {
    fn ready(&self) -> Option<&Checked> {
        match &self.checked {
            Some(Ok(c)) if c.refused.is_none() => Some(c),
            _ => None,
        }
    }

    /// Its sync with `opts`, the resources `left_out` unpicked, when it is ready and has
    /// something to sync.
    fn request(&self, opts: Options, left_out: &[Res]) -> Option<SyncRequest> {
        opts.request_picked(self.ready()?, left_out)
    }

    /// Its operation ended (or it couldn't be sent).
    fn ended(&self) -> bool {
        match &self.progress {
            Some(Ok(p)) => p.ended,
            Some(Err(_)) => true,
            None => false,
        }
    }
}

/// A made-up Application whose sync is in `phase` (`Running`, `Succeeded` or `Failed`): a
/// migration hook before, two config objects in wave -1, the workloads, a check hook after
/// (the developer tools' example).
#[cfg(feature = "dev")]
fn demo_operation(ns: &str, phase: &str, now: jiff::Timestamp) -> Value {
    let running = phase == "Running";
    let res = |kind: &str, name: &str, status: &str, hook: Option<(&str, &str)>, message: &str| {
        let mut r = serde_json::json!({"group": if kind == "Deployment" { "apps" } else { "" }, "kind": kind,
            "namespace": ns, "name": name, "status": status, "message": message,
            "syncPhase": hook.map_or("Sync", |(t, _)| t)});
        if let Some((t, p)) = hook {
            r["hookType"] = t.into();
            r["hookPhase"] = p.into();
        }
        r
    };
    let mut results = vec![
        res("Job", "db-migrate", "Synced", Some(("PreSync", "Succeeded")), "job.batch/db-migrate created"),
        res("ConfigMap", "api-config", "Synced", None, "configmap/api-config configured"),
        res("Secret", "api-env", "Synced", None, "secret/api-env unchanged"),
        res("Deployment", "api", "Synced", None, "deployment.apps/api configured"),
    ];
    match phase {
        "Running" => results.push(res("Deployment", "worker", "", None, "")),
        "Failed" => {
            results.push(res("Deployment", "worker", "Synced", None, "deployment.apps/worker configured"));
            results.push(res(
                "Job",
                "smoke-test",
                "Synced",
                Some(("PostSync", "Failed")),
                "Job has reached the specified backoff limit",
            ));
        }
        _ => {
            results.push(res("Deployment", "worker", "Synced", None, "deployment.apps/worker configured"));
            results.push(res("Job", "smoke-test", "Synced", Some(("PostSync", "Succeeded")), "job completed"));
        }
    }
    let wave = |kind: &str, name: &str, w: i64| serde_json::json!({"kind": kind, "namespace": ns, "name": name, "syncWave": w});
    serde_json::json!({
        "metadata": {"name": format!("{ns}-workload"), "namespace": "argocd"},
        "status": {
            "resources": [wave("ConfigMap", "api-config", -1), wave("Secret", "api-env", -1), wave("Deployment", "worker", 1)],
            "operationState": {
                "phase": phase,
                "message": if running { "waiting for healthy state of apps/Deployment/api" } else { "" },
                "startedAt": jiff::Timestamp::from_second(now.as_second() - 90).unwrap_or(now).to_string(),
                "finishedAt": if running { Value::Null } else { now.to_string().into() },
                "syncResult": {"resources": results},
            },
        },
    })
}

/// Whether every Application to sync has a sync option on in its own spec (`(on, of)` from
/// `grouped_sync::already_on`): its checkbox then shows on, and can't be unticked (the sync
/// keeps the spec's options).
fn all_on((on, of): (usize, usize)) -> bool {
    of > 0 && on == of
}

/// A sync option's checkbox label, saying how many of the Applications to sync have it on in
/// their own spec already: `Server-side apply (already on for 1 of 3)`.
fn own_label(name: &str, (on, of): (usize, usize)) -> String {
    match on {
        0 => name.to_string(),
        _ if on == of && of == 1 => format!("{name} (already on in the Application's spec)"),
        _ if on == of => format!("{name} (already on in every Application's spec)"),
        _ => format!("{name} (already on for {on} of {of})"),
    }
}

/// What one Application's check found, before the sync, line by line (and whether each is
/// a warning): whether it is synced (`synced`: it has a sync to send with `opts`), then what
/// the sync applies, the hooks it runs or skips and what prune deletes. One left as it is
/// says so in one line: none of its hooks is skipped, nothing of it is pruned.
fn checked_lines(checked: &Checked, synced: bool, opts: Options, production: Option<&str>) -> Vec<(String, bool)> {
    let lead = match (synced, opts.hooks) {
        (true, _) => "Ready",
        (false, true) => "Nothing to sync",
        (false, false) => "Nothing out of sync: not synced",
    };
    let mut facts = vec![lead.to_string()];
    if checked.auto_sync {
        facts.push("auto-sync on".into());
    }
    if let Some(rev) = &checked.revision {
        facts.push(format!("revision {}", short_revision(rev)));
    }
    if let Some(t) = production {
        facts.push(format!("{t} environment"));
    }
    let mut lines = vec![(facts.join(" · "), false)];
    if !synced {
        return lines;
    }
    if !opts.hooks {
        let text = if checked.out_of_sync.is_empty() {
            "Nothing out of sync but what prune deletes.".to_string()
        } else {
            format!(
                "{} out of sync: {}",
                crate::plural::count(checked.out_of_sync.len(), "resource"),
                some_named(checked.out_of_sync.iter().map(Res::label))
            )
        };
        lines.push((text, false));
    }
    if !checked.hooks.is_empty() {
        let what = if opts.hooks { "Hooks that run" } else { "Hooks skipped" };
        lines.push((format!("{what}: {}", some_named(checked.hooks.iter().cloned())), !opts.hooks));
    }
    if opts.prune {
        let text = if checked.pruned.is_empty() {
            "Nothing to prune.".to_string()
        } else {
            format!("Prune deletes {}", all_named(&checked.pruned))
        };
        lines.push((text, !checked.pruned.is_empty()));
    }
    lines
}

/// The sync of the picked Applications, open on the page.
pub(super) struct GroupedSync {
    rows: Vec<Row>,
    /// Picked but hidden by the filter: not synced.
    hidden: usize,
    /// Picked past `MAX_APPS`: not synced.
    left_out: usize,
    opts: Options,
    /// The options it was sent with; None before.
    sent_with: Option<Options>,
    /// Sync… was clicked once with production environments among them: the next click goes on.
    armed: bool,
    /// The resources unpicked, by row: a sync of the resources out of sync leaves them out.
    unpicked: Vec<(usize, Res)>,
    /// Shown instead of the list (hidden while it runs, when left).
    pub(super) shown: bool,
    /// Followed too long: no longer read.
    gave_up: bool,
    checks: crate::latest::Latest,
    sends: Option<Task<()>>,
    follow: Option<Task<()>>,
    /// The rows whose resources' list was folded or unfolded by hand (see `ops_open`).
    ops_flipped: HashSet<usize>,
}

/// An Application followed: its row, where it is read, and when the operation before the one sent started.
type Follow = (usize, Target, Option<String>);

impl GroupedSync {
    /// Whether the list of resources synced of row `i` is unfolded: by default when a few
    /// Applications are synced, or when its sync failed; a click flips it.
    fn ops_open(&self, i: usize) -> bool {
        let few = self.rows.iter().filter(|r| r.in_sync).count() <= OPS_UNFOLDED;
        let failed = self.rows.get(i).is_some_and(|r| matches!(&r.progress, Some(Ok(p)) if p.ended && !p.succeeded));
        (few || failed) != self.ops_flipped.contains(&i)
    }

    /// Sent, and some of it is still going.
    pub(super) fn running(&self) -> bool {
        self.sent_with.is_some() && !self.gave_up && self.rows.iter().any(|r| r.in_sync && !r.ended())
    }

    /// The Applications of the sync.
    pub(super) fn keys(&self) -> Vec<Key> {
        self.rows.iter().map(|r| r.key.clone()).collect()
    }

    /// How many are synced and how many are still going: the selection bar's word on it.
    pub(super) fn status(&self) -> String {
        let synced: Vec<&Row> = self.rows.iter().filter(|r| r.in_sync).collect();
        let going = synced.iter().filter(|r| !r.ended()).count();
        format!("Sync of {}: {going} running", crate::plural::count(synced.len(), "Application"))
    }

    fn checking(&self) -> bool {
        self.rows.iter().any(|r| r.checked.is_none())
    }

    /// The rows the sync with `opts` goes to, with their request.
    fn jobs(&self, opts: Options) -> Vec<(usize, SyncRequest)> {
        self.rows.iter().enumerate().filter_map(|(i, r)| Some((i, r.request(opts, &self.left_out(i))?))).collect()
    }

    /// The resources of row `i` unpicked.
    fn left_out(&self, i: usize) -> Vec<Res> {
        self.unpicked.iter().filter(|(r, _)| *r == i).map(|(_, res)| res.clone()).collect()
    }

    /// Pick the resource `res` of row `i` again, or unpick it.
    fn flip_pick(&mut self, i: usize, res: Res) {
        match self.unpicked.iter().position(|(r, x)| *r == i && x.same(&res)) {
            Some(at) => {
                self.unpicked.remove(at);
            }
            None => self.unpicked.push((i, res)),
        }
        self.armed = false;
    }

    /// The production environments the sync goes to, each named once (`prod: shop-prod-acme`).
    fn production(&self) -> Vec<String> {
        let mut named: Vec<String> = self
            .jobs(self.opts)
            .iter()
            .filter_map(|(i, _)| {
                let r = &self.rows[*i];
                r.production.as_ref().map(|t| format!("{t}: {}", r.label))
            })
            .collect();
        named.dedup();
        named
    }

    /// Change the options: a second click is asked again.
    fn set_opts(&mut self, f: impl FnOnce(&mut Options)) {
        f(&mut self.opts);
        self.armed = false;
    }

    /// The Applications to follow now (sent, not ended), whether some are still sent, and
    /// whether the page is shown.
    fn to_follow(&self) -> (Vec<Follow>, bool, bool) {
        let follow = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.in_sync && matches!(r.sent, Some(Ok(_))) && !r.ended())
            .map(|(i, r)| (i, r.target.clone(), r.before.clone()))
            .collect();
        let sending = self.rows.iter().any(|r| r.in_sync && r.sent.is_none());
        (follow, sending, self.shown)
    }
}

impl ArgoView {
    /// Open the sync of the Applications picked that the filter keeps, and read each. A sync
    /// still running is shown again instead.
    pub(super) fn sync_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let apps = self.selected_apps();
        let hidden = self.picked_hidden;
        self.sync_apps(apps, hidden, window, cx);
    }

    /// Open the sync of `apps` (`hidden`: picks the filter hides, counted), and read each. A
    /// sync still running is shown again instead.
    pub(super) fn sync_apps(
        &mut self,
        apps: Vec<FleetApp>,
        hidden: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(g) = self.sync.as_mut().filter(|g| g.running()) {
            g.shown = true;
            cx.notify();
            return;
        }
        // Opened in read-only mode too: what would be synced, left out, pruned and which hooks
        // run shows; only Sync… asks to turn writes on.
        self.open_sync(apps, hidden, cx);
    }

    /// Open the sync of `apps` (`hidden`: picks the filter hides, counted), and read each.
    fn open_sync(&mut self, mut apps: Vec<FleetApp>, hidden: usize, cx: &mut Context<Self>) {
        let Some(listed) = &self.rows else { return };
        if apps.is_empty() {
            return;
        }
        let left_out = apps.len().saturating_sub(MAX_APPS);
        apps.truncate(MAX_APPS);
        let names = labels(&apps);
        let production: Vec<Option<String>> = apps.iter().map(|a| listed.production_type(&key(a))).collect();
        let mut plan = Planner::new(jiff::Timestamp::now());
        let rows = apps
            .iter()
            .zip(names)
            .zip(production)
            .map(|((a, label), production)| Row {
                key: key(a),
                label,
                source: a.server.name(),
                target: Target {
                    server: a.server.clone(),
                    instance: a.instance.clone(),
                    namespace: a.app.namespace.clone(),
                    name: a.app.name.clone(),
                },
                window: listed.fleet.sync_window(a, &mut plan, true),
                checked: None,
                production,
                in_sync: false,
                sent: None,
                before: None,
                progress: None,
                note: None,
            })
            .collect();
        self.sync = Some(GroupedSync {
            rows,
            hidden,
            left_out,
            opts: Options::default(),
            sent_with: None,
            armed: false,
            unpicked: Vec::new(),
            shown: true,
            gave_up: false,
            checks: Default::default(),
            sends: None,
            follow: None,
            ops_flipped: HashSet::new(),
        });
        self.check_grouped(cx);
        cx.notify();
    }

    /// The sync of the made-up example's Applications (`demo_list`), as their reads would
    /// find them: nothing is read or sent. `how`: `oos` for the resources out of sync only,
    /// `armed` for the second click asked with production environments among them.
    #[cfg(feature = "dev")]
    pub fn demo_sync(&mut self, how: &str, cx: &mut Context<Self>) {
        self.demo_list(true, cx);
        let Some(listed) = &self.rows else { return };
        let apps: Vec<FleetApp> = listed
            .fleet
            .apps
            .iter()
            .filter(|a| a.app.name.starts_with("shop-") && a.app.name.ends_with("-workload"))
            .cloned()
            .collect();
        let dest: Vec<(String, bool)> =
            apps.iter().map(|a| (a.dest.namespace.clone(), a.app.sync == "OutOfSync")).collect();
        self.open_sync(apps, 0, cx);
        let Some(g) = &mut self.sync else { return };
        g.checks.cancel();
        g.opts.hooks = !how.contains("oos");
        for (r, (ns, oos)) in g.rows.iter_mut().zip(dest) {
            let res = |kind: &str, name: &str| Res {
                group: if kind == "Deployment" { "apps".into() } else { String::new() },
                kind: kind.into(),
                namespace: ns.clone(),
                name: name.into(),
                ..Default::default()
            };
            r.checked = Some(Ok(Checked {
                out_of_sync: if oos {
                    vec![res("Deployment", "web"), res("ConfigMap", "web-config")]
                } else {
                    Vec::new()
                },
                hooks: vec![format!("PreSync Job {ns}/db-migrate"), format!("PostSync Job {ns}/cache-warmup")],
                revision: Some("77b324b4a0c4f1e2d3b4a5968778695a4b3c2d1e".into()),
                ..Default::default()
            }));
        }
        g.armed = how.contains("armed");
        // `-running` / `-done`: sent, and followed as Argo CD would say it (the first one
        // failed when done).
        let (running, done) = (how.contains("running"), how.contains("done"));
        if running || done {
            g.sent_with = Some(g.opts);
            let now = jiff::Timestamp::now();
            for (n, r) in g.rows.iter_mut().enumerate() {
                let ns = r.target.name.trim_end_matches("-workload").to_string();
                r.in_sync = true;
                r.sent = Some(Ok("Sync started".into()));
                let app = demo_operation(
                    &ns,
                    if done && n == 0 {
                        "Failed"
                    } else if done {
                        "Succeeded"
                    } else {
                        "Running"
                    },
                    now,
                );
                r.progress = Some(Ok(grouped_sync::progress(&app, None)));
            }
        }
        cx.notify();
    }

    /// Read each Application of the sync, at most a few at a time, and check it.
    fn check_grouped(&mut self, cx: &mut Context<Self>) {
        let Some(g) = &mut self.sync else { return };
        if g.sent_with.is_some() {
            return;
        }
        g.armed = false;
        for r in &mut g.rows {
            r.checked = None;
        }
        let targets: Vec<Target> = g.rows.iter().map(|r| r.target.clone()).collect();
        let reads = grouped_sync::bounded(targets.len(), PARALLEL, move |i| targets[i].read());
        let now = jiff::Timestamp::now();
        g.checks.spawn_each(cx, reads, move |this: &mut Self, (i, read), cx| {
            if let Some(r) = this.sync.as_mut().and_then(|g| g.rows.get_mut(i)) {
                r.checked = Some(read.map(|app| grouped_sync::check(&app, &r.window, now)));
            }
            cx.notify();
        });
    }

    /// Back to the list: a sync running is only hidden.
    fn close_sync(&mut self, cx: &mut Context<Self>) {
        if self.sync.as_ref().is_some_and(GroupedSync::running) {
            if let Some(g) = &mut self.sync {
                g.shown = false;
            }
        } else {
            self.sync = None;
        }
        cx.notify();
    }

    fn update_sync(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut GroupedSync)) {
        if let Some(g) = &mut self.sync {
            f(g);
        }
        cx.notify();
    }

    /// Read what a sync would change (the grouped diff of the same Applications): its summary
    /// shows here once read.
    fn sync_diff(&mut self, cx: &mut Context<Self>) {
        if let Some(g) = &mut self.sync {
            g.shown = false;
        }
        self.diff_selected(false, cx);
    }

    /// The grouped diff's summary, when it is of the same Applications as the sync.
    fn sync_diff_summary(&self) -> Option<String> {
        let (g, d) = (self.sync.as_ref()?, self.grouped.as_ref()?);
        (d.keys() == g.keys()).then(|| d.summary_line()).flatten()
    }

    /// Work out each Application's sync windows again as of now, from the list as last read:
    /// one a window now denies is left out (the page may have stayed open a while).
    fn recheck_windows(&mut self) {
        let now = jiff::Timestamp::now();
        let mut plan = Planner::new(now);
        let fresh: Vec<Option<NextSync>> = match (&self.sync, &self.rows) {
            (Some(g), Some(listed)) => {
                g.rows.iter().map(|r| self.find(&r.key).map(|a| listed.fleet.sync_window(a, &mut plan, true))).collect()
            }
            _ => return,
        };
        let Some(g) = &mut self.sync else { return };
        for (r, w) in g.rows.iter_mut().zip(fresh) {
            if let Some(w) = w {
                r.window = w;
            }
            if let Some(Ok(c)) = &mut r.checked
                && c.refused.is_none()
            {
                c.refused = grouped_sync::window_refusal(&r.window, now);
            }
        }
    }

    /// Confirm the sync of the Applications ready (their count typed, or a wait), then send it.
    fn confirm_grouped_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recheck_windows();
        let Some(g) = &self.sync else { return };
        if g.sent_with.is_some() || g.checking() {
            return;
        }
        let opts = g.opts;
        let jobs = g.jobs(opts);
        if jobs.is_empty() {
            return;
        }
        // Production environments among them: a first click only says so.
        if !g.armed && !g.production().is_empty() {
            self.update_sync(cx, |g| g.armed = true);
            return;
        }
        let mut servers: Vec<String> = jobs.iter().map(|(i, _)| g.rows[*i].source.clone()).collect();
        servers.sort();
        servers.dedup();
        let asked = format!(
            "{} {}",
            if g.opts.dry_run { "dry-run a sync of" } else { "sync" },
            crate::plural::count(jobs.len(), "Application")
        );
        if !crate::details::ensure_argo_writes(&asked, &servers.join(", "), window, cx) {
            return;
        }
        let resources: usize = jobs.iter().map(|(_, req)| req.resources.len()).sum();
        let skipped: Vec<String> = if opts.hooks {
            Vec::new()
        } else {
            jobs.iter()
                .filter_map(|(i, _)| {
                    let r = &g.rows[*i];
                    let c = r.ready()?;
                    (!c.hooks.is_empty()).then(|| format!("{}: {}", r.label, some_named(c.hooks.iter().cloned())))
                })
                .collect()
        };
        let n = jobs.len();
        let what = crate::plural::count(n, "Application");
        let names = some(jobs.iter().map(|(i, _)| g.rows[*i].label.clone()), NAMED);
        let left = g.rows.len() - n;
        let deletes: Vec<String> = if opts.prune {
            jobs.iter()
                .filter_map(|(i, _)| {
                    let r = &g.rows[*i];
                    let c = r.ready()?;
                    (!c.pruned.is_empty())
                        .then(|| format!("{} ({}): {}", r.label, c.pruned.len(), all_named(&c.pruned)))
                })
                .collect()
        } else {
            Vec::new()
        };
        let danger = opts.prune && !opts.dry_run && !deletes.is_empty();
        let sources = servers;
        let guard = Guard::typed_text(&n.to_string(), format!("Type {n} to sync {what}:"), window, cx);
        let view = cx.entity().downgrade();
        let title = if opts.dry_run { format!("Dry run of a sync of {what}?") } else { format!("Sync {what}?") };
        let note = format!(
            "Argo CD syncs each to the revision shown (its target revision when it can't be pinned: auto-sync on, \
             several sources), {}. {}",
            opts.text(),
            if opts.hooks {
                "The whole Application is applied, and its PreSync, Sync and PostSync hooks run.".to_string()
            } else {
                format!(
                    "Only the {} out of sync are applied (the apply strategy), named in the sync: what is in sync \
                     is left as it is, and no hook runs.",
                    crate::plural::count(resources, "resource")
                )
            }
        );
        let ok = if opts.dry_run { "Dry run" } else { "Sync" };
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (guard, view, jobs) = (guard.clone(), view.clone(), jobs.clone());
            let skipped = skipped.clone();
            let theme = cx.theme();
            let (muted, warning) = (theme.muted_foreground, theme.warning);
            let prune_text = match (opts.prune, deletes.is_empty()) {
                (false, _) => None,
                (true, true) => Some("Prune: nothing to delete.".to_string()),
                (true, false) => Some(if opts.dry_run { "Prune would delete:" } else { "Prune deletes:" }.to_string()),
            };
            alert
                .title(title.clone())
                .description(note.clone())
                .child(
                    v_flex()
                        .gap_1()
                        .pt_2()
                        .text_sm()
                        .child(selectable(names.clone()))
                        .child(div().text_xs().text_color(muted).child(format!("On {}", sources.join(", "))))
                        .when(left > 0, |v| {
                            v.child(div().text_xs().text_color(muted).child(format!(
                                "{} left out (see why on the page).",
                                crate::plural::count(left, "Application")
                            )))
                        })
                        .when(!skipped.is_empty(), |v| {
                            v.child(div().text_xs().text_color(warning).child("Hooks skipped:")).child(
                                v_flex()
                                    .id("argo-sync-skipped")
                                    .max_h(px(160.))
                                    .overflow_y_scroll()
                                    .text_xs()
                                    .text_color(muted)
                                    .children(skipped.iter().map(|d| div().child(selectable(d.clone())))),
                            )
                        })
                        .children(prune_text.map(|t| div().text_xs().text_color(warning).child(t)))
                        .when(!deletes.is_empty(), |v| {
                            v.child(
                                v_flex()
                                    .id("argo-sync-deletes")
                                    .max_h(px(220.))
                                    .overflow_y_scroll()
                                    .text_xs()
                                    .text_color(warning)
                                    .children(deletes.iter().map(|d| div().child(selectable(d.clone())))),
                            )
                        }),
                )
                .child(confirm_extras("", Some(&guard), cx))
                .confirm_buttons(ok, danger, Some(&guard))
                .on_ok(move |_, window, cx| {
                    if !guard_ok(Some(&guard), cx) {
                        return false;
                    }
                    // Asked with write mode on, but it may have been turned off since.
                    if !crate::details::writes_still_on(window, cx) {
                        return true;
                    }
                    let jobs = jobs.clone();
                    let _ = view.update(cx, |this, cx| this.send_grouped(opts, jobs, cx));
                    true
                })
        });
    }

    /// Send the syncs confirmed (`jobs`: each row and its request), at most a few at a time,
    /// then follow each Application.
    fn send_grouped(&mut self, opts: Options, jobs: Vec<(usize, SyncRequest)>, cx: &mut Context<Self>) {
        let Some(g) = &mut self.sync else { return };
        if g.sent_with.is_some() {
            return;
        }
        g.sent_with = Some(opts);
        g.checks.cancel();
        let mut list = Vec::with_capacity(jobs.len());
        let mut index = Vec::with_capacity(jobs.len());
        for (i, req) in jobs {
            if let Some(r) = g.rows.get_mut(i) {
                r.in_sync = true;
                list.push((r.target.clone(), req));
                index.push(i);
            }
        }
        let sent = grouped_sync::send_all(list);
        g.sends = Some(cx.spawn(async move |this, cx| {
            let mut sent = std::pin::pin!(sent);
            while let Some((k, result)) = sent.next().await {
                let i = index[k];
                if this.update(cx, |this, cx| this.grouped_sent(i, result, cx)).is_err() {
                    return;
                }
            }
        }));
        self.follow_grouped(cx);
        cx.notify();
    }

    fn grouped_sent(&mut self, i: usize, result: grouped_sync::Sent, cx: &mut Context<Self>) {
        if let Some(r) = self.sync.as_mut().and_then(|g| g.rows.get_mut(i)) {
            match result {
                Ok((said, before)) => {
                    r.before = before;
                    r.sent = Some(Ok(said));
                }
                Err(e) => {
                    r.progress = Some(Err(e.clone()));
                    r.sent = Some(Err(e));
                }
            }
        }
        cx.notify();
    }

    /// Read the Applications synced every `READ_EVERY` until each operation ends (or for
    /// `FOLLOW_FOR`), a few at a time; each read goes to the sync journal.
    fn follow_grouped(&mut self, cx: &mut Context<Self>) {
        let Some(g) = &mut self.sync else { return };
        g.follow = Some(cx.spawn(async move |this, cx| {
            let start = Instant::now();
            let exec = cx.background_executor().clone();
            let mut wait = READ_EVERY;
            loop {
                exec.timer(wait).await;
                let Ok(Some((todo, sending, shown))) =
                    this.update(cx, |this, _| this.sync.as_ref().map(GroupedSync::to_follow))
                else {
                    return;
                };
                // Hidden, or many: read less often (each read is a whole Application).
                wait = if shown && todo.len() <= MANY { READ_EVERY } else { READ_SLOWLY };
                if todo.is_empty() && !sending {
                    return;
                }
                if start.elapsed() > FOLLOW_FOR {
                    let _ = this.update(cx, |this, cx| this.update_sync(cx, |g| g.gave_up = true));
                    return;
                }
                let reads: Vec<(usize, Result<Value, String>)> =
                    grouped_sync::bounded(todo.len(), PARALLEL, |k| todo[k].1.read()).collect().await;
                let read = this.update(cx, |this, cx| {
                    let Some(g) = &mut this.sync else { return };
                    for (k, read) in reads {
                        let (i, _, before) = &todo[k];
                        let Some(r) = g.rows.get_mut(*i) else { continue };
                        match read {
                            Ok(app) => {
                                r.progress = Some(Ok(grouped_sync::progress(&app, before.as_deref())));
                                r.note = None;
                            }
                            Err(e) => r.note = Some(format!("could not be read: {e}")),
                        }
                    }
                    cx.notify();
                });
                if read.is_err() {
                    return;
                }
            }
        }));
    }

    /// Confirm, then terminate the operation of the Application at `i`, the way it was read.
    fn terminate_grouped(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.sync.as_ref().and_then(|g| g.rows.get(i)) else { return };
        let (target, label, source) = (r.target.clone(), r.label.clone(), r.source.clone());
        if !crate::details::ensure_argo_writes(&format!("terminate the sync of {label}"), &source, window, cx) {
            return;
        }
        let guard = Guard::protected(&source, window, cx);
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (target, guard) = (target.clone(), guard.clone());
            alert
                .title(format!("Terminate the sync of {label}?"))
                .description(TERMINATE_NOTE)
                .child(confirm_extras("", guard.as_ref(), cx))
                .confirm_buttons("Terminate", true, guard.as_ref())
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if !crate::details::writes_still_on(window, cx) {
                        return true;
                    }
                    let send = target.send(Op::Terminate);
                    window
                        .spawn(cx, async move |cx| {
                            let result = send.await;
                            let _ = cx.update(|window, cx| match result {
                                Ok((msg, _)) => window.push_notification(Notification::success(msg), cx),
                                Err(e) => {
                                    window.push_notification(Notification::error(format!("Terminate failed: {e}")), cx);
                                }
                            });
                        })
                        .detach();
                    true
                })
        });
    }

    // MARK: Rendering

    /// The sync page: the options, each Application with what its check found (and, once
    /// sent, how its operation goes), and the summary once all ended.
    pub(super) fn sync_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = &self.sync else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let sent = g.sent_with.is_some();
        let opts = g.sent_with.unwrap_or(g.opts);
        let checking = g.checking();
        let ready = g.jobs(opts).len();
        let production = if sent { Vec::new() } else { g.production() };
        let n = g.rows.len();
        let title = if sent {
            format!(
                "{} of {}",
                if opts.dry_run { "Dry run" } else { "Sync" },
                crate::plural::count(g.rows.iter().filter(|r| r.in_sync).count(), "Application")
            )
        } else {
            format!("Sync {}", crate::plural::count(n, "Application"))
        };
        let header = h_flex()
            .gap_2()
            .child(
                Button::new("argo-sync-back")
                    .small()
                    .ghost()
                    .icon(Lucide::ArrowLeft)
                    .label("Applications")
                    .on_click(cx.listener(|this, _, _, cx| this.close_sync(cx))),
            )
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().flex_1())
            .when(checking, |h| {
                let read = g.rows.iter().filter(|r| r.checked.is_some()).count();
                h.child(div().text_xs().text_color(muted).child(format!("Read {read} of {n}…")))
            })
            .when(!sent, |h| {
                h.child(
                    Button::new("argo-sync-check")
                        .small()
                        .outline()
                        .icon(Lucide::RefreshCw)
                        .label("Read again")
                        .loading(checking)
                        .disabled(checking)
                        .on_click(cx.listener(|this, _, _, cx| this.check_grouped(cx))),
                )
                .child(
                    Button::new("argo-sync-go")
                        .small()
                        .when(!g.armed, ButtonVariants::primary)
                        .when(g.armed, ButtonVariants::danger)
                        .icon(Lucide::GitBranch)
                        .label(if g.armed {
                            format!("Confirm: {}…", crate::plural::count(production.len(), "production Application"))
                        } else if opts.dry_run {
                            "Dry run…".to_string()
                        } else {
                            "Sync…".to_string()
                        })
                        .disabled(checking || ready == 0)
                        .on_click(cx.listener(|this, _, window, cx| this.confirm_grouped_sync(window, cx))),
                )
            })
            .when(sent, |h| {
                h.child(
                    Button::new("argo-sync-journal")
                        .small()
                        .outline()
                        .icon(Lucide::ScrollText)
                        .label("Journal")
                        .on_click(cx.listener(|this, _, window, cx| this.open_journal(None, window, cx))),
                )
            });
        let view = cx.entity().downgrade();
        let options = (!sent).then(|| {
            // What the Applications to sync already have on in their own spec: kept by the sync
            // whatever is ticked (see `ops::sync_options`).
            let own = |option: &str| grouped_sync::already_on(g.rows.iter().filter_map(Row::ready), option);
            let (server_side_on, replace_on) = (own(ops::SERVER_SIDE), own(ops::REPLACE));
            let set = |f: fn(&mut Options, bool)| {
                let view = view.clone();
                move |v: &bool, _: &mut Window, cx: &mut App| {
                    let v = *v;
                    let _ = view.update(cx, |this, cx| this.update_sync(cx, |g| g.set_opts(|o| f(o, v))));
                }
            };
            let hooks = |id: &'static str, label: &'static str, on: bool| {
                Button::new(id)
                    .small()
                    .when(opts.hooks == on, ButtonVariants::primary)
                    .when(opts.hooks != on, Button::outline)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.update_sync(cx, |g| g.set_opts(|o| o.hooks = on))))
            };
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(hooks("argo-sync-hooks", "Full, with hooks", true))
                        .child(hooks("argo-sync-no-hooks", "Out of sync only, without hooks", false))
                        .child(div().text_xs().text_color(muted).child(if opts.hooks {
                            "Argo CD's own Sync: the whole Application, and its PreSync, Sync and PostSync hooks run \
                             (a migration, a secret refresh…)."
                        } else {
                            "Only the resources out of sync, named one by one, with the apply strategy: what is in \
                             sync is not applied again and no hook runs (those skipped are listed). For a simple change."
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Checkbox::new("argo-sync-prune")
                                .label("Prune")
                                .checked(opts.prune)
                                .on_click(set(|o, v| o.prune = v)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if opts.prune { theme.warning } else { muted })
                                .child("Deletes what is no longer in git: listed under each Application."),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Checkbox::new("argo-sync-dry-run")
                                .label("Dry run")
                                .checked(opts.dry_run)
                                .on_click(set(|o, v| o.dry_run = v)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Argo CD applies nothing and says what it would do."),
                        ),
                )
                .child(
                    h_flex()
                        .gap_4()
                        .flex_wrap()
                        .child(
                            Checkbox::new("argo-sync-force")
                                .label("Force")
                                .checked(opts.force)
                                .on_click(set(|o, v| o.force = v)),
                        )
                        .child(
                            Checkbox::new("argo-sync-server-side")
                                .label(own_label("Server-side apply", server_side_on))
                                .checked(opts.server_side || all_on(server_side_on))
                                .disabled(all_on(server_side_on))
                                .on_click(set(|o, v| o.server_side = v)),
                        )
                        .child(
                            Checkbox::new("argo-sync-replace")
                                .label(own_label("Replace", replace_on))
                                .checked(opts.replace || all_on(replace_on))
                                .disabled(all_on(replace_on))
                                .on_click(set(|o, v| o.replace = v)),
                        )
                        .child(div().text_xs().text_color(if opts.force || opts.replace { theme.warning } else { muted }).child(
                            "Force deletes and creates again what can't be patched; Replace uses kubectl replace \
                             instead of apply; server-side apply lets the API server merge.",
                        )),
                )
        });
        let diff = match self.sync_diff_summary() {
            Some(s) => div().text_sm().font_weight(FontWeight::MEDIUM).child(selectable(s)),
            None => div().child(
                link("argo-sync-diff", "Read what a sync of each would change", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.sync_diff(cx))),
            ),
        };
        let tallied: Vec<Result<Progress, String>> =
            g.rows.iter().filter(|r| r.in_sync).filter_map(|r| r.progress.clone()).collect();
        let (ok, failed, _) = grouped_sync::tally(tallied.iter());
        let in_sync = g.rows.iter().filter(|r| r.in_sync).count();
        let done = sent && g.rows.iter().filter(|r| r.in_sync).all(Row::ended);
        let summary = sent.then(|| {
            let left = n - in_sync;
            let mut s = if done {
                format!("Done: {ok} succeeded, {failed} failed")
            } else {
                format!("{ok} succeeded, {failed} failed, {} running", in_sync - ok - failed)
            };
            if left > 0 {
                s.push_str(&format!(", {left} left out"));
            }
            let tone = if failed > 0 {
                Tone::Bad
            } else if done {
                Tone::Good
            } else {
                Tone::Busy
            };
            div().text_sm().font_weight(FontWeight::MEDIUM).text_color(tone.color(cx)).child(s)
        });
        let body = v_flex()
            .gap_2()
            .when(!sent, |v| {
                v.child(div().text_xs().text_color(muted).child(
                    "Each Application is read first: one already syncing, being deleted, or held by a sync window now \
                     is left out, saying why. The syncs are sent four at a time, write mode checked as each is sent.",
                ))
            })
            .when(!sent && !crate::details::writes_enabled(cx), |v| {
                v.child(div().text_xs().text_color(theme.warning).child(
                    "Read-only: this shows what the sync would do (and the link below, what it would change). \
                     Sync… asks to turn writes on for it.",
                ))
            })
            .when(!production.is_empty(), |v| {
                v.child(div().text_xs().text_color(theme.danger).child(selectable(format!(
                    "Includes {} of production environments: {}{}",
                    crate::plural::count(production.len(), "Application"),
                    production.iter().take(NAMED).cloned().collect::<Vec<_>>().join(", "),
                    if production.len() > NAMED {
                        format!(" and {} more", production.len() - NAMED)
                    } else {
                        String::new()
                    },
                ))))
            })
            .when(g.hidden > 0, |v| {
                v.child(div().text_xs().text_color(theme.warning).child(format!(
                    "{} more picked are hidden by the filter: not synced.",
                    crate::plural::count(g.hidden, "Application")
                )))
            })
            .when(g.left_out > 0, |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!("{} more picked are left out: at most {MAX_APPS} at a time.", g.left_out)),
                )
            })
            .children(options)
            .when(!sent && !checking && !opts.hooks, |v| {
                let jobs = g.jobs(opts);
                let resources: usize = jobs.iter().map(|(_, req)| req.resources.len()).sum();
                let idle = g.rows.iter().filter(|r| r.ready().is_some()).count() - jobs.len();
                let mut text = format!(
                    "{} out of sync on {}, without hooks",
                    crate::plural::count(resources, "resource"),
                    crate::plural::count(jobs.len(), "Application")
                );
                if idle > 0 {
                    text.push_str(&format!("; {idle} with nothing out of sync left as they are"));
                }
                v.child(div().text_sm().child(text))
            })
            .when(!sent, |v| v.child(diff))
            .children(summary)
            .when(g.gave_up, |v| {
                v.child(div().text_xs().text_color(theme.warning).child(format!(
                    "No longer followed after {} min: the journal keeps what Argo CD says of them.",
                    FOLLOW_FOR.as_secs() / 60
                )))
            })
            .children(g.rows.iter().enumerate().map(|(i, r)| self.sync_row(g, i, r, opts, cx)));
        v_flex()
            .size_full()
            .gap_2()
            .child(header)
            .child(div().id("argo-sync-body").flex_1().min_h_0().overflow_y_scroll().p_1().child(body))
            .into_any_element()
    }

    /// One Application: before the sync, what its check found; after, how it goes.
    fn sync_row(&self, g: &GroupedSync, i: usize, r: &Row, opts: Options, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let sent = g.sent_with.is_some();
        let line = |text: String, color: Hsla| div().text_xs().text_color(color).child(selectable(text));
        let mut out = v_flex().gap_0p5().py_1().border_b_1().border_color(theme.border).child(
            h_flex()
                .gap_2()
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(selectable(r.label.clone())))
                .child(div().text_xs().text_color(muted).child(r.source.clone())),
        );
        let checked = match &r.checked {
            None => return out.child(line("Reading…".into(), muted)),
            Some(Err(e)) => return out.child(line(format!("Could not be read: {e}"), theme.danger)),
            Some(Ok(c)) => c,
        };
        if let Some(why) = &checked.refused {
            return out.child(line(format!("Left out: {why}"), theme.warning));
        }
        if !sent || !r.in_sync {
            let synced = r.request(opts, &g.left_out(i)).is_some();
            for (text, warn) in checked_lines(checked, synced, opts, r.production.as_deref()) {
                out = out.child(line(text, if warn { theme.warning } else { muted }));
            }
            // A few Applications: each resource out of sync can be left out (Argo CD's
            // selective sync).
            if !sent && !opts.hooks && g.rows.len() <= PICK_ROWS {
                let mut pickable: Vec<Res> = checked.out_of_sync.clone();
                if opts.prune {
                    pickable.extend(checked.pruned.iter().cloned());
                }
                let left_out = g.left_out(i);
                let view = cx.entity().downgrade();
                out = out.child(h_flex().flex_wrap().gap_x_3().gap_y_0p5().pl_2().children(
                    pickable.into_iter().enumerate().map(|(n, res)| {
                        let picked = !left_out.iter().any(|l| l.same(&res));
                        let view = view.clone();
                        Checkbox::new(SharedString::from(format!("argo-sync-pick-{i}-{n}")))
                            .label(res.label())
                            .checked(picked)
                            .on_click(move |_: &bool, _, cx| {
                                let res = res.clone();
                                let _ = view.update(cx, |this, cx| this.update_sync(cx, |g| g.flip_pick(i, res)));
                            })
                    }),
                ));
            }
            return out;
        }
        let progress = match (&r.sent, &r.progress) {
            (None, _) => return out.child(line("Sending…".into(), muted)),
            (_, Some(Err(e))) => return out.child(line(format!("Failed: {e}"), theme.danger)),
            (Some(Ok(said)), None) => return out.child(line(said.clone(), muted)),
            (_, Some(Ok(p))) => p,
            (Some(Err(e)), None) => return out.child(line(format!("Failed: {e}"), theme.danger)),
        };
        let tone = if progress.phase == "Waiting" { Tone::Busy } else { operation_tone(&progress.phase) };
        let steps = (progress.total > 0).then(|| format!("{} of {} done", progress.done, progress.total));
        out = out.child(
            h_flex()
                .gap_2()
                .child(chip(&progress.phase, tone, cx))
                .children(steps.map(|s| div().text_xs().text_color(muted).child(s)))
                .child(div().flex_1())
                // Waiting too: Argo CD's controller may hold the operation written (a sync
                // window it waits for), and terminating it is how to take it back.
                .when(!progress.ended, |h| {
                    h.child(
                        Button::new(("argo-sync-terminate", i))
                            .xsmall()
                            .outline()
                            .icon(Lucide::CircleStop)
                            .label("Terminate")
                            .on_click(cx.listener(move |this, _, window, cx| this.terminate_grouped(i, window, cx))),
                    )
                }),
        );
        if !progress.ended && progress.total > 0 {
            let share = progress.done as f32 / progress.total as f32;
            out = out.child(
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(theme.muted)
                    .child(div().h_full().w(relative(share)).rounded_full().bg(theme.warning)),
            );
        }
        if let Some(m) = &progress.message {
            let color = if progress.ended && !progress.succeeded { theme.danger } else { muted };
            out = out.child(line(m.clone(), color));
        }
        if let Some(note) = &r.note {
            out = out.child(line(note.clone(), muted));
        }
        // What it is doing: its resources, by wave, with their status, live.
        if !progress.rows.is_empty() {
            let open = g.ops_open(i);
            let failed = progress.rows.iter().filter(|r| steps::tone(&r.step) == Tone::Bad).count();
            let running = progress.rows.iter().filter(|r| steps::tone(&r.step) == Tone::Busy).count();
            let mut what = vec![crate::plural::count(progress.rows.len(), "resource")];
            if running > 0 {
                what.push(format!("{running} running"));
            }
            if failed > 0 {
                what.push(format!("{failed} failed"));
            }
            let label = format!("{} {}", if open { "▾" } else { "▸" }, what.join(" · "));
            out = out.child(h_flex().child(link(("argo-sync-ops", i), &label, cx).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.update_sync(cx, |g| {
                        if !g.ops_flipped.remove(&i) {
                            g.ops_flipped.insert(i);
                        }
                    });
                },
            ))));
            if open {
                let rows: Vec<&SyncRow> = progress.rows.iter().collect();
                out = out.child(div().pl_2().child(sync_table(&rows, OPS_ROWS, cx)));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Checked, Options, Res, all_on, checked_lines, own_label};

    /// An option on in every Application's spec shows on; one on in some says how many.
    #[test]
    fn options_already_on_say_so() {
        assert_eq!(own_label("Replace", (0, 3)), "Replace");
        assert_eq!(own_label("Server-side apply", (1, 3)), "Server-side apply (already on for 1 of 3)");
        assert_eq!(own_label("Server-side apply", (1, 1)), "Server-side apply (already on in the Application's spec)");
        assert!(all_on((2, 2)) && !all_on((1, 2)) && !all_on((0, 0)));
    }

    /// An Application left as it is says so in one line: no hook is skipped and nothing is
    /// pruned on it. One synced says what is applied and the hooks it skips or runs.
    #[test]
    fn hooks_are_skipped_only_where_something_is_synced() {
        let hooks = vec!["PreSync Job shop/db-migrate".to_string()];
        let checked = Checked { hooks, auto_sync: true, ..Default::default() };
        let without = Options { hooks: false, prune: true, dry_run: false, ..Default::default() };
        assert_eq!(
            checked_lines(&checked, false, without, None),
            [("Nothing out of sync: not synced · auto-sync on".to_string(), false)]
        );
        let res = Res { kind: "Deployment".into(), namespace: "shop".into(), name: "web".into(), ..Default::default() };
        let synced = Checked { out_of_sync: vec![res], ..checked };
        assert_eq!(
            checked_lines(&synced, true, without, Some("production")),
            [
                ("Ready · auto-sync on · production environment".to_string(), false),
                ("1 resource out of sync: Deployment shop/web".to_string(), false),
                ("Hooks skipped: PreSync Job shop/db-migrate".to_string(), true),
                ("Nothing to prune.".to_string(), false),
            ]
        );
        let whole = Options { hooks: true, prune: false, dry_run: false, ..Default::default() };
        assert_eq!(
            checked_lines(&synced, true, whole, None),
            [
                ("Ready · auto-sync on".to_string(), false),
                ("Hooks that run: PreSync Job shop/db-migrate".to_string(), false),
            ]
        );
    }
}
