//! Files: download and upload through the container, download the YAML.

#[allow(unused_imports)]
use super::*;

impl DetailPanel {
    /// Save the object's YAML (as shown: managedFields dropped, Secrets masked unless revealed).
    pub fn download_yaml(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use std::io::Write as _;
        let ns = self.obj.namespace.as_ref().map(|n| format!("{n}-")).unwrap_or_default();
        let name = format!("{}-{ns}{}.yaml", self.obj.kind.ar.kind.to_lowercase(), self.obj.name);
        // Private (a revealed Secret is in there) and never over an earlier download.
        let r = crate::private_files::create_new_in(&crate::private_files::downloads_dir(), &name)
            .and_then(|(mut f, path)| f.write_all(self.yaml_text.as_bytes()).map(|_| path));
        window.push_notification(
            match r {
                Ok(path) => Notification::success(format!("Saved {}", path.display())),
                Err(e) => Notification::error(format!("Could not save: {e}")),
            },
            cx,
        );
    }

    /// Ask for a path in the container, then copy it to ~/Downloads.
    pub(super) fn download_dialog(&mut self, container: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("/path/in/container");
            s.set_value("/etc/hostname", window, cx);
            s
        });
        let panel = cx.entity().downgrade();
        let (ns, pod, ctx) =
            (self.obj.namespace.clone().unwrap_or_default(), self.obj.name.clone(), self.context.clone());
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (input, panel, container) = (input.clone(), panel.clone(), container.clone());
            let path = input.read(cx).value().to_string();
            alert
                .title(format!("Download from {container}"))
                .description("A file is saved as is; a folder is saved as a .tar.")
                .child(Input::new(&input))
                .child(confirm_extras(
                    &kubectl(
                        &ctx,
                        Some(&ns),
                        &format!("cp {} ./ -c {}", shell_word(&format!("{pod}:{path}")), shell_word(&container)),
                    ),
                    None,
                    cx,
                ))
                .show_cancel(true)
                .ok_text("Download")
                .on_ok(move |_, window, cx| {
                    let path = input.read(cx).value().trim().to_string();
                    if path.is_empty() {
                        return false;
                    }
                    let container = container.clone();
                    let _ = panel.update(cx, |this, cx| {
                        let (client, ns, pod) = (
                            this.client.clone(),
                            this.obj.namespace.clone().unwrap_or_default(),
                            this.obj.name.clone(),
                        );
                        let task = cx.spawn_in(window, async move |_, cx| {
                            let r = k8s::download_from_container(client, ns, pod, Some(container), path).await;
                            let _ = cx.update(|window, cx| match r {
                                Ok(dest) => window
                                    .push_notification(Notification::success(format!("Saved {}", dest.display())), cx),
                                Err(e) => window.push_notification(
                                    Notification::error(format!("Download failed: {}", k8s::short_error(&e))),
                                    cx,
                                ),
                            });
                        });
                        this._tasks.push(task);
                    });
                    true
                })
        });
    }

    /// Pick a local file, then ask where to put it in the container.
    pub(super) fn upload_pick(&mut self, container: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Upload".into()),
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(local) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| this.upload_dialog(container, local, window, cx));
        });
        self._tasks.push(task);
    }

    pub(super) fn upload_dialog(
        &mut self,
        container: String,
        local: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The file picker came after write mode was checked.
        if !writes_still_on(window, cx) {
            return;
        }
        let input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("/tmp");
            s.set_value("/tmp", window, cx);
            s
        });
        let guard = self.protection(window, cx);
        let panel = cx.entity().downgrade();
        let (ns, pod, ctx) =
            (self.obj.namespace.clone().unwrap_or_default(), self.obj.name.clone(), self.context.clone());
        let file = local.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (input, panel, container, local, guard) =
                (input.clone(), panel.clone(), container.clone(), local.clone(), guard.clone());
            let dir = input.read(cx).value().to_string();
            alert
                .title(format!("Upload {file} to {container}"))
                .description("Folder in the container (it must exist and be writable):")
                .child(Input::new(&input))
                .child(confirm_extras(
                    &kubectl(
                        &ctx,
                        Some(&ns),
                        &format!(
                            "cp {} {} -c {}",
                            shell_word(&local.display().to_string()),
                            shell_word(&format!("{pod}:{dir}/{file}")),
                            shell_word(&container)
                        ),
                    ),
                    guard.as_ref(),
                    cx,
                ))
                .show_cancel(true)
                .ok_text("Upload")
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if !writes_still_on(window, cx) {
                        return true;
                    }
                    let dir = input.read(cx).value().trim().to_string();
                    let (container, local) = (container.clone(), local.clone());
                    let _ = panel.update(cx, |this, cx| {
                        let (client, ns, pod) = (
                            this.client.clone(),
                            this.obj.namespace.clone().unwrap_or_default(),
                            this.obj.name.clone(),
                        );
                        let task = cx.spawn_in(window, async move |_, cx| {
                            let r = k8s::upload_to_container(client, ns, pod, Some(container), local, dir).await;
                            let _ = cx.update(|window, cx| match r {
                                Ok(dest) => {
                                    window.push_notification(Notification::success(format!("Uploaded to {dest}")), cx)
                                }
                                Err(e) => window.push_notification(
                                    Notification::error(format!("Upload failed: {}", k8s::short_error(&e))),
                                    cx,
                                ),
                            });
                        });
                        this._tasks.push(task);
                    });
                    true
                })
        });
    }
}
