//! A page's Argo CD section: reading the Applications on Argo CD's cluster, opening one,
//! and its actions (Refresh, Sync with its options, Terminate, Roll back), sent with that
//! cluster's client. The Argo CD page asks the same of its rows through `confirm_argo_action`.

use std::rc::Rc;
use std::time::Instant;

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{ConfirmButtons as _, DetailPanel, Guard, RunAs, WriteFuture, confirm_with, guard_ok, kubectl, shell_word};
use crate::k8s::argo::diagnosis::Res;
use crate::k8s::argo::ops::{self, Op, SyncRequest};
use crate::k8s::argo::{Hub, Lookup};
use crate::k8s::{self, ObjectRef};

/// What Refresh, Hard refresh, Sync, Terminate and Roll back do, in their confirmations.
pub(super) const REFRESH_NOTE: &str = "Argo CD compares it with git again. Nothing is deployed.";
const HARD_REFRESH_NOTE: &str =
    "Argo CD renders its manifests again, without its cache, and compares them with the cluster. Nothing is deployed.";
const SYNC_NOTE: &str = "Argo CD deploys its target revision now, with its own sync options.";
const TERMINATE_NOTE: &str =
    "Argo CD stops the operation running, hooks included. What it applied so far stays as it is.";
const ROLLBACK_NOTE: &str = "Argo CD applies that deployment's revision and sources again, without hooks and \
     without pruning. Keep auto-sync off until git has the fix: it would deploy git's revision again.";
/// Resources out of sync listed to pick from, at most: past that, the whole Application syncs.
const PICK_MAX: usize = 200;

/// What can be asked of an Application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgoAction {
    Refresh,
    HardRefresh,
    /// A sync, with the options picked in its dialog.
    Sync,
    /// Stop the operation running.
    Terminate,
    /// Back to the deployment of its history with this id; None is the one before the newest.
    Rollback(Option<i64>),
}

impl ArgoAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Refresh => "Refresh",
            Self::HardRefresh => "Hard refresh",
            Self::Sync => "Sync",
            Self::Terminate => "Terminate",
            Self::Rollback(_) => "Roll back",
        }
    }

    /// Whether its confirmation needs the Application as it is now: what a sync can pick
    /// from, the deployment a rollback goes to, and whether Argo CD would refuse it.
    fn reads_first(self) -> bool {
        !matches!(self, Self::Refresh | Self::HardRefresh)
    }

    /// The operation it writes, for those that write one.
    fn op(self) -> Option<Op> {
        match self {
            Self::Terminate => Some(Op::Terminate),
            Self::Rollback(id) => Some(Op::Rollback { id, dry_run: false }),
            Self::Sync => Some(Op::Sync(SyncRequest::default())),
            Self::Refresh | Self::HardRefresh => None,
        }
    }
}

/// The kubectl arguments of a Refresh, as shown in confirmations.
pub(super) fn refresh_args(kind: &str, name: &str, hard: bool) -> String {
    let mode = if hard { "hard" } else { "normal" };
    format!("annotate {kind} {} --overwrite argocd.argoproj.io/refresh={mode}", shell_word(name))
}

/// The kubectl arguments of a merge patch, as shown in confirmations. The `resourceVersion`
/// the write adds (so a change meanwhile is refused) is left out: kubectl has none to give.
fn patch_args(kind: &str, name: &str, patch: &Value) -> String {
    let mut shown = patch.clone();
    if let Some(o) = shown.as_object_mut() {
        o.remove("metadata");
    }
    format!("patch {kind} {} --type merge -p {}", shell_word(name), k8s::shell_quote(&shown.to_string()))
}

/// The Application an action is for, on its hub.
#[derive(Clone)]
struct Target {
    hub: Hub,
    ns: String,
    name: String,
}

impl Target {
    fn object(&self) -> ObjectRef {
        ObjectRef {
            kind: k8s::argo::application_kind(),
            name: self.name.clone(),
            namespace: Some(self.ns.clone()),
            uid: String::new(),
        }
    }

