//! A page's Argo CD section: the Application that deployed the object (or those deploying
//! into a namespace), found in what Argo CD's API feeds hold, opening one on the Argo CD
//! page, and its actions (Refresh, Sync, Terminate, Roll back) by Argo CD's API
//! (`confirm_api_action`). The Argo CD page asks the same of its rows. An Application opened
//! as a Kubernetes object is not acted on from its page: it opens on the Argo CD page, found
//! by its uid.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use serde_json::Value;

use super::{ConfirmButtons as _, DetailPanel, Guard, RunAs, confirm_with, guard_ok, shell_word};
use crate::k8s;
use crate::k8s::argo::Lookup;
use crate::k8s::argo::api;
use crate::k8s::argo::dashboard::rollback_undoes;
use crate::k8s::argo::diagnosis::Res;
use crate::k8s::argo::ops::{self, Op, SyncRequest};
use crate::ui::argo::OpenArgoApp;

/// What Refresh, Hard refresh, Sync, Terminate and Roll back do, in their confirmations.
pub(super) const REFRESH_NOTE: &str = "Argo CD compares it with git again. Nothing is deployed.";
const HARD_REFRESH_NOTE: &str =
    "Argo CD renders its manifests again, without its cache, and compares them with the cluster. Nothing is deployed.";
const SYNC_NOTE: &str = "Argo CD deploys its target revision now, with its own sync options.";
const TERMINATE_NOTE: &str =
    "Argo CD stops the operation running, hooks included. What it applied so far stays as it is.";
const ROLLBACK_NOTE: &str = "Argo CD applies that deployment's revision and sources again, without hooks and \
     without pruning. Keep auto-sync off until git has the fix: it would deploy git's revision again.";
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

/// Confirm, then run `action` on the Application `ns/name` read from the Argo CD API server
/// `endpoint`, through that API: a server set read-only (or the hub context tied to it) is
/// refused before anything is asked, write mode applies when it is sent, as to any write, and
/// the confirmation is a protected context's (the name typed, or a wait), named after the
/// server. A sync, a terminate or a rollback reads the Application first, and what Argo CD
/// would refuse is said instead. A sync here is of the whole Application, with its own sync
/// options. What it changes comes back on the feed's stream.
pub(crate) fn confirm_api_action(
    endpoint: &Arc<api::Endpoint>,
    ns: &str,
    name: &str,
    action: ArgoAction,
    window: &mut Window,
    cx: &mut App,
) {
    confirm_api_action_with(endpoint, ns, name, action, Extra::default(), window, cx);
}

/// What a page adds to an action's dialog: its title, its note, what it shows under them (the
/// versions a rollback changes), and a name to type (a production environment's) in place of
/// the server's protection.
#[derive(Clone, Default)]
pub(crate) struct Extra {
    pub title: Option<String>,
    pub note: Option<String>,
    pub body: Option<Body>,
    pub typed: Option<String>,
}

/// What a dialog shows under its note, drawn each time it is.
pub(crate) type Body = Rc<dyn Fn(&App) -> AnyElement>;

/// [`confirm_api_action`], its dialog with what `extra` adds.
pub(crate) fn confirm_api_action_with(
    endpoint: &Arc<api::Endpoint>,
    ns: &str,
    name: &str,
    action: ArgoAction,
    extra: Extra,
    window: &mut Window,
    cx: &mut App,
) {
    let protected = api::read_only(endpoint).unwrap_or_else(|| endpoint.name());
    let what = format!("{} {name}", action.label().to_lowercase());
    if !super::not_read_only(&protected, window, cx) || !super::ensure_argo_writes(&what, &endpoint.name(), window, cx)
    {
        return;
    }
    let (endpoint, ns, name) = (endpoint.clone(), ns.to_string(), name.to_string());
    if !action.reads_first() {
        confirm_api(endpoint, ns, name, action, &Value::Null, extra, window, cx);
        return;
    }
    let read = api::read_app(endpoint.clone(), ns.clone(), name.clone());
    window
        .spawn(cx, async move |cx| {
            let result = read.await;
            let _ = cx.update(|window, cx| match result {
                Ok(app) => confirm_api(endpoint, ns, name, action, &app, extra, window, cx),
                Err(e) => window.push_notification(
                    Notification::error(format!("{} failed: the Application could not be read: {e}", action.label())),
                    cx,
                ),
            });
        })
        .detach();
}

