//! A container that isn't running (crash loop, exited, waiting) has no process to exec in:
//! its files are read from a copy of the pod instead (`k8s::pod_copy`), after one
//! confirmation. The copy is browsed like the live container, and deleted when the page goes
//! away or on the button next to it.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::tabs::error_box;
use super::{FileBrowser, Place, short};
use crate::details::{
    ConfirmButtons as _, Guard, confirm_extras, ensure_writes, guard_ok, kubectl, shell_word, writes_still_on,
};
use crate::k8s::files::FileSource;
use crate::k8s::pod_copy::{self, COPY_SECS, PodCopy};

/// The copy of the pod that stopped containers are read from.
#[derive(Default)]
pub(super) enum CopyState {
    #[default]
    None,
    Starting,
    Ready(PodCopy),
    Failed(SharedString),
}

impl FileBrowser {
    /// The pod as read: its containers' states, and the spec a copy is made from.
    pub fn set_pod(&mut self, pod: &serde_json::Value, cx: &mut Context<Self>) {
        if self.pod_read.as_ref() != Some(pod) {
            self.pod_read = Some(pod.clone());
            cx.notify();
        }
    }

    /// Whether the API server refuses this user to create pods in the namespace, or to delete
    /// them (the copy, afterwards).
    pub fn set_copy_denied(&mut self, create: bool, delete: bool, cx: &mut Context<Self>) {
        let denied = if create {
            Some("Your RBAC does not allow creating pods in this namespace (create on pods)")
        } else if delete {
            Some("Your RBAC does not allow deleting the copy afterwards (delete on pods)")
        } else {
            None
        };
        if self.copy_denied != denied {
            self.copy_denied = denied;
            cx.notify();
        }
    }

    /// Why the container shown isn't running, when it isn't.
    pub(super) fn stopped(&self) -> Option<String> {
        pod_copy::not_running(self.pod_read.as_ref()?, self.container.as_deref()?)
    }

    /// The copy, for a container read from it.
    pub(super) fn copy_source(&self, place: Option<&Place>) -> Option<FileSource> {
        let CopyState::Ready(copy) = &self.copy else { return None };
        place.filter(|p| p.copy)?;
        Some(FileSource::copy(self.client.clone(), copy.ns.clone(), copy.name.clone(), self.container.clone()))
    }

