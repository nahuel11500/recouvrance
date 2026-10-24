//! Write actions and their confirmations: scale, restart, rollback, delete, debug, node shell, RBAC bindings, files, YAML saves.

#[allow(unused_imports)]
use super::*;

impl DetailPanel {
    pub(super) fn run_write(
        &mut self,
        label: &'static str,
        fut: impl Future<Output = Result<String>> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_write_then(label, fut, false, window, cx);
    }

    /// Run a write, then re-read the object and its events once it is done.
    /// `reset_yaml`: on success, the YAML editor takes the server's version
    /// even though its text was edited (it was what got saved).
    pub(super) fn run_write_then(
        &mut self,
        label: &'static str,
        fut: impl Future<Output = Result<String>> + 'static,
        reset_yaml: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let ok = result.is_ok();
                match result {
                    Ok(msg) => window.push_notification(Notification::success(msg), cx),
                    Err(e) => window.push_notification(
                        Notification::error(format!("{label} failed: {}", k8s::short_error(&e))),
                        cx,
                    ),
                }
                this.refresh_object(reset_yaml && ok, window, cx);
                this.load_events(cx);
            });
        });
        self.keep_task(task);
    }

    pub(super) fn confirm_scale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        // Never a made-up count: unknown stays empty and must be typed.
        let current = self.object.as_ref().and_then(|o| o.pointer("/spec/replicas")).and_then(|v| v.as_i64());
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("replicas");
            if let Some(c) = current {
                s.set_value(c.to_string(), window, cx);
            }
            s
        });
        let panel = cx.entity().downgrade();
        let title = format!("Scale {} {}", self.obj.kind.ar.kind, self.obj.name);
        let cmd = self.kubectl_for("Scale");
        let guard = self.protection(window, cx);
        let description = match current {
            Some(c) => format!("Currently {c} replica{}. New replica count:", if c == 1 { "" } else { "s" }),
            None => "The current replica count is unknown. New replica count:".to_string(),
        };
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (input, panel, guard) = (input.clone(), panel.clone(), guard.clone());
            let typed = input.read(cx).value().trim().to_string();
            let cmd =
                cmd.replace("--replicas=…", &format!("--replicas={}", if typed.is_empty() { "…" } else { &typed }));
            alert
                .title(title.clone())
                .description(description.clone())
                .child(Input::new(&input))
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Scale")
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let Some(n) = parse_replicas(&input.read(cx).value()) else { return false };
                    let _ = panel.update(cx, |this, cx| {
                        let (c, k, ns, name) = (
                            this.client.clone(),
                            this.obj.kind.clone(),
                            this.obj.namespace.clone(),
                            this.obj.name.clone(),
                        );
                        this.run_write("Scale", k8s::scale(c, k, ns, name, n, false), window, cx);
                    });
                    true
                })
        });
    }

    /// Write actions are visible always; using one first offers to enable write mode.
    pub(super) fn ensure_writes(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        ensure_writes(window, cx)
    }

    /// The kubectl command equivalent to a write action, shown in confirmations.
    pub(super) fn kubectl_for(&self, action: &str) -> String {
        let (kind, name) = (self.obj.kind.kubectl_name(), shell_word(&self.obj.name));
        // The same patch the write sends, so what is shown is what runs.
        let patch =
            |v: serde_json::Value| format!("patch {kind} {name} --type merge -p {}", k8s::shell_quote(&v.to_string()));
        let args = match action {
            // The Job gets a unique name, built like the real one.
            "Run" => format!(
                "create job --from=cronjob/{name} {}$(($(date +%s) % 100000))",
                shell_word(&k8s::manual_job_prefix(&self.obj.name))
            ),
            "Cordon" => format!("cordon {name}"),
            "Uncordon" => format!("uncordon {name}"),
            "Drain" => format!("drain {name} --ignore-daemonsets --delete-emptydir-data"),
            "Suspend" => patch(serde_json::json!({"spec": {"suspend": true}})),
            "Resume" => patch(serde_json::json!({"spec": {"suspend": false}})),
            "Reconcile" => format!("annotate {kind} {name} --overwrite reconcile.fluxcd.io/requestedAt=$(date +%s)"),
            "Refresh" => format!("annotate {kind} {name} --overwrite argocd.argoproj.io/refresh=normal"),
            "Sync" => patch(k8s::argo_sync_patch()),
            "Restart" => format!("rollout restart {kind}/{name}"),
            "Scale" => format!("scale {kind}/{name} --replicas=…"),
            "Delete" => format!("delete {kind} {name}"),
            "Unfinalize" => patch(serde_json::json!({"metadata": {"finalizers": null}})),
            "Save" => format!("replace -f {}", shell_word(&format!("{}.yaml", self.obj.name))),
            "Debug" => format!("debug -it {name} --image=busybox:1.37"),
            other => format!("{} {kind}/{name}", other.to_lowercase()),
        };
        kubectl(&self.context, self.obj.namespace.as_deref(), &args)
    }

    /// The uid of the object on screen: a delete hits that object, not one re-created under
    /// its name since.
    fn shown_uid(&self) -> Option<String> {
        let uid = self.object.as_ref()?.pointer("/metadata/uid")?.as_str()?;
        Some(uid.to_string())
    }

    /// On a protected context, an input where the context name must be typed.
    pub(super) fn protection(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(Entity<InputState>, String)> {
        crate::settings::is_protected(&self.context).then(|| {
            let ctx = self.context.clone();
            (cx.new(|cx| InputState::new(window, cx).placeholder(ctx.clone())), ctx)
        })
    }

    /// Confirm, then run a write built from this panel's state.
    pub(super) fn confirm_action(
        &mut self,
        title: String,
        description: &'static str,
        ok: &'static str,
        action: fn(&DetailPanel) -> WriteFuture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let panel = cx.entity().downgrade();
        let cmd = self.kubectl_for(ok);
        let guard = self.protection(window, cx);
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (panel, guard) = (panel.clone(), guard.clone());
            alert
                .title(title.clone())
                .description(description)
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text(ok)
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let _ = panel.update(cx, |this, cx| {
                        let fut = action(this);
                        this.run_write(ok, fut, window, cx);
                    });
                    true
                })
        });
    }

    /// Roll back to an older revision, after showing what changes (the Revisions diff).
    pub fn confirm_rollback(&mut self, revision: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let Some(rev) = self
            .related
            .revisions
            .iter()
            .find(|r| r.pointer("/metadata/name").and_then(|v| v.as_str()) == Some(revision.as_str()))
            .cloned()
        else {
            return;
        };
        self.compare_rev = Some(revision.clone());
        cx.notify();
        let number = rev
            .pointer("/metadata/annotations/deployment.kubernetes.io~1revision")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let template = rev.pointer("/spec/template").cloned().unwrap_or_default();
        let current = self.object.as_ref().and_then(|o| o.pointer("/spec/template")).cloned().unwrap_or_default();
        let (added, removed) = crate::overview::template_changes(&current, &template);
        let panel = cx.entity().downgrade();
        let cmd = kubectl(
            &self.context,
            self.obj.namespace.as_deref(),
            &format!(
                "rollout undo {}/{} --to-revision={}",
                self.obj.kind.kubectl_name(),
                shell_word(&self.obj.name),
                shell_word(&number)
            ),
        );
        let guard = self.protection(window, cx);
        let title = format!("Roll {} {} back to revision {number}?", self.obj.kind.ar.kind, self.obj.name);
        // Validate with a server dry run first, like YAML edits.
        let dry = k8s::rollback(
            self.client.clone(),
            self.obj.kind.clone(),
            self.obj.namespace.clone(),
            self.obj.name.clone(),
            template.clone(),
            true,
        );
        let task = cx.spawn_in(window, async move |_, cx| {
            let check = dry.await;
            let _ = cx.update(|window, cx| {
                // Write mode may have been turned off during the dry run.
                if check.is_ok() && !writes_still_on(window, cx) {
                    return;
                }
                window.open_alert_dialog(cx, move |alert, _, cx| {
                    let (panel, guard, template) = (panel.clone(), guard.clone(), template.clone());
                    let summary = match &check {
                        Ok(_) => format!("The pod template goes back to revision {number}: {added} lines added, {removed} removed (diff shown in the Revisions tab). The server dry run passed; a new rollout starts."),
                        // Nothing to confirm: the real rollback would fail the same way.
                        Err(e) => return alert
                            .title(title.clone())
                            .description(format!("The server dry run failed, so nothing was changed: {e:#}"))
                            .ok_text("Close"),
                    };
                    alert
                        .title(title.clone())
                        .description(summary)
                        .child(confirm_extras(&cmd, guard.as_ref(), cx))
                        .show_cancel(true)
                        .ok_text("Roll back")
                        .on_ok(move |_, window, cx| {
                            if !guard_ok(guard.as_ref(), cx) {
                                return false;
                            }
                            if !writes_still_on(window, cx) {
                                return true;
                            }
                            let template = template.clone();
                            let _ = panel.update(cx, |this, cx| {
                                let fut = k8s::rollback(this.client.clone(), this.obj.kind.clone(), this.obj.namespace.clone(), this.obj.name.clone(), template, false);
                                this.run_write("Roll back", fut, window, cx);
                            });
                            true
                        })
                });
            });
        });
        self.keep_task(task);
    }

    pub(super) fn confirm_restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_action(
            format!("Restart {} {}?", self.obj.kind.ar.kind, self.obj.name),
            "Pods are replaced one by one, like `kubectl rollout restart`.",
            "Restart",
            |p| {
                Box::pin(k8s::rollout_restart(
                    p.client.clone(),
                    p.obj.kind.clone(),
                    p.obj.namespace.clone(),
                    p.obj.name.clone(),
                    false,
                ))
            },
            window,
            cx,
        );
    }

    /// A root shell on the node, through a privileged pod.
    pub(super) fn confirm_node_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let guard = self.protection(window, cx);
        let (node, context) = (self.obj.name.clone(), self.context.clone());
        let image = crate::settings::cluster(&self.context).node_shell_image.unwrap_or_else(|| "busybox:1.37".into());
        let overrides =
            format!("{{\"spec\":{{\"nodeName\":{},\"hostPID\":true,…privileged…}}}}", serde_json::json!(node));
        let cmd = kubectl(
            &context,
            Some(k8s::NODE_SHELL_NS),
            &format!(
                "run node-shell --image={} --overrides={} -it -- nsenter -t 1 -m -u -i -n -p -- sh",
                shell_word(&image),
                k8s::shell_quote(&overrides)
            ),
        );
        let panel = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (guard, panel) = (guard.clone(), panel.clone());
            alert
                .title(format!("Open a root shell on node {node}?"))
                .description(
                    "Starts a privileged pod on the node that enters the host's namespaces: you get root on the node. \
                     The pod is deleted when its shell ends or this page closes, and after 4 hours at most.",
                )
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Open shell")
                .ok_variant(component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let _ = panel.update(cx, |this, cx| this.start_node_shell(window, cx));
                    true
                })
        });
    }

    pub(super) fn start_node_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, node) = (self.client.clone(), self.obj.name.clone());
        let image = crate::settings::cluster(&self.context).node_shell_image.unwrap_or_else(|| "busybox:1.37".into());
        window.push_notification(Notification::info("Starting a node shell pod…"), cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = k8s::create_node_shell(client.clone(), node, image, false).await;
            let created = result.as_ref().ok().cloned();
            let shown = this.update_in(cx, |this, window, cx| match result {
                Ok(pod) => {
                    if let Some(old) = this.node_shell.replace(pod.clone()) {
                        k8s::delete_node_shell(client.clone(), old);
                    }
                    // Write mode still applies to every connect; the context name was typed already.
                    let ctx = this.context.clone();
                    let terminal = cx.new(|cx| {
                        TerminalView::new(client.clone(), k8s::NODE_SHELL_NS.into(), pod, None, true, cx)
                            .guarded(ctx)
                            .preauthorized()
                    });
                    // The privileged pod lives only as long as its shell.
                    let sub = cx.subscribe(&terminal, |this, _, _: &crate::ui::terminal::SessionEnded, cx| {
                        this.end_node_shell(cx);
                    });
                    this._subs.push(sub);
                    this.terminal = Some(terminal);
                    this.tab = DetailTab::Shell;
                    cx.notify();
                }
                Err(e) => window
                    .push_notification(Notification::error(format!("Node shell failed: {}", k8s::short_error(&e))), cx),
            });
            // The page went away while the pod started: nobody will use it.
            if shown.is_err()
                && let Some(pod) = created
            {
                k8s::delete_node_shell(client, pod);
            }
        });
        self.keep_task(task);
    }

    /// Delete the node shell pod and drop its terminal.
    pub(super) fn end_node_shell(&mut self, cx: &mut Context<Self>) {
        let Some(pod) = self.node_shell.take() else { return };
        k8s::delete_node_shell(self.client.clone(), pod);
        self.terminal = None;
        if self.tab == DetailTab::Shell {
            self.tab = DetailTab::Overview;
        }
        cx.notify();
    }

    /// Show (or hide) the key/value editor of a ConfigMap or Secret.
    pub fn toggle_data_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_editor.take().is_some() {
            cx.notify();
            return;
        }
        let Some(obj) = self.object.clone() else { return };
        let (ctx, client, kind, ns) =
            (self.context.clone(), self.client.clone(), self.obj.kind.clone(), self.obj.namespace.clone());
        let editor = cx.new(|cx| crate::ui::data_edit::DataEditor::new(ctx, client, kind, ns, Some(obj), window, cx));
        let sub = cx.subscribe_in(&editor, window, |this, _, _: &crate::ui::data_edit::DataSaved, window, cx| {
            this.data_editor = None;
            this.refresh_object(false, window, cx);
            this.load_events(cx);
        });
        self._subs.push(sub);
        self.data_editor = Some(editor);
        self.tab = DetailTab::Overview;
        cx.notify();
    }

    /// Add an ephemeral debug container, then open a shell in it.
    pub(super) fn confirm_debug(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let containers: Vec<String> = self
            .object
            .as_ref()
            .and_then(|o| o.pointer("/spec/containers"))
            .and_then(|c| c.as_array())
            .map(|a| a.iter().filter_map(|c| c["name"].as_str().map(String::from)).collect())
            .unwrap_or_default();
        let image = cx.new(|cx| {
            let mut s = InputState::new(window, cx);
            s.set_value("busybox:1.37", window, cx);
            s
        });
        let target = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("none (own process namespace)");
            s.set_value(containers.first().cloned().unwrap_or_default(), window, cx);
            s
        });
        let guard = self.protection(window, cx);
        let panel = cx.entity().downgrade();
        let (ns, pod, context) =
            (self.obj.namespace.clone().unwrap_or_default(), self.obj.name.clone(), self.context.clone());
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (image, target, guard, panel) = (image.clone(), target.clone(), guard.clone(), panel.clone());
            let (img, tgt) = (image.read(cx).value().to_string(), target.read(cx).value().to_string());
            let cmd = kubectl(
                &context,
                Some(&ns),
                &format!(
                    "debug -it {} --image={}{}",
                    shell_word(&pod),
                    shell_word(&img),
                    if tgt.is_empty() { String::new() } else { format!(" --target={}", shell_word(&tgt)) }
                ),
            );
            alert
                .title(format!("Debug pod {pod}?"))
                .description("Adds an ephemeral container to the running pod and opens a shell in it. It stays until the pod is replaced.")
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child("Image"))
                        .child(Input::new(&image))
                        .child(div().text_sm().pt_1().child("Share processes with container"))
                        .child(Input::new(&target)),
                )
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Debug")
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let (img, tgt) = (image.read(cx).value().trim().to_string(), target.read(cx).value().trim().to_string());
                    if img.is_empty() {
                        return false;
                    }
                    let _ = panel.update(cx, |this, cx| this.start_debug(img, (!tgt.is_empty()).then_some(tgt), window, cx));
                    true
                })
        });
    }

    pub(super) fn start_debug(
        &mut self,
        image: String,
        target: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (client, ns, pod) =
            (self.client.clone(), self.obj.namespace.clone().unwrap_or_default(), self.obj.name.clone());
        window.push_notification(Notification::info(format!("Starting a debug container ({image})…")), cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = k8s::add_debug_container(client.clone(), ns.clone(), pod.clone(), image, target, false).await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(container) => {
                    let risky = crate::settings::is_protected(&this.context);
                    // Write mode still applies to every connect; the context name was typed already.
                    let ctx = this.context.clone();
                    // In a tab of its own: a shell open in the pod's containers stays.
                    this.debug_terminal = Some(cx.new(|cx| {
                        TerminalView::new(client, ns, pod, Some(container.clone()), risky, cx)
                            .guarded(ctx)
                            .preauthorized()
                    }));
                    this.tab = DetailTab::Debug;
                    window
                        .push_notification(Notification::success(format!("Shell in debug container {container}")), cx);
                    cx.notify();
                }
                Err(e) => {
                    window.push_notification(Notification::error(format!("Debug failed: {}", k8s::short_error(&e))), cx)
                }
            });
        });
        self.keep_task(task);
    }

    /// Delete, asking first as set for this kind (Settings → General): not at all, with a
    /// dialog, or with the name typed. A protected context always asks for its name, and an
    /// object stuck terminating always shows the dialog (to remove its finalizers).
    pub(super) fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let level = crate::settings::delete_confirm(&self.obj.kind.ar.group, &self.obj.kind.ar.kind);
        let guard = self.protection(window, cx);
        let terminating = self.object.as_ref().is_some_and(|o| o.pointer("/metadata/deletionTimestamp").is_some());
        if level == crate::settings::DeleteConfirm::Off && guard.is_none() && !terminating {
            let (c, k, ns, n) =
                (self.client.clone(), self.obj.kind.clone(), self.obj.namespace.clone(), self.obj.name.clone());
            let uid = self.shown_uid();
            self.run_write("Delete", k8s::delete_with(c, k, ns, n, uid, false, false), window, cx);
            return;
        }
        let typed = level == crate::settings::DeleteConfirm::Type;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(self.obj.name.clone()));
        let panel = cx.entity().downgrade();
        let (name, context) = (self.obj.name.clone(), self.context.clone());
        let ar = self.obj.kind.ar.clone();
        let label = self.obj.kind.label.clone();
        let opts = cx.new(|_| (false, false));
        let base = self.kubectl_for("Delete");
        let unfinalize = self.kubectl_for("Unfinalize");
        // The object shown now: the refresh may swap in one re-created under its name while
        // the dialog is open, and that one must not be deleted.
        let uid = self.shown_uid();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (input, panel, name, guard, uid) =
                (input.clone(), panel.clone(), name.clone(), guard.clone(), uid.clone());
            let (force, strip) = *opts.read(cx);
            let mut cmd = if force { format!("{base} --grace-period=0 --force") } else { base.clone() };
            // Delete first; finalizers are cleared only when it then stays, stuck terminating.
            if strip {
                cmd = format!("{cmd} --timeout={}s || {unfinalize}", k8s::FINALIZER_GRACE_SECS);
            }
            let (o1, o2) = (opts.clone(), opts.clone());
            alert
                .title(format!("Delete {} {name}?", ar.kind))
                .description(if typed {
                    format!("This deletes it from {context}. Type the name to confirm.")
                } else {
                    format!("This deletes it from {context}.")
                })
                .when(typed, |a| a.child(Input::new(&input)))
                .child(
                    v_flex()
                        .gap_1()
                        .pt_2()
                        .child(Checkbox::new("del-force").label("Force (no grace period)").checked(force).on_click(
                            move |v: &bool, _, cx| {
                                let v = *v;
                                o1.update(cx, |o, cx| {
                                    o.0 = v;
                                    cx.notify();
                                });
                                cx.refresh_windows();
                            },
                        ))
                        .child(
                            Checkbox::new("del-finalizers")
                                .label(if terminating {
                                    "Remove finalizers (it is stuck terminating)"
                                } else {
                                    "Remove finalizers"
                                })
                                .checked(strip)
                                .on_click(move |v: &bool, _, cx| {
                                    let v = *v;
                                    o2.update(cx, |o, cx| {
                                        o.1 = v;
                                        cx.notify();
                                    });
                                    cx.refresh_windows();
                                }),
                        )
                        .when(strip, |d| {
                            d.child(div().text_sm().text_color(cx.theme().warning).child(
                                "Finalizers let controllers clean up (volumes, cloud resources, dependents): \
                                 removing them can leave those behind for good.",
                            ))
                        }),
                )
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .child(delete_settings_link(&ar.group, &ar.kind, &label, cx))
                .show_cancel(true)
                .ok_text("Delete")
                .ok_variant(component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    if (typed && input.read(cx).value().trim() != name) || !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let _ = panel.update(cx, |this, cx| {
                        let (c, k, ns, n) = (
                            this.client.clone(),
                            this.obj.kind.clone(),
                            this.obj.namespace.clone(),
                            this.obj.name.clone(),
                        );
                        let uid = uid.clone();
                        this.run_write("Delete", k8s::delete_with(c, k, ns, n, uid, force, strip), window, cx);
                    });
                    true
                })
        });
    }

    /// Validate the edited YAML with a server dry run, then confirm and save.
    pub(super) fn apply_yaml(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let text = self.yaml.read(cx).value().to_string();
        let obj: serde_json::Value = match crate::yaml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let at = e.location().map(|l| format!(" at line {}", l.line())).unwrap_or_default();
                let msg = format!("Invalid YAML{at}: {}", crate::yaml::message(&e));
                window.push_notification(Notification::error(msg), cx);
                return;
            }
        };
        if has_masked_values(&obj) {
            window.push_notification(Notification::error("Reveal secret values before editing a Secret."), cx);
            return;
        }
        let k = &self.obj.kind;
        if let Some(why) = identity_change(&obj, &k.ar.group, &k.ar.kind, self.obj.namespace.as_deref(), &self.obj.name)
        {
            window.push_notification(
                Notification::error(format!(
                    "{why}: this editor only saves this object. Use Create / apply for others."
                )),
                cx,
            );
            return;
        }
        let (client, kind, ns) = (self.client.clone(), self.obj.kind.clone(), self.obj.namespace.clone());
        let check = k8s::replace(client, kind, ns, obj.clone(), true);
        let panel = cx.entity().downgrade();
        let cmd = self.kubectl_for("Save");
        let guard = self.protection(window, cx);
        let task = cx.spawn_in(window, async move |_, cx| {
            let result = check.await;
            let _ = cx.update(|window, cx| match result {
                Err(e) => {
                    window.push_notification(Notification::error(format!("The server rejected the change: {e:#}")), cx)
                }
                Ok(_) if !writes_still_on(window, cx) => {}
                Ok(_) => window.open_alert_dialog(cx, move |alert, _, cx| {
                    let (panel, obj, guard) = (panel.clone(), obj.clone(), guard.clone());
                    alert
                        .title("Apply YAML changes?")
                        .description("The server accepted them in a dry run. Save them now?")
                        .child(confirm_extras(&cmd, guard.as_ref(), cx))
                        .show_cancel(true)
                        .ok_text("Save")
                        .on_ok(move |_, window, cx| {
                            if !guard_ok(guard.as_ref(), cx) {
                                return false;
                            }
                            if !writes_still_on(window, cx) {
                                return true;
                            }
                            let obj = obj.clone();
                            let _ = panel.update(cx, |this, cx| {
                                let (c, k, ns) =
                                    (this.client.clone(), this.obj.kind.clone(), this.obj.namespace.clone());
                                // Saved: the editor shows the server's version again, which
                                // the live refresh then keeps current.
                                this.run_write_then("Save", k8s::replace(c, k, ns, obj, false), true, window, cx);
                            });
                            true
                        })
                }),
            });
        });
        self.keep_task(task);
    }
}