    /// The kubectl command shown for `args` on this Application.
    fn command(&self, args: &str) -> String {
        kubectl(&self.hub.context, Some(&self.ns), args)
    }

    fn kind_name() -> String {
        k8s::argo::application_kind().kubectl_name()
    }

    /// Start `op` on it with `client` (the hub's, or one running as the identity chosen).
    fn operation(&self, client: kube::Client, op: Op) -> WriteFuture {
        let kind = k8s::argo::application_kind();
        Box::pin(k8s::argo_operation(client, kind, Some(self.ns.clone()), self.name.clone(), op))
    }
}

/// Run once Argo CD's cluster has answered: read the Application again.
type Done = Rc<dyn Fn(&mut Window, &mut App)>;

/// Confirm, then run `action` on the Application `ns/name` of `hub`, with the hub's client:
/// write mode, the hub's protection and the write gate apply as to any write. A sync, a
/// terminate or a rollback reads the Application first: its dialog shows what it would do,
/// and what Argo CD would refuse is said instead. `done` runs once Argo CD's cluster has
/// answered, to read the Application again.
pub(crate) fn confirm_argo_action(
    hub: &Hub,
    ns: &str,
    name: &str,
    action: ArgoAction,
    done: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    if !super::ensure_writes(window, cx) {
        return;
    }
    let done: Done = Rc::new(done);
    let target = Target { hub: hub.clone(), ns: ns.to_string(), name: name.to_string() };
    if !action.reads_first() {
        confirm_refresh(target, action == ArgoAction::HardRefresh, done, window, cx);
        return;
    }
    let read = k8s::get_object(target.hub.client.clone(), target.object());
    window
        .spawn(cx, async move |cx| {
            let result = read.await;
            let _ = cx.update(|window, cx| match result {
                Ok(app) => confirm_operation(target, action, &app, done, window, cx),
                Err(e) => window.push_notification(
                    Notification::error(format!(
                        "{} failed: the Application could not be read: {}",
                        action.label(),
                        k8s::short_error(&e)
                    )),
                    cx,
                ),
            });
        })
        .detach();
}

/// Send a write confirmed in a dialog, say how it went, then read the hub's Applications again.
fn send(label: &'static str, write: WriteFuture, hub: String, done: Done, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            let result = write.await;
            let _ = cx.update(|window, cx| {
                match result {
                    Ok(msg) => window.push_notification(Notification::success(msg), cx),
                    Err(e) => window.push_notification(
                        Notification::error(format!("{label} failed: {}", k8s::short_error(&e))),
                        cx,
                    ),
                }
                // The hub's Applications come from a list kept a little while: read it anew.
                k8s::argo::forget_list(&hub);
                done(window, cx);
            });
        })
        .detach();
}

fn confirm_refresh(t: Target, hard: bool, done: Done, window: &mut Window, cx: &mut App) {
    let action = if hard { ArgoAction::HardRefresh } else { ArgoAction::Refresh };
    let label = action.label();
    let cmd = t.command(&refresh_args(&Target::kind_name(), &t.name, hard));
    let guard = Guard::protected(&t.hub.context, window, cx);
    let run_as = RunAs::offer(&t.hub.context, cx);
    let title = format!("{label} Application {}?", t.name);
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let (t, guard, run_as, done) = (t.clone(), guard.clone(), run_as.clone(), done.clone());
        alert
            .title(title.clone())
            .description(if hard { HARD_REFRESH_NOTE } else { REFRESH_NOTE })
            .child(confirm_with(&cmd, guard.as_ref(), &run_as, cx))
            .confirm_buttons(label, false, guard.as_ref())
            .on_ok(move |_, window, cx| {
                if !guard_ok(guard.as_ref(), cx) {
                    return false;
                }
                // Asked with write mode on, but it may have been turned off since.
                if !super::writes_still_on(window, cx) {
                    return true;
                }
                let client = run_as.client(&t.hub.client, cx);
                let kind = k8s::argo::application_kind();
                let write: WriteFuture =
                    Box::pin(k8s::argo_refresh(client, kind, Some(t.ns.clone()), t.name.clone(), hard));
                send(label, write, t.hub.context.clone(), done.clone(), window, cx);
                true
            })
    });
}