/// The dialog of an action by the API, on the Application `app` as just read (null for a
/// refresh, which reads nothing first), with what `extra` adds.
#[allow(clippy::too_many_arguments)]
fn confirm_api(
    endpoint: Arc<api::Endpoint>,
    ns: String,
    name: String,
    action: ArgoAction,
    app: &Value,
    extra: Extra,
    window: &mut Window,
    cx: &mut App,
) {
    let refused = |why: String, window: &mut Window, cx: &mut App| {
        let msg = format!("{} Application {name}: Argo CD would refuse it: {why}.", action.label());
        window.push_notification(Notification::error(msg), cx);
    };
    let checked = action.op().map(|op| ops::refusal(app, &op).map_or_else(|| ops::pinned(app, &op), Err));
    let call = match (action, checked) {
        (_, Some(Err(why))) => return refused(why, window, cx),
        (ArgoAction::Refresh, _) => api::Action::Refresh { hard: false },
        (ArgoAction::HardRefresh, _) => api::Action::Refresh { hard: true },
        (ArgoAction::Sync, _) => api::Action::Sync(SyncRequest::default()),
        (ArgoAction::Terminate, _) => api::Action::Terminate,
        (ArgoAction::Rollback(_), Some(Ok(Op::Rollback { id: Some(id), .. }))) => {
            api::Action::Rollback { id, dry_run: false }
        }
        (ArgoAction::Rollback(_), _) => return refused("that deployment has no id".into(), window, cx),
    };
    let label = action.label();
    let note = match action {
        _ if extra.note.is_some() => extra.note.as_deref().unwrap_or_default(),
        ArgoAction::Refresh => REFRESH_NOTE,
        ArgoAction::HardRefresh => HARD_REFRESH_NOTE,
        ArgoAction::Sync => SYNC_NOTE,
        ArgoAction::Terminate => TERMINATE_NOTE,
        ArgoAction::Rollback(_) => ROLLBACK_NOTE,
    };
    // A selective sync after the newest deployment is undone too: say so.
    let note: SharedString = match (&call, rollback_undoes(app)) {
        (api::Action::Rollback { .. }, Some(undoes)) => format!("{note} {undoes}").into(),
        _ => note.to_string().into(),
    };
    let title = match &call {
        _ if extra.title.is_some() => extra.title.clone().unwrap_or_default(),
        api::Action::Rollback { id, .. } => {
            let to = ops::rollback_target(app, Some(*id)).map_or_default(ops::deployment_label);
            format!("Roll back Application {name} to {to}?")
        }
        api::Action::Terminate => format!("Terminate the operation of Application {name}?"),
        _ => format!("{label} Application {name}?"),
    };
    let server_name = endpoint.name();
    let target = shell_word(&format!("{ns}/{name}"));
    let args = match &call {
        api::Action::Refresh { hard: false } => format!("get {target} --refresh"),
        api::Action::Refresh { hard: true } => format!("get {target} --hard-refresh"),
        api::Action::Sync(_) => format!("sync {target}"),
        api::Action::Terminate => format!("terminate-op {target}"),
        api::Action::Rollback { id, .. } => format!("rollback {target} {id}"),
        api::Action::Delete { cascade } => format!("delete {target} --cascade={cascade}"),
    };
    let cmd = format!("argocd app {args} --server {}", shell_word(&server_name));
    // A production environment's name typed, else what the server's protection asks.
    let guard = match &extra.typed {
        Some(t) => Some(Guard::typed_text(t, format!("Type {t} to confirm:"), window, cx)),
        None => Guard::protected(&server_name, window, cx),
    };
    let body = extra.body.clone();
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let (endpoint, ns, name, call, guard) =
            (endpoint.clone(), ns.clone(), name.clone(), call.clone(), guard.clone());
        let mut alert = alert.title(title.clone()).description(note.clone());
        if let Some(body) = &body {
            alert = alert.child(body(cx));
        }
        alert
            .child(confirm_with(&cmd, guard.as_ref(), &RunAs::default(), cx))
            .confirm_buttons(label, false, guard.as_ref())
            .on_ok(move |_, window, cx| {
                if !guard_ok(guard.as_ref(), cx) {
                    return false;
                }
                // Asked with write mode on, but it may have been turned off since.
                if !super::writes_still_on(window, cx) {
                    return true;
                }
                let write = api::act(endpoint.clone(), ns.clone(), name.clone(), call.clone());
                window
                    .spawn(cx, async move |cx| {
                        let result = write.await;
                        let _ = cx.update(|window, cx| match result {
                            Ok(msg) => window.push_notification(Notification::success(msg), cx),
                            Err(e) => window.push_notification(Notification::error(format!("{label} failed: {e}")), cx),
                        });
                    })
                    .detach();
                true
            })
    });
}

/// What deleting an Application does, with and without its resources.
const DELETE_NOTE: &str = "Argo CD deletes the Application and every object it deployed (cascade). Argo CD \
     deploys it again if an app of apps or an ApplicationSet still lists it in git.";
const DELETE_KEEP_NOTE: &str = "Argo CD deletes the Application only: the objects it deployed stay in the cluster, \
     no longer managed. Argo CD deploys it again if an app of apps or an ApplicationSet still lists it in git.";