/// A replica count typed by the user (a whole number, 0 or more).
pub(crate) fn parse_replicas(text: &str) -> Option<i32> {
    text.trim().parse::<i32>().ok().filter(|n| *n >= 0)
}

/// A word in a shell command shown to the user, quoted only when it has to be.
pub(crate) fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@=,+".contains(c)) {
        s.to_string()
    } else {
        k8s::shell_quote(s)
    }
}

/// `kubectl --context C [-n NS] ARGS`, the context and namespace quoted for a shell.
pub(crate) fn kubectl(context: &str, ns: Option<&str>, args: &str) -> String {
    let ns = ns.map(|n| format!(" -n {}", shell_word(n))).unwrap_or_default();
    format!("kubectl --context {}{ns} {args}", shell_word(context))
}

/// Masked Secret values (in `data`, `stringData` or the last-applied annotation)
/// would be saved as the mask itself.
pub(crate) fn has_masked_values(obj: &serde_json::Value) -> bool {
    const MASK: &str = "••••••";
    let in_map = |field: &str| {
        obj.get(field).and_then(|d| d.as_object()).is_some_and(|d| d.values().any(|v| v.as_str() == Some(MASK)))
    };
    let annotation = obj
        .pointer("/metadata/annotations/kubectl.kubernetes.io~1last-applied-configuration")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v.contains(MASK));
    in_map("data") || in_map("stringData") || annotation
}