/// The dialog of a sync, a terminate or a rollback, for the Application `app` as just read;
/// what Argo CD would refuse is said instead.
fn confirm_operation(t: Target, action: ArgoAction, app: &Value, done: Done, window: &mut Window, cx: &mut App) {
    let Some(op) = action.op() else { return };
    // A rollback to "the one before the newest" goes to the deployment named here, even if
    // another lands while the dialog is open.
    let checked = ops::refusal(app, &op).map_or_else(|| ops::pinned(app, &op), Err);
    let op = match checked {
        Ok(op) => op,
        Err(why) => {
            let msg = format!("{} Application {}: Argo CD would refuse it: {why}.", action.label(), t.name);
            window.push_notification(Notification::error(msg), cx);
            return;
        }
    };
    if action == ArgoAction::Sync {
        confirm_sync(t, app, done, window, cx);
        return;
    }
    let Ok(patch) = ops::patch(app, &op) else { return };
    let label = action.label();
    let cmd = t.command(&patch_args(&Target::kind_name(), &t.name, &patch));
    let (title, note) = match &op {
        Op::Rollback { id, .. } => {
            let to = ops::rollback_target(app, *id).map_or_default(ops::deployment_label);
            (format!("Roll back Application {} to {to}?", t.name), ROLLBACK_NOTE)
        }
        _ => (format!("Terminate the operation of Application {}?", t.name), TERMINATE_NOTE),
    };
    let guard = Guard::protected(&t.hub.context, window, cx);
    let run_as = RunAs::offer(&t.hub.context, cx);
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let (t, op, guard, run_as, done) = (t.clone(), op.clone(), guard.clone(), run_as.clone(), done.clone());
        alert
            .title(title.clone())
            .description(note)
            .child(confirm_with(&cmd, guard.as_ref(), &run_as, cx))
            .confirm_buttons(label, false, guard.as_ref())
            .on_ok(move |_, window, cx| {
                if !guard_ok(guard.as_ref(), cx) {
                    return false;
                }
                if !super::writes_still_on(window, cx) {
                    return true;
                }
                let write = t.operation(run_as.client(&t.hub.client, cx), op.clone());
                send(label, write, t.hub.context.clone(), done.clone(), window, cx);
                true
            })
    });
}

/// What the sync dialog has picked so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SyncChoice {
    /// Each resource out of sync offered, ticked or not; empty when none is offered.
    picked: Vec<bool>,
    prune: bool,
    dry_run: bool,
}

impl SyncChoice {
    /// The sync asked for: of the whole Application while every resource offered is ticked
    /// (hooks run then, as with Argo CD's own Sync), else of those ticked alone.
    fn request(&self, offered: &[Res]) -> SyncRequest {
        let resources = if self.picked.iter().all(|p| *p) {
            Vec::new()
        } else {
            offered.iter().zip(&self.picked).filter(|(_, p)| **p).map(|(r, _)| r.clone()).collect()
        };
        SyncRequest { resources, prune: self.prune, dry_run: self.dry_run, prune_shown: None }
    }

    /// Resources were offered and none is ticked: there is nothing to sync.
    fn nothing_picked(&self) -> bool {
        !self.picked.is_empty() && !self.picked.contains(&true)
    }
}

