//! A page's Argo CD section: reading the Applications on Argo CD's cluster, opening one,
//! and its Refresh and Sync, sent with that cluster's client. The Argo CD page asks the
//! same of its rows through `confirm_argo_action`.

use std::rc::Rc;
use std::time::Instant;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::{ConfirmButtons as _, DetailPanel, Guard, WriteFuture, confirm_extras, guard_ok, kubectl, shell_word};
use crate::k8s::argo::{Hub, Lookup};
use crate::k8s::{self, ObjectRef};

/// What Refresh, Hard refresh and Sync do, in their confirmations.
pub(super) const REFRESH_NOTE: &str = "Argo CD compares it with git again. Nothing is deployed.";
const HARD_REFRESH_NOTE: &str =
    "Argo CD renders its manifests again, without its cache, and compares them with the cluster. Nothing is deployed.";
pub(super) const SYNC_NOTE: &str = "Argo CD deploys its target revision now, with its own sync options.";

/// What can be asked of an Application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgoAction {
    Refresh,
    HardRefresh,
    Sync,
}

impl ArgoAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Refresh => "Refresh",
            Self::HardRefresh => "Hard refresh",
            Self::Sync => "Sync",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Self::Refresh => REFRESH_NOTE,
            Self::HardRefresh => HARD_REFRESH_NOTE,
            Self::Sync => SYNC_NOTE,
        }
    }

    /// The kubectl arguments it stands for, as shown in confirmations.
    pub(super) fn args(self, kind: &str, name: &str) -> String {
        let name = shell_word(name);
        match self {
            Self::Sync => {
                let patch = k8s::shell_quote(&k8s::argo_sync_patch().to_string());
                format!("patch {kind} {name} --type merge -p {patch}")
            }
            Self::Refresh => format!("annotate {kind} {name} --overwrite argocd.argoproj.io/refresh=normal"),
            Self::HardRefresh => format!("annotate {kind} {name} --overwrite argocd.argoproj.io/refresh=hard"),
        }
    }

    fn run(self, client: kube::Client, ns: String, name: String) -> WriteFuture {
        let kind = k8s::argo::application_kind();
        match self {
            Self::Sync => Box::pin(k8s::argo_sync(client, kind, Some(ns), name)),
            Self::Refresh => Box::pin(k8s::argo_refresh(client, kind, Some(ns), name, false)),
            Self::HardRefresh => Box::pin(k8s::argo_refresh(client, kind, Some(ns), name, true)),
        }
    }
}

/// Confirm, then run `action` on the Application `ns/name` of `hub`, with the hub's client:
/// write mode, the hub's protection and the write gate apply as to any write. `done` runs
/// once Argo CD's cluster has answered, to read the Application again.
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
    let label = action.label();
    let cmd = kubectl(&hub.context, Some(ns), &action.args(&k8s::argo::application_kind().kubectl_name(), name));
    let guard = Guard::protected(&hub.context, window, cx);
    let title = format!("{label} Application {name}?");
    let (hub, ns, name, done) = (hub.clone(), ns.to_string(), name.to_string(), Rc::new(done));
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let (guard, hub, ns, name, done) = (guard.clone(), hub.clone(), ns.clone(), name.clone(), done.clone());
        alert
            .title(title.clone())
            .description(action.note())
            .child(confirm_extras(&cmd, guard.as_ref(), cx))
            .confirm_buttons(label, false, guard.as_ref())
            .on_ok(move |_, window, cx| {
                if !guard_ok(guard.as_ref(), cx) {
                    return false;
                }
                // Asked with write mode on, but it may have been turned off since.
                if !super::writes_still_on(window, cx) {
                    return true;
                }
                let write = action.run(hub.client.clone(), ns.clone(), name.clone());
                let (hub, done) = (hub.context.clone(), done.clone());
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

/// Ask the app to open a namespace on a context, connected or not: where an Application
/// deploys.
#[derive(Clone)]
pub struct OpenNamespace {
    pub context: String,
    pub namespace: String,
}

impl EventEmitter<OpenNamespace> for DetailPanel {}

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

    /// Confirm, then Refresh (or Sync) the Application shown at `ix`, with the client of Argo
    /// CD's cluster: write mode, its protection and the write gate apply as to any write.
    pub fn confirm_argo(&mut self, ix: usize, sync: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = self.argo.apps.get(ix).cloned() else { return };
        let action = if sync { ArgoAction::Sync } else { ArgoAction::Refresh };
        let panel = cx.entity().downgrade();
        let done = move |_: &mut Window, cx: &mut App| {
            let _ = panel.update(cx, Self::reload_argo);
        };
        confirm_argo_action(&found.hub, &found.app.namespace, &found.app.name, action, done, window, cx);
    }

    /// Open the namespace this Application deploys into, on the context that reaches it.
    pub fn open_destination(&mut self, cx: &mut Context<Self>) {
        let Some((dest, Some(context))) = self.related.argo_destination.clone() else { return };
        if !dest.namespace.is_empty() {
            cx.emit(OpenNamespace { context, namespace: dest.namespace });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ArgoAction;

    #[test]
    fn the_kubectl_shown_is_what_runs() {
        let kind = "applications.argoproj.io";
        let refresh = ArgoAction::Refresh.args(kind, "web");
        assert_eq!(refresh, "annotate applications.argoproj.io web --overwrite argocd.argoproj.io/refresh=normal");
        let hard = ArgoAction::HardRefresh.args(kind, "web");
        assert_eq!(hard, "annotate applications.argoproj.io web --overwrite argocd.argoproj.io/refresh=hard");
        let sync = ArgoAction::Sync.args(kind, "web");
        assert!(sync.starts_with("patch applications.argoproj.io web --type merge -p '"), "{sync}");
        assert!(sync.contains("\"syncStrategy\""), "{sync}");
    }
}