/// Why an edited object is not the one the page shows (another kind, API group,
/// namespace or name), if it is not. No namespace means the page's.
pub(crate) fn identity_change(
    edited: &serde_json::Value,
    group: &str,
    kind: &str,
    ns: Option<&str>,
    name: &str,
) -> Option<String> {
    let api_version = edited["apiVersion"].as_str().unwrap_or_default();
    let edited_group = api_version.rsplit_once('/').map_or("", |(g, _)| g);
    let edited_kind = edited["kind"].as_str().unwrap_or_default();
    if edited_kind != kind || edited_group != group {
        return Some(format!("The kind changed to {edited_kind} ({api_version})"));
    }
    if let Some(e) = edited.pointer("/metadata/namespace").and_then(|v| v.as_str())
        && Some(e) != ns
    {
        return Some(format!("The namespace changed to {e}"));
    }
    let edited_name = edited.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default();
    (edited_name != name).then(|| format!("The name changed to {edited_name}"))
}

/// A write started from a detail page (built from the panel's state).
pub(super) type WriteFuture = std::pin::Pin<Box<dyn Future<Output = Result<String>>>>;

/// The kubectl equivalent of a confirmation, plus the protected-context input.
pub fn confirm_extras(cmd: &str, guard: Option<&(Entity<InputState>, String)>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    v_flex()
        .gap_2()
        .pt_1()
        .when(!cmd.is_empty(), |d| {
            d.child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(theme.secondary)
                    .font_family("monospace")
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(cmd.to_string()),
            )
        })
        .when_some(guard, |d, (input, ctx)| {
            d.child(
                div()
                    .text_sm()
                    .text_color(theme.warning)
                    .child(format!("{ctx} is protected. Type its name to confirm:")),
            )
            .child(Input::new(input))
        })
        .into_any_element()
}