/// The sync dialog: the resources out of sync to pick from, all ticked; prune, listing what
/// it deletes; dry run. The kubectl command follows what is picked.
fn confirm_sync(t: Target, app: &Value, done: Done, window: &mut Window, cx: &mut App) {
    let offered = Rc::new(ops::out_of_sync(app));
    let pick = offered.len() <= PICK_MAX;
    let to_prune = Rc::new(ops::pruned(app, &[]));
    let choice = cx.new(|_| SyncChoice {
        picked: if pick { vec![true; offered.len()] } else { Vec::new() },
        ..Default::default()
    });
    let guard = Guard::protected(&t.hub.context, window, cx);
    let run_as = RunAs::offer(&t.hub.context, cx);
    let title = format!("Sync Application {}?", t.name);
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let (t, guard, run_as, done) = (t.clone(), guard.clone(), run_as.clone(), done.clone());
        let (offered, choice_ok, to_prune_ok) = (offered.clone(), choice.clone(), to_prune.clone());
        let c = choice.read(cx).clone();
        let req = c.request(&offered);
        let gone = ops::reached(&to_prune, &req.resources);
        let cmd = t.command(&patch_args(&Target::kind_name(), &t.name, &ops::sync_patch(&req)));
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let deletes = c.prune && !c.dry_run && !gone.is_empty();
        let prune_text = if to_prune.is_empty() {
            "Nothing is to prune: every resource it tracks is still in git.".to_string()
        } else if gone.is_empty() {
            "None of the resources picked is to prune.".to_string()
        } else {
            let verb = if deletes { "Deletes" } else { "Would delete" };
            format!("{verb} what is no longer in git: {}", ops::named(&gone))
        };
        let picker = (pick && !offered.is_empty()).then(|| {
            let rows = offered.iter().enumerate().map(|(i, r)| {
                let choice = choice.clone();
                let checked = c.picked.get(i).copied().unwrap_or(true);
                Checkbox::new(("argo-sync-res", i)).label(r.label()).checked(checked).on_click(
                    move |v: &bool, _, cx| {
                        let v = *v;
                        choice.update(cx, |c, cx| {
                            if let Some(p) = c.picked.get_mut(i) {
                                *p = v;
                            }
                            cx.notify();
                        });
                        cx.refresh_windows();
                    },
                )
            });
            v_flex()
                .gap_1()
                .pt_2()
                .child(div().text_sm().text_color(muted).child(format!(
                    "Out of sync ({}). Untick some to sync only the others; hooks run only when all are synced.",
                    offered.len()
                )))
                .child(v_flex().id("argo-sync-pick").max_h(px(200.)).overflow_y_scroll().gap_0p5().children(rows))
        });
        let too_many = (!pick).then(|| {
            div().pt_2().text_sm().text_color(muted).child(format!(
                "{} resources are out of sync: too many to pick here, the whole Application is synced.",
                offered.len()
            ))
        });
        let (prune_choice, dry_choice) = (choice.clone(), choice.clone());
        let options = v_flex()
            .gap_1()
            .pt_2()
            .child(Checkbox::new("argo-sync-prune").label("Prune").checked(c.prune).on_click(move |v: &bool, _, cx| {
                let v = *v;
                prune_choice.update(cx, |c, cx| {
                    c.prune = v;
                    cx.notify();
                });
                cx.refresh_windows();
            }))
            .child(div().text_sm().text_color(if deletes { theme.warning } else { muted }).child(prune_text))
            .child(Checkbox::new("argo-sync-dry-run").label("Dry run").checked(c.dry_run).on_click(
                move |v: &bool, _, cx| {
                    let v = *v;
                    dry_choice.update(cx, |c, cx| {
                        c.dry_run = v;
                        cx.notify();
                    });
                    cx.refresh_windows();
                },
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Argo CD applies nothing and says what it would do; its result is shown when it ends."),
            );
        alert
            .title(title.clone())
            .description(SYNC_NOTE)
            .when_some(picker, ParentElement::child)
            .when_some(too_many, ParentElement::child)
            .child(options)
            .child(confirm_with(&cmd, guard.as_ref(), &run_as, cx))
            .confirm_buttons(if c.dry_run { "Dry run" } else { "Sync" }, deletes, guard.as_ref())
            .on_ok(move |_, window, cx| {
                if !guard_ok(guard.as_ref(), cx) {
                    return false;
                }
                let c = choice_ok.read(cx).clone();
                if c.nothing_picked() {
                    window.push_notification(Notification::warning("Tick at least one resource to sync."), cx);
                    return false;
                }
                if !super::writes_still_on(window, cx) {
                    return true;
                }
                // Prune deletes no more than the list shown: if git changed since, it is refused.
                let mut req = c.request(&offered);
                req.prune_shown = Some(ops::reached(&to_prune_ok, &req.resources));
                let write = t.operation(run_as.client(&t.hub.client, cx), Op::Sync(req));
                send("Sync", write, t.hub.context.clone(), done.clone(), window, cx);
                true
            })
    });
}