/// Confirm, then delete the Application `ns/name` read from `endpoint` by Argo CD's API, with
/// what it deployed (`cascade`) or without. The checks of [`confirm_api_action`] apply (a
/// read-only server refused first, write mode when it is sent), and its name is typed to
/// confirm, as for a Kubernetes delete.
pub(crate) fn confirm_api_delete(
    endpoint: &Arc<api::Endpoint>,
    ns: &str,
    name: &str,
    cascade: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let protected = api::read_only(endpoint).unwrap_or_else(|| endpoint.name());
    let what = format!("delete {name}");
    if !super::not_read_only(&protected, window, cx) || !super::ensure_argo_writes(&what, &endpoint.name(), window, cx)
    {
        return;
    }
    let (endpoint, ns, name) = (endpoint.clone(), ns.to_string(), name.to_string());
    let call = api::Action::Delete { cascade };
    let cmd = format!(
        "argocd app delete {} --cascade={cascade} --server {}",
        shell_word(&format!("{ns}/{name}")),
        shell_word(&endpoint.name())
    );
    let guard = Guard::typed_text(&name, format!("Type {name} to delete it:"), window, cx);
    let title = if cascade {
        format!("Delete Application {name} and what it deployed?")
    } else {
        format!("Delete Application {name}, keeping what it deployed?")
    };
    let note = if cascade { DELETE_NOTE } else { DELETE_KEEP_NOTE };
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let (endpoint, ns, name, call, guard) =
            (endpoint.clone(), ns.clone(), name.clone(), call.clone(), guard.clone());
        alert
            .title(title.clone())
            .description(note)
            .child(confirm_with(&cmd, Some(&guard), &RunAs::default(), cx))
            .confirm_buttons("Delete", true, Some(&guard))
            .on_ok(move |_, window, cx| {
                if !guard_ok(Some(&guard), cx) {
                    return false;
                }
                if !super::writes_still_on(window, cx) {
                    return true;
                }
                let write = api::act(endpoint.clone(), ns.clone(), name.clone(), call.clone());
                window
                    .spawn(cx, async move |cx| {
                        let result = write.await;
                        let _ = cx.update(|window, cx| match result {
                            Ok(msg) => window.push_notification(Notification::success(msg), cx),
                            Err(e) => window.push_notification(Notification::error(format!("Delete failed: {e}")), cx),
                        });
                    })
                    .detach();
                true
            })
    });
}

/// Ask the app to open an object on a context, connected or not: where an Application
/// deploys (its namespace), and what it deploys there.
#[derive(Clone)]
pub struct OpenOn {
    pub context: String,
    pub res: Res,
}

impl EventEmitter<OpenOn> for DetailPanel {}

impl EventEmitter<OpenArgoApp> for DetailPanel {}

impl DetailPanel {
    /// The read of the Argo CD section for the object as last read; None while it is deleted.
    fn argo_work(&self) -> Option<std::pin::Pin<Box<dyn Future<Output = Lookup>>>> {
        let obj = self.object.clone().filter(|_| !self.deleted)?;
        let (context, kind) = (self.context.clone(), &self.obj.kind);
        let work: std::pin::Pin<Box<dyn Future<Output = Lookup>>> = if kind.is("", "Namespace") {
            Box::pin(k8s::argo::for_namespace(context, self.obj.name.clone()))
        } else if kind.is("argoproj.io", "Application") {
            // Its own Application: the Argo CD server that holds it (by uid), and its parent.
            let uid = obj.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
            let parent = k8s::argo::for_object(context, kind.ar.group.clone(), kind.ar.kind.clone(), obj);
            Box::pin(async move {
                let (mut lookup, own) = futures::join!(parent, k8s::argo::api_by_uid(uid));
                lookup.own = Some(own);
                lookup
            })
        } else {
            Box::pin(k8s::argo::for_object(context, kind.ar.group.clone(), kind.ar.kind.clone(), obj))
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
                let Ok(work) = this.update(cx, |this, _| this.argo_work()) else { break };
                // Nothing to read while the object is deleted: one created again under its name
                // is followed (see `start_live`).
                if let Some(work) = work {
                    let started = Instant::now();
                    let lookup = work.await;
                    if this.update(cx, |this, cx| this.set_argo(lookup, started, cx)).is_err() {
                        break;
                    }
                }
                exec.timer(crate::settings::secs(crate::settings::get().metrics_refresh)).await;
            }
        });
        self.keep_task(task);
    }

    /// Open the Application shown at `ix` on the Argo CD page's dashboard.
    pub fn open_argo_app(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(found) = self.argo.apps.get(ix) else { return };
        let (namespace, name) = (found.app.namespace.clone(), found.app.name.clone());
        cx.emit(OpenArgoApp { instance: found.server.instance(), namespace, name });
    }

    /// Confirm, then run `action` on the Application shown at `ix`, by Argo CD's API (the
    /// server's protection and the write gate apply, see `confirm_api_action`). What it
    /// changes comes back on the feed, read again on the page's next round.
    pub fn confirm_argo(&mut self, ix: usize, action: ArgoAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = self.argo.apps.get(ix).cloned() else { return };
        confirm_api_action(&found.server, &found.app.namespace, &found.app.name, action, window, cx);
    }

    /// Open the Application of this page on the Argo CD page's dashboard, on the Argo CD
    /// server found holding it (the same `metadata.uid`).
    pub fn open_own_in_argo(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(endpoint)) = &self.argo.own else { return };
        let namespace = self.obj.namespace.clone().unwrap_or_else(|| k8s::argo::DEFAULT_NAMESPACE.into());
        cx.emit(OpenArgoApp { instance: endpoint.instance(), namespace, name: self.obj.name.clone() });
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