/// "Change what deleting asks…": a link under delete dialogs to their setting.
pub fn delete_settings_link(group: &str, kind: &str, label: &str, cx: &App) -> AnyElement {
    let text = match crate::settings::delete_confirm(group, kind) {
        crate::settings::DeleteConfirm::Off => format!("{label} are deleted without asking."),
        crate::settings::DeleteConfirm::Ask => format!("Deleting {label} asks first."),
        crate::settings::DeleteConfirm::Type => format!("Deleting {label} asks for the name."),
    };
    h_flex()
        .pt_1()
        .gap_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .child(
            div()
                .id("delete-settings")
                .text_color(cx.theme().blue)
                .cursor_pointer()
                .hover(|s| s.underline())
                .child("Change in Settings")
                .on_click(|_, window, cx| crate::app::open_delete_settings(window, cx)),
        )
        .into_any_element()
}

pub fn guard_ok(guard: Option<&(Entity<InputState>, String)>, cx: &App) -> bool {
    guard.is_none_or(|(input, ctx)| input.read(cx).value().trim() == ctx)
}

#[cfg(test)]
mod tests {
    use super::{has_masked_values, identity_change, kubectl, parse_replicas, shell_word};

    #[test]
    fn replicas_must_be_typed() {
        assert_eq!(parse_replicas(""), None);
        assert_eq!(parse_replicas(" 3 "), Some(3));
        assert_eq!(parse_replicas("0"), Some(0));
        assert_eq!(parse_replicas("-1"), None);
        assert_eq!(parse_replicas("two"), None);
    }