/// Ask a page showing another context's object to open (an Application, on Argo CD's
/// cluster, which may not be connected).
#[derive(Clone)]
pub struct OpenOnContext {
    pub context: String,
    pub client: kube::Client,
    pub obj: ObjectRef,
}

impl EventEmitter<OpenOnContext> for DetailPanel {}

/// Ask the app to open an object on a context, connected or not: where an Application
/// deploys (its namespace), and what it deploys there.
#[derive(Clone)]
pub struct OpenOn {
    pub context: String,
    pub res: Res,
}

impl EventEmitter<OpenOn> for DetailPanel {}

impl DetailPanel {
    /// The read of the Argo CD section for the object as last read; None once it is gone.
    fn argo_work(&self) -> Option<std::pin::Pin<Box<dyn Future<Output = Lookup>>>> {
        let obj = self.object.clone().filter(|_| !self.deleted)?;
        // This page's cluster counts as connected (a page opened on Argo CD's own cluster).
        let mut connected = self.clusters.clone();
        if !connected.iter().any(|(n, _)| *n == self.context) {
            connected.insert(0, (self.context.clone(), self.client.clone()));
        }
        let (context, kind) = (self.context.clone(), &self.obj.kind);
        let work: std::pin::Pin<Box<dyn Future<Output = Lookup>>> = if kind.is("", "Namespace") {
            Box::pin(k8s::argo::for_namespace(connected, context, self.obj.name.clone()))
        } else {
            Box::pin(k8s::argo::for_object(connected, context, kind.ar.group.clone(), kind.ar.kind.clone(), obj))
        };
        Some(work)
    }

    /// Show a read, unless one that started later is shown already.
    fn set_argo(&mut self, lookup: Lookup, started: Instant, cx: &mut Context<Self>) {
        if self.argo_started.is_some_and(|shown| shown > started) {
            return;
        }
        self.argo_started = Some(started);
        self.argo = lookup;
        self.merge_history();
        cx.notify();
    }

