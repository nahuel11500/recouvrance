//! A page's Argo CD section: reading the Applications on Argo CD's cluster, opening one,
//! and its Refresh and Sync, sent with that cluster's client.

use std::time::Instant;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::InputState;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::{DetailPanel, WriteFuture, confirm_extras, guard_ok, kubectl, shell_word};
use crate::k8s::{self, ObjectRef, argo::Lookup};

/// What Refresh and Sync do, in their confirmations.
pub(super) const REFRESH_NOTE: &str = "Argo CD compares it with git again. Nothing is deployed.";
pub(super) const SYNC_NOTE: &str = "Argo CD deploys its target revision now, with its own sync options.";

/// The kubectl arguments of an Application's Refresh or Sync, as shown in confirmations.
pub(super) fn argo_args(action: &str, kind: &str, name: &str) -> String {
    let name = shell_word(name);
    if action == "Sync" {
        let patch = k8s::shell_quote(&k8s::argo_sync_patch().to_string());
        format!("patch {kind} {name} --type merge -p {patch}")
    } else {
        format!("annotate {kind} {name} --overwrite argocd.argoproj.io/refresh=normal")
    }
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
        if !self.ensure_writes(window, cx) {
            return;
        }
        let Some(found) = self.argo.apps.get(ix).cloned() else { return };
        let (hub, app) = (found.hub, found.app);
        let kind = k8s::argo::application_kind();
        let action = if sync { "Sync" } else { "Refresh" };
        let cmd = kubectl(&hub.context, Some(&app.namespace), &argo_args(action, &kind.kubectl_name(), &app.name));
        let guard = crate::settings::is_protected(&hub.context).then(|| {
            let ctx = hub.context.clone();
            (cx.new(|cx| InputState::new(window, cx).placeholder(ctx.clone())), ctx)
        });
        let title = format!("{action} Application {}?", app.name);
        let description = if sync { SYNC_NOTE } else { REFRESH_NOTE };
        let panel = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (panel, guard, hub, app, kind) = (panel.clone(), guard.clone(), hub.clone(), app.clone(), kind.clone());
            alert
                .title(title.clone())
                .description(description)
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text(action)
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let (client, ns, name) = (hub.client.clone(), Some(app.namespace.clone()), app.name.clone());
                    let fut: WriteFuture = if sync {
                        Box::pin(k8s::argo_sync(client, kind.clone(), ns, name))
                    } else {
                        Box::pin(k8s::argo_refresh(client, kind.clone(), ns, name))
                    };
                    let hub = hub.context.clone();
                    let _ = panel.update(cx, |this, cx| this.run_argo_write(action, hub, fut, window, cx));
                    true
                })
        });
    }

    fn run_argo_write(
        &mut self,
        label: &'static str,
        hub: String,
        fut: WriteFuture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(msg) => window.push_notification(Notification::success(msg), cx),
                    Err(e) => window.push_notification(
                        Notification::error(format!("{label} failed: {}", k8s::short_error(&e))),
                        cx,
                    ),
                }
                // A namespace's Applications come from a list kept a little while: read it anew.
                k8s::argo::forget_list(&hub);
                this.reload_argo(cx);
            });
        });
        self.keep_task(task);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_kubectl_shown_is_what_runs() {
        let refresh = super::argo_args("Refresh", "applications.argoproj.io", "web");
        assert_eq!(refresh, "annotate applications.argoproj.io web --overwrite argocd.argoproj.io/refresh=normal");
        let sync = super::argo_args("Sync", "applications.argoproj.io", "web");
        assert!(sync.starts_with("patch applications.argoproj.io web --type merge -p '"), "{sync}");
        assert!(sync.contains("\"syncStrategy\""), "{sync}");
    }
}