    #[test]
    fn kubectl_quotes_what_needs_it() {
        assert_eq!(kubectl("prod-eu", Some("team-a"), "get pods"), "kubectl --context prod-eu -n team-a get pods");
        assert_eq!(
            kubectl("arn:aws:eks:eu-west-1:1:cluster/x", None, "get ns"),
            "kubectl --context arn:aws:eks:eu-west-1:1:cluster/x get ns"
        );
        assert_eq!(kubectl("my ctx", Some("a'b"), "x"), "kubectl --context 'my ctx' -n 'a'\\''b' x");
        assert_eq!(shell_word("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(shell_word(""), "''");
    }

    #[test]
    fn masked_values_are_refused() {
        let m = "••••••";
        assert!(has_masked_values(&serde_json::json!({"data": {"a": m}})));
        assert!(has_masked_values(&serde_json::json!({"stringData": {"a": m}})));
        let annotated = serde_json::json!({"metadata": {"annotations": {
            "kubectl.kubernetes.io/last-applied-configuration": m
        }}});
        assert!(has_masked_values(&annotated));
        assert!(!has_masked_values(&serde_json::json!({"data": {"a": "eA=="}, "stringData": {"b": "x"}})));
    }

    #[test]
    fn edits_stay_on_the_same_object() {
        let obj = |v: &str, k: &str, ns: Option<&str>, n: &str| {
            let mut o = serde_json::json!({"apiVersion": v, "kind": k, "metadata": {"name": n}});
            if let Some(ns) = ns {
                o["metadata"]["namespace"] = ns.into();
            }
            o
        };
        let check = |o: &serde_json::Value| identity_change(o, "apps", "Deployment", Some("team-a"), "web");
        assert_eq!(check(&obj("apps/v1", "Deployment", Some("team-a"), "web")), None);
        assert_eq!(check(&obj("apps/v1", "Deployment", None, "web")), None);
        assert!(check(&obj("apps/v1", "Deployment", Some("team-b"), "web")).is_some());
        assert!(check(&obj("apps/v1", "Deployment", Some("team-a"), "api")).is_some());
        assert!(check(&obj("apps/v1", "StatefulSet", Some("team-a"), "web")).is_some());
        assert!(check(&obj("extensions/v1beta1", "Deployment", Some("team-a"), "web")).is_some());
        // Core group, cluster-scoped.
        assert_eq!(identity_change(&obj("v1", "Namespace", None, "x"), "", "Namespace", None, "x"), None);
        assert!(identity_change(&obj("v1", "Namespace", Some("y"), "x"), "", "Namespace", None, "x").is_some());
    }
}