    /// Read the container shown from the copy: made once, after its confirmation.
    fn confirm_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.copy {
            CopyState::Ready(_) => {
                self.guarded(Self::browse_copy, window, cx);
                return;
            }
            CopyState::Starting => return,
            CopyState::None | CopyState::Failed(_) => {}
        }
        if !ensure_writes(window, cx) {
            return;
        }
        let (Some(container), Some(why), Some(pod)) = (self.container.clone(), self.stopped(), self.pod_read.as_ref())
        else {
            return;
        };
        let original =
            pod.pointer("/metadata/name").and_then(serde_json::Value::as_str).unwrap_or(&self.pod).to_string();
        let name = pod_copy::new_name(&original);
        let image =
            crate::settings::cluster(&self.context).node_shell_image.unwrap_or_else(|| pod_copy::DEFAULT_IMAGE.into());
        let cmd = kubectl(
            &self.context,
            Some(&self.ns),
            &format!(
                "debug {} --copy-to={name} --same-node --container={} -- sleep {COPY_SECS}",
                shell_word(&original),
                shell_word(&container)
            ),
        );
        let guard = if self.authorized { None } else { Guard::protected(&self.context, window, cx) };
        let description = format!(
            "Container {container} isn't running ({why}): nothing can run in it. Recouvrance creates pod {name}, a \
             copy of this one on the same node: same images and volumes, without its labels, owners, init \
             containers and probes, every container sleeping instead of starting its app (with a busybox from \
             {image}). It reads the files there through exec, and deletes the copy when this page closes, or after \
             {} minutes. Files the container wrote while it ran are not in the copy: it starts from the image again.",
            COPY_SECS / 60
        );
        let me = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (me, guard, name, image) = (me.clone(), guard.clone(), name.clone(), image.clone());
            alert
                .title(format!("Read container {container} from a copy of the pod?"))
                .description(description.clone())
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .confirm_buttons("Create the copy", false, guard.as_ref())
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if !writes_still_on(window, cx) {
                        return true;
                    }
                    let (name, image) = (name.clone(), image.clone());
                    let _ = me.update(cx, |this, cx| {
                        this.authorized = true;
                        this.start_copy(name, image, window, cx);
                    });
                    true
                })
        });
    }

    /// Create the copy `name`; once it runs, the container that asked for it opens there.
    fn start_copy(&mut self, name: String, image: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pod) = self.pod_read.clone() else { return };
        let (client, key) = (self.client.clone(), self.key());
        self.copy = CopyState::Starting;
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = pod_copy::create(client, pod, name, image).await;
            let made = result.as_ref().ok().cloned();
            let shown = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(copy) => {
                        this.copy = CopyState::Ready(copy);
                        if this.key() == key {
                            this.browse_copy(window, cx);
                        }
                    }
                    Err(e) => this.copy = CopyState::Failed(short(&e)),
                }
                cx.notify();
            });
            // The page went away while the copy started: nobody will read it.
            if shown.is_err()
                && let Some(copy) = made
            {
                pod_copy::delete(copy);
            }
        });
        self._copy_task = Some(task);
        cx.notify();
    }

    /// Read the container shown from the copy, in the folder it was in.
    fn browse_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.container.is_none() {
            return;
        }
        let key = self.key();
        let place = self.places.entry(key).or_default();
        place.copy = true;
        let dir = place.dir.clone().unwrap_or_else(|| b"/".to_vec());
        self.open_dir(dir, None, window, cx);
    }

    /// Delete the copy; the containers read from it go back to their start.
    fn end_copy(&mut self, cx: &mut Context<Self>) {
        if let CopyState::Ready(copy) = std::mem::take(&mut self.copy) {
            pod_copy::delete(copy);
        }
        self._copy_task = None;
        self.places.retain(|_, p| !p.copy);
        self.previewed = None;
        cx.notify();
    }

    /// The button that reads the container from a copy, greyed out with the reason when
    /// RBAC refuses what it needs.
    pub(super) fn copy_button(&self, cx: &mut Context<Self>) -> Button {
        let button = Button::new("browse-copy").primary().icon(Lucide::Copy).label("Browse a copy of the pod");
        if matches!(self.copy, CopyState::Starting) {
            return button.loading(true).disabled(true).label("Starting the copy…");
        }
        let denied = if matches!(self.copy, CopyState::Ready(_)) { None } else { self.copy_denied };
        match denied {
            Some(reason) => button.disabled(true).tooltip(reason),
            None if self.exec_denied => {
                button.disabled(true).tooltip("Your RBAC does not allow exec in this pod (create on pods/exec)")
            }
            None => button.on_click(cx.listener(|this, _, window, cx| this.confirm_copy(window, cx))),
        }
    }

    /// Before browsing a container that isn't running: why, and the copy offered instead.
    pub(super) fn render_copy_start(&self, why: &str, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let container = self.container.clone().unwrap_or_default();
        let failed = match &self.copy {
            CopyState::Failed(e) => Some(div().max_w(px(640.)).child(error_box(e.clone(), cx))),
            _ => None,
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().text_sm().text_color(muted).child(format!(
                "Container {container} of {}/{} isn't running ({why}): nothing can run in it.",
                self.ns, self.pod
            )))
            .when_some(self.container_select.clone(), |d, s| {
                d.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_sm()
                        .text_color(muted)
                        .child("Container")
                        .child(div().w(px(220.)).child(Select::new(&s).small())),
                )
            })
            .child(self.copy_button(cx))
            .children(failed)
            .child(div().max_w(px(520.)).text_xs().text_color(muted).child(format!(
                "Its files are read from a copy of the pod on the same node, with the same images and volumes and \
                 every container asleep, deleted when this page closes (after {} minutes at most). Files the \
                 container wrote while it ran are not in the copy. It needs write mode, and RBAC to create and \
                 delete pods and to exec in them.",
                COPY_SECS / 60
            )))
            .into_any_element()
    }

    /// Above a container read from the copy: what it is, and what it is not.
    pub(super) fn render_copy_note(&self, place: &Place, cx: &mut Context<Self>) -> Option<AnyElement> {
        let CopyState::Ready(copy) = &self.copy else { return None };
        if !place.copy {
            return None;
        }
        let theme = cx.theme().clone();
        let ends = copy.ends.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string();
        Some(
            h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .items_center()
                .rounded_sm()
                .bg(theme.secondary)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(Lucide::Info).size_3p5())
                .child(div().flex_1().child(format!(
                    "Read from pod {}, a copy of this one started from the same images with the same volumes: files \
                     the container wrote while it ran are not here. {} holds the copy's busybox. It stops at {ends}, \
                     and is deleted when this page closes.",
                    copy.name,
                    pod_copy::TOOLS
                )))
                .child(
                    Button::new("files-end-copy")
                        .xsmall()
                        .outline()
                        .icon(Lucide::Trash)
                        .label("Delete the copy")
                        .on_click(cx.listener(|this, _, _, cx| this.end_copy(cx))),
                )
                .into_any_element(),
        )
    }
}

impl Drop for FileBrowser {
    fn drop(&mut self) {
        if let CopyState::Ready(copy) = std::mem::take(&mut self.copy) {
            pod_copy::delete(copy);
        }
    }
}