    /// Read the Argo CD section while the page is shown, at the usage charts' pace.
    pub(super) fn start_argo(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                let Ok(Some(work)) = this.update(cx, |this, _| this.argo_work()) else { break };
                let started = Instant::now();
                let lookup = work.await;
                if this.update(cx, |this, cx| this.set_argo(lookup, started, cx)).is_err() {
                    break;
                }
                exec.timer(crate::settings::secs(crate::settings::get().metrics_refresh)).await;
            }
        });
        self.keep_task(task);
    }

    /// Read it again now (after a Refresh or a Sync).
    fn reload_argo(&mut self, cx: &mut Context<Self>) {
        let Some(work) = self.argo_work() else { return };
        let started = Instant::now();
        self.argo_load.spawn(cx, work, move |this, lookup, cx| this.set_argo(lookup, started, cx));
    }

    /// Open the Application shown at `ix`, on Argo CD's cluster.
    pub fn open_argo_app(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(found) = self.argo.apps.get(ix) else { return };
        let obj = ObjectRef {
            kind: k8s::argo::application_kind(),
            name: found.app.name.clone(),
            namespace: Some(found.app.namespace.clone()),
            uid: String::new(),
        };
        cx.emit(OpenOnContext { context: found.hub.context.clone(), client: found.hub.client.clone(), obj });
    }

    /// Confirm, then run `action` on the Application shown at `ix`, with the client of Argo
    /// CD's cluster: write mode, its protection and the write gate apply as to any write.
    pub fn confirm_argo(&mut self, ix: usize, action: ArgoAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = self.argo.apps.get(ix).cloned() else { return };
        let panel = cx.entity().downgrade();
        let done = move |_: &mut Window, cx: &mut App| {
            let _ = panel.update(cx, Self::reload_argo);
        };
        confirm_argo_action(&found.hub, &found.app.namespace, &found.app.name, action, done, window, cx);
    }

    /// Confirm, then run `action` on the Application of this page (its cluster is Argo CD's).
    pub fn confirm_own_argo(&mut self, action: ArgoAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ns) = self.obj.namespace.clone() else { return };
        let hub = Hub { context: self.context.clone(), client: self.client.clone() };
        let panel = cx.entity().downgrade();
        let done = move |window: &mut Window, cx: &mut App| {
            let _ = panel.update(cx, |this, cx| {
                this.refresh_object(false, window, cx);
                this.load_events(cx);
            });
        };
        confirm_argo_action(&hub, &ns, &self.obj.name, action, done, window, cx);
    }

    /// Open the namespace this Application deploys into, on the context that reaches it.
    pub fn open_destination(&mut self, cx: &mut Context<Self>) {
        let Some((dest, Some(context))) = self.related.argo_destination.clone() else { return };
        if !dest.namespace.is_empty() {
            cx.emit(OpenOn { context, res: Res::namespace(&dest.namespace) });
        }
    }

    /// Show the Application's resources out of sync or unhealthy alone, or all of them again.
    pub fn toggle_argo_attention(&mut self, cx: &mut Context<Self>) {
        self.argo_resources.alone = !self.argo_resources.alone;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{SyncChoice, patch_args, refresh_args};
    use crate::k8s::argo::diagnosis::Res;
    use crate::k8s::argo::ops::{self, SyncRequest};
    use proptest::prelude::*;

    #[test]
    fn the_kubectl_shown_is_what_runs() {
        let kind = "applications.argoproj.io";
        let refresh = refresh_args(kind, "web", false);
        assert_eq!(refresh, "annotate applications.argoproj.io web --overwrite argocd.argoproj.io/refresh=normal");
        let hard = refresh_args(kind, "web", true);
        assert_eq!(hard, "annotate applications.argoproj.io web --overwrite argocd.argoproj.io/refresh=hard");
        let mut patch = ops::sync_patch(&SyncRequest { prune: true, ..Default::default() });
        patch["metadata"] = serde_json::json!({"resourceVersion": "42"});
        let sync = patch_args(kind, "web", &patch);
        assert!(sync.starts_with("patch applications.argoproj.io web --type merge -p '"), "{sync}");
        assert!(sync.contains("\"syncStrategy\"") && sync.contains("\"prune\":true"), "{sync}");
        assert!(!sync.contains("resourceVersion"), "{sync}");
    }

    fn offered(n: usize) -> Vec<Res> {
        (0..n).map(|i| Res { kind: "ConfigMap".into(), name: format!("c{i}"), ..Default::default() }).collect()
    }

    proptest! {
        /// All ticked is Argo CD's own Sync (the whole Application, hooks included); otherwise
        /// the sync names exactly the resources ticked, and nothing ticked is never sent.
        #[test]
        fn the_sync_asked_for_is_what_is_ticked(
            picked in proptest::collection::vec(any::<bool>(), 0..8),
            prune in any::<bool>(),
            dry_run in any::<bool>(),
        ) {
            let res = offered(picked.len());
            let c = SyncChoice { picked: picked.clone(), prune, dry_run };
            let req = c.request(&res);
            prop_assert_eq!((req.prune, req.dry_run), (prune, dry_run));
            if picked.iter().all(|p| *p) {
                prop_assert!(req.resources.is_empty());
                prop_assert!(!c.nothing_picked());
            } else {
                let ticked: Vec<Res> = res.iter().zip(&picked).filter(|(_, p)| **p).map(|(r, _)| r.clone()).collect();
                prop_assert_eq!(c.nothing_picked(), ticked.is_empty());
                prop_assert_eq!(req.resources, ticked);
            }
        }
    }
}
