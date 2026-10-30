//! The Files tab of a pod: a container's filesystem as a folder tree, read through exec
//! (`k8s::files`). Open folders, preview a text file, download a file or a folder, search
//! by name. Every command runs in the container, so like a shell it needs write mode, and
//! on a protected context its name typed once for the tab. A container that isn't running
//! is read from a copy of the pod (`copy`).

mod copy;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use super::ConfirmButtons as _;
use super::tabs::error_box;
use crate::k8s;
use crate::k8s::files::{self, Entry, FileKind, FileSource, Found, Preview};
use crate::latest::Latest;
use copy::CopyState;

/// The most of a file previewed.
const PREVIEW_BYTES: usize = 256 * 1024;

pub struct FileBrowser {
    client: Client,
    context: String,
    ns: String,
    pod: String,
    /// The pod's containers (default first), once it is read.
    containers: Vec<String>,
    /// The container shown; none until the pod is read (its default one then).
    container: Option<String>,
    container_select: Option<Entity<SelectState<SearchableVec<SharedString>>>>,
    _container_sub: Option<Subscription>,
    /// Each container's folder, listing, preview and search, kept while another is shown.
    places: HashMap<String, Place>,
    /// The API server says this user may not exec in the pod.
    exec_denied: bool,
    /// The pod as last read: its containers' states, and the spec a copy is made from.
    pod_read: Option<serde_json::Value>,
    /// Why RBAC refuses a copy of the pod (create or delete pods), if it does.
    copy_denied: Option<&'static str>,
    /// The copy of the pod that stopped containers are read from, and its creation.
    copy: CopyState,
    _copy_task: Option<Task<()>>,
    /// The protected context's name was typed (or its wait passed) once for this tab.
    authorized: bool,
    /// A name to search for, or a path to open.
    query: Entity<InputState>,
    preview: Entity<EditorState>,
    /// The container and path whose text the preview editor holds.
    previewed: Option<(String, Vec<u8>)>,
    _downloads: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

/// Where one container's browsing is.
#[derive(Default)]
struct Place {
    /// The folder shown; none until browsing starts.
    dir: Option<Vec<u8>>,
    listing: Option<Result<Vec<Entry>, SharedString>>,
    /// The entry picked in the folder, by name.
    selected: Option<Vec<u8>>,
    /// Open (or preview) this entry once the folder is listed: a search result, a path typed.
    pending: Option<Vec<u8>>,
    preview: Option<Result<Preview, SharedString>>,
    /// The last search in the folder: what was looked for, and what was found.
    search: Option<(String, Option<Result<Found, SharedString>>)>,
    /// One load of each kind at a time: a newer one replaces (and stops) the one running.
    list_load: Latest,
    preview_load: Latest,
    search_load: Latest,
    /// Read from the copy of the pod: the container wasn't running.
    copy: bool,
}

impl Place {
    fn entries(&self) -> Option<&[Entry]> {
        self.listing.as_ref()?.as_ref().ok().map(Vec::as_slice)
    }

    /// The entry picked, as listed.
    fn selected_entry(&self) -> Option<&Entry> {
        let name = self.selected.as_ref()?;
        self.entries()?.iter().find(|e| &e.name == name)
    }

    /// The path of the entry picked.
    fn selected_path(&self) -> Option<Vec<u8>> {
        Some(files::join(self.dir.as_ref()?, self.selected.as_ref()?))
    }
}

/// Only regular files (and symlinks to one) are read: a device or a pipe could block, or
/// never end.
fn readable(e: &Entry) -> bool {
    e.kind == FileKind::File || (e.kind == FileKind::Symlink && !e.target_is_dir)
}

fn short(e: &anyhow::Error) -> SharedString {
    k8s::short_error(e).into()
}

impl FileBrowser {
    pub fn new(
        client: Client,
        context: String,
        ns: String,
        pod: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search by name, or a /path to open"));
        let preview = cx.new(|cx| EditorState::new(window, cx).line_number(true).searchable(true));
        let subs = vec![cx.subscribe_in(&query, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.submit_query(window, cx);
            }
        })];
        Self {
            client,
            context,
            ns,
            pod,
            containers: Vec::new(),
            container: None,
            container_select: None,
            _container_sub: None,
            places: HashMap::new(),
            exec_denied: false,
            pod_read: None,
            copy_denied: None,
            copy: CopyState::None,
            _copy_task: None,
            authorized: false,
            query,
            preview,
            previewed: None,
            _downloads: Vec::new(),
            _subs: subs,
        }
    }

    /// The pod's containers as read, and its `default` one, shown until another is picked.
    /// A choice the pod still has stays.
    pub fn set_containers(
        &mut self,
        names: Vec<String>,
        default: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if names == self.containers {
            return;
        }
        if self.container.as_ref().is_none_or(|c| !names.contains(c)) {
            self.container = default;
        }
        self.container_select = None;
        self._container_sub = None;
        if names.len() > 1 {
            let ix = self.container.as_ref().and_then(|c| names.iter().position(|n| n == c));
            let items: Vec<SharedString> = names.iter().cloned().map(Into::into).collect();
            let select = cx.new(|cx| {
                SelectState::new(SearchableVec::new(items), ix.map(|ix| IndexPath::default().row(ix)), window, cx)
            });
            self._container_sub = Some(cx.subscribe_in(
                &select,
                window,
                |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, window, cx| {
                    let SelectEvent::Confirm(Some(name)) = ev else { return };
                    this.choose_container(name.to_string(), window, cx);
                },
            ));
            self.container_select = Some(select);
        }
        self.containers = names;
        self.sync_preview(window, cx);
        cx.notify();
    }

    /// Whether the API server refuses this user an exec in the pod (`create` on `pods/exec`).
    pub fn set_exec_denied(&mut self, denied: bool, cx: &mut Context<Self>) {
        if self.exec_denied != denied {
            self.exec_denied = denied;
            cx.notify();
        }
    }

    /// Show another container: its own folder, as it was left.
    fn choose_container(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.container.as_deref() == Some(name.as_str()) {
            return;
        }
        self.container = Some(name);
        self.sync_preview(window, cx);
        cx.notify();
    }

    /// The key of the container shown in `places`.
    fn key(&self) -> String {
        self.container.clone().unwrap_or_default()
    }

    fn place(&self) -> Option<&Place> {
        self.places.get(&self.key())
    }

    fn source(&self) -> FileSource {
        if let Some(source) = self.copy_source(self.place()) {
            return source;
        }
        FileSource::exec(self.client.clone(), self.ns.clone(), self.pod.clone(), self.container.clone())
    }

    /// Run `then` once a command may run in the container: write mode on (offered when
    /// off), and on a protected context its name typed, once for this tab.
    fn guarded(
        &mut self,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Write mode may have been turned off since the last command: checked every time.
        if !super::ensure_writes(window, cx) {
            return;
        }
        if self.authorized || !crate::settings::is_protected(&self.context) {
            self.authorized = true;
            then(self, window, cx);
            return;
        }
        let guard = super::Guard::protected(&self.context, window, cx);
        let me = cx.entity().downgrade();
        let then = Rc::new(RefCell::new(Some(then)));
        let title = match &self.container {
            Some(c) => format!("Read the files of container {c}?"),
            None => "Read the files of this pod?".to_string(),
        };
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (me, guard, then) = (me.clone(), guard.clone(), then.clone());
            alert
                .title(title.clone())
                .description(
                    "Recouvrance lists folders and reads files by running sh, stat, head and find in the container \
                     (exec), with its rights.",
                )
                .child(super::confirm_extras("", guard.as_ref(), cx))
                .confirm_buttons("Browse", false, guard.as_ref())
                .on_ok(move |_, window, cx| {
                    if !super::guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if !super::writes_still_on(window, cx) {
                        return true;
                    }
                    let Some(then) = then.borrow_mut().take() else { return true };
                    let _ = me.update(cx, |this, cx| {
                        this.authorized = true;
                        then(this, window, cx);
                    });
                    true
                })
        });
    }

    /// List folder `dir` of the container shown; once listed, open (or preview) its entry
    /// `then_open`.
    fn open_dir(&mut self, dir: Vec<u8>, then_open: Option<Vec<u8>>, window: &mut Window, cx: &mut Context<Self>) {
        // Before the pod is read, which container a command would run in isn't known: its
        // place would be filed under none, and lost once the default one is.
        if self.container.is_none() {
            return;
        }
        let dir = files::normalize(&dir);
        let (key, source) = (self.key(), self.source());
        let place = self.places.entry(key.clone()).or_default();
        place.dir = Some(dir.clone());
        place.listing = None;
        place.selected = None;
        place.preview = None;
        place.preview_load.cancel();
        place.search = None;
        place.search_load.cancel();
        place.pending = then_open;
        let want = dir.clone();
        place.list_load.spawn_in(
            window,
            cx,
            async move { source.list(&dir).await },
            move |this, result, window, cx| {
                // Another folder was opened since: this listing is not shown.
                let Some(place) = this.places.get_mut(&key).filter(|p| p.dir.as_ref() == Some(&want)) else { return };
                place.listing = Some(result.map_err(|e| short(&e)));
                let entry =
                    place.pending.take().and_then(|name| place.entries()?.iter().find(|e| e.name == name).cloned());
                // Still the container shown: what was asked for opens.
                if let Some(entry) = entry.filter(|_| this.key() == key) {
                    this.activate(&entry, window, cx);
                }
                cx.notify();
            },
        );
        self.sync_preview(window, cx);
        cx.notify();
    }

    /// Open the entry `ix` of the folder shown.
    fn click(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.place().and_then(|p| p.entries()?.get(ix).cloned()) else { return };
        self.guarded(move |this, window, cx| this.activate(&entry, window, cx), window, cx);
    }

    /// Open a folder (or a symlink to one, where it leads), or pick anything else and preview
    /// it when it is a file.
    fn activate(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.place().and_then(|p| p.dir.clone()) else { return };
        if entry.opens() {
            let to = match &entry.target {
                Some(target) => files::resolve(&dir, target),
                None => files::join(&dir, &entry.name),
            };
            self.open_dir(to, None, window, cx);
            return;
        }
        let (key, source) = (self.key(), self.source());
        let Some(place) = self.places.get_mut(&key) else { return };
        place.selected = Some(entry.name.clone());
        place.preview = None;
        if readable(entry) {
            let path = files::join(&dir, &entry.name);
            let want = path.clone();
            let read = async move { source.read(&path, PREVIEW_BYTES).await };
            place.preview_load.spawn_in(window, cx, read, move |this, result, window, cx| {
                let Some(place) = this.places.get_mut(&key) else { return };
                if place.selected_path().as_ref() != Some(&want) {
                    return;
                }
                place.preview = Some(result.map_err(|e| short(&e)));
                // Read again: the editor takes the new text even for the same path.
                this.previewed = None;
                this.sync_preview(window, cx);
                cx.notify();
            });
        } else {
            place.preview_load.cancel();
        }
        self.sync_preview(window, cx);
        cx.notify();
    }

    /// Put the text previewed in the container shown into the editor, when it isn't there.
    fn sync_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.key();
        let Some(place) = self.places.get(&key) else { return };
        let Some(path) = place.selected_path() else { return };
        let id = (key, path);
        if self.previewed.as_ref() == Some(&id) {
            return;
        }
        let Some(Ok(preview)) = &place.preview else { return };
        let Some(text) = preview.text() else { return };
        let text = text.to_string();
        self.preview.update(cx, |s, cx| s.set_value(text, window, cx));
        self.previewed = Some(id);
    }

    fn close_preview(&mut self, cx: &mut Context<Self>) {
        let key = self.key();
        if let Some(place) = self.places.get_mut(&key) {
            place.selected = None;
            place.preview = None;
            place.preview_load.cancel();
        }
        cx.notify();
    }

    /// Enter in the query: a `/path` opens (a folder) or is picked in its folder (a file);
    /// anything else is searched for, by name, under the folder shown.
    fn submit_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.query.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.starts_with('/') {
            let path = files::normalize(text.as_bytes());
            let name = files::base_name(&path).to_vec();
            self.guarded(
                move |this, window, cx| {
                    if name.is_empty() {
                        this.open_dir(path, None, window, cx);
                    } else {
                        this.open_dir(files::parent(&path), Some(name), window, cx);
                    }
                },
                window,
                cx,
            );
        } else {
            self.guarded(move |this, window, cx| this.search(text, window, cx), window, cx);
        }
    }

    fn search(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let (key, source) = (self.key(), self.source());
        let Some(place) = self.places.get_mut(&key) else { return };
        let Some(dir) = place.dir.clone() else { return };
        place.search = Some((text.clone(), None));
        let want = text.clone();
        let find = async move { source.find(&dir, &text).await };
        place.search_load.spawn_in(window, cx, find, move |this, result, _, cx| {
            let Some(place) = this.places.get_mut(&key) else { return };
            // Another search since, or another folder opened (which ends the search).
            let Some((_, found)) = place.search.as_mut().filter(|(looked, _)| *looked == want) else { return };
            *found = Some(result.map_err(|e| short(&e)));
            cx.notify();
        });
        cx.notify();
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        let key = self.key();
        if let Some(place) = self.places.get_mut(&key) {
            place.search = None;
            place.search_load.cancel();
        }
        cx.notify();
    }

    /// A search result: its folder opens, with it opened or previewed there.
    fn open_found(&mut self, path: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) {
        let name = files::base_name(&path).to_vec();
        let dir = files::parent(&path);
        self.guarded(move |this, window, cx| this.open_dir(dir, Some(name), window, cx), window, cx);
    }

    /// Copy `path` to ~/Downloads, a folder as a `.tar`.
    fn download(&mut self, path: Vec<u8>, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.source();
        self.guarded(
            move |this, window, cx| {
                let name = files::display_name(&path);
                window.push_notification(Notification::info(format!("Downloading {name}…")), cx);
                let task = cx.spawn_in(window, async move |_, cx| {
                    let r = source.download(&path, folder).await;
                    let _ = cx.update(|window, cx| match r {
                        Ok(dest) => {
                            window.push_notification(Notification::success(format!("Saved {}", dest.display())), cx);
                        }
                        Err(e) => window.push_notification(
                            Notification::error(format!("Download of {name} failed: {}", k8s::short_error(&e))),
                            cx,
                        ),
                    });
                });
                this._downloads.retain(|t| !t.is_ready());
                this._downloads.push(task);
            },
            window,
            cx,
        );
    }

    /// Before browsing: what will run, and the button that starts.
    fn render_start(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(why) = self.stopped() {
            return self.render_copy_start(&why, cx);
        }
        let muted = cx.theme().muted_foreground;
        let target = format!(
            "{}/{}{}",
            self.ns,
            self.pod,
            self.container.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
        );
        let browse = Button::new("browse-files").primary().icon(Lucide::FolderOpen).label("Browse files");
        let browse = if self.exec_denied {
            browse.disabled(true).tooltip("Your RBAC does not allow exec in this pod (create on pods/exec)")
        } else if self.container.is_none() {
            browse.disabled(true).tooltip("Reading the pod…")
        } else {
            browse.on_click(cx.listener(|this, _, window, cx| {
                this.guarded(|this, window, cx| this.open_dir(b"/".to_vec(), None, window, cx), window, cx);
            }))
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().text_sm().text_color(muted).child(format!("Browse the files of {target}")))
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
            .child(browse)
            .child(div().max_w(px(520.)).text_xs().text_color(muted).child(
                "Folders are listed and files read by running sh, stat, head and find in the container (exec), \
                 so it needs write mode, like a shell.",
            ))
            .into_any_element()
    }

    fn render_toolbar(&self, place: &Place, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let dir = place.dir.clone().unwrap_or_else(|| b"/".to_vec());
        let at_root = dir == b"/";
        let crumbs = files::crumbs(&dir);
        let last = crumbs.len() - 1;
        let up = files::parent(&dir);
        let (here, refresh) = (dir.clone(), dir);
        h_flex()
            .gap_1()
            .flex_wrap()
            .items_center()
            .when_some(self.container_select.clone(), |d, s| d.child(div().w(px(180.)).child(Select::new(&s).xsmall())))
            .child(div().px_1().text_xs().text_color(theme.muted_foreground).child(self.source().origin().label()))
            .child(
                Button::new("files-up")
                    .xsmall()
                    .ghost()
                    .icon(Lucide::ArrowUp)
                    .tooltip("Parent folder")
                    .disabled(at_root)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let up = up.clone();
                        this.guarded(move |this, window, cx| this.open_dir(up, None, window, cx), window, cx);
                    })),
            )
            .children(crumbs.into_iter().enumerate().map(|(i, (name, path))| {
                h_flex()
                    .items_center()
                    .when(i > 1, |d| d.child(div().text_xs().text_color(theme.muted_foreground).child("/")))
                    .child(Button::new(("files-crumb", i)).xsmall().ghost().selected(i == last).label(name).on_click(
                        cx.listener(move |this, _, window, cx| {
                            let path = path.clone();
                            this.guarded(move |this, window, cx| this.open_dir(path, None, window, cx), window, cx);
                        }),
                    ))
            }))
            .child(div().flex_1())
            .child(
                Button::new("files-refresh")
                    .xsmall()
                    .ghost()
                    .icon(Lucide::RefreshCw)
                    .tooltip("List this folder again")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let dir = refresh.clone();
                        this.guarded(move |this, window, cx| this.open_dir(dir, None, window, cx), window, cx);
                    })),
            )
            .child(
                Button::new("files-download-dir")
                    .xsmall()
                    .ghost()
                    .icon(Lucide::Download)
                    .tooltip("Download this folder to ~/Downloads, as a .tar")
                    .on_click(cx.listener(move |this, _, window, cx| this.download(here.clone(), true, window, cx))),
            )
            .child(div().w(px(280.)).child(Input::new(&self.query).xsmall().cleanable(true)))
            .into_any_element()
    }

    fn render_header(&self, cx: &App) -> AnyElement {
        h_flex()
            .gap_3()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().flex_1().child("Name"))
            .child(div().w(px(90.)).child("Size"))
            .child(div().w(px(90.)).child("Mode"))
            .child(div().w(px(120.)).child("Owner"))
            .child(div().w(px(130.)).child("Modified"))
            .into_any_element()
    }

    fn render_row(&self, ix: usize, e: &Entry, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let icon = match e.kind {
            FileKind::Dir => Lucide::Folder,
            FileKind::Symlink if e.target_is_dir => Lucide::FolderSymlink,
            FileKind::Symlink => Lucide::FileSymlink,
            FileKind::File => Lucide::FileText,
            _ => Lucide::HardDrive,
        };
        let mut name = e.display();
        if let Some(target) = &e.target {
            name.push_str(" → ");
            name.push_str(&files::display_name(target));
        }
        let modified = jiff::Timestamp::from_second(e.modified).map_or_else(
            |_| String::new(),
            |t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string(),
        );
        h_flex()
            .id(("file", ix))
            .w_full()
            .gap_3()
            .px_2()
            .py_0p5()
            .text_sm()
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.list_active))
            .hover(|s| s.bg(theme.list_hover))
            .on_click(cx.listener(move |this, _, window, cx| this.click(ix, window, cx)))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(Icon::new(icon).size_3p5().text_color(if e.opens() {
                        theme.blue
                    } else {
                        theme.muted_foreground
                    }))
                    .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name)),
            )
            .child(div().w(px(90.)).text_color(theme.muted_foreground).child(e.size_text()))
            .child(div().w(px(90.)).font_family(theme.mono_font_family.clone()).text_xs().child(e.mode_string()))
            .child(
                div()
                    .w(px(120.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(theme.muted_foreground)
                    .child(e.owner()),
            )
            .child(div().w(px(130.)).text_color(theme.muted_foreground).child(modified))
            .into_any_element()
    }

    /// The folder's entries, only the rows on screen laid out: `/usr/lib` holds thousands.
    fn render_listing(&self, place: &Place, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let entries = match &place.listing {
            None => return div().p_2().text_sm().text_color(muted).child("Listing…").into_any_element(),
            Some(Err(e)) => return self.render_error(e.clone(), place.dir.clone(), cx),
            Some(Ok(entries)) => entries,
        };
        if entries.is_empty() {
            return div().p_2().text_sm().text_color(muted).child("This folder is empty.").into_any_element();
        }
        v_flex()
            .size_full()
            .child(self.render_header(cx))
            .child(
                uniform_list(
                    "files",
                    entries.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let Some(place) = this.place() else { return Vec::new() };
                        let Some(entries) = place.entries() else { return Vec::new() };
                        let selected = place.selected.as_ref();
                        range
                            .filter_map(|i| {
                                let e = entries.get(i)?;
                                Some(this.render_row(i, e, selected == Some(&e.name), cx))
                            })
                            .collect()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    /// Why a listing failed, and a button to try again.
    fn render_error(&self, e: SharedString, dir: Option<Vec<u8>>, cx: &mut Context<Self>) -> AnyElement {
        // The container stopped meanwhile: offer its copy.
        let copy = (self.stopped().is_some() && !self.place().is_some_and(|p| p.copy)).then(|| self.copy_button(cx));
        v_flex()
            .gap_2()
            .p_2()
            .child(error_box(e, cx))
            .child(h_flex().gap_2().children(copy).child(
                Button::new("files-retry").xsmall().outline().icon(Lucide::RotateCw).label("Try again").on_click(
                    cx.listener(move |this, _, window, cx| {
                        let dir = dir.clone().unwrap_or_else(|| b"/".to_vec());
                        this.guarded(move |this, window, cx| this.open_dir(dir, None, window, cx), window, cx);
                    }),
                ),
            ))
            .into_any_element()
    }

    fn render_search(
        &self,
        looked: &str,
        found: Option<&Result<Found, SharedString>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let head = h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(Lucide::Search).size_3p5())
            .child(div().flex_1().child(match found {
                None => format!("Searching for “{looked}”…"),
                Some(Err(_)) => format!("The search for “{looked}” failed"),
                Some(Ok(f)) => format!(
                    "{} found for “{looked}”{}",
                    f.paths.len(),
                    if f.truncated { " (more were not read: search for a longer name)" } else { "" }
                ),
            }))
            .child(
                Button::new("files-search-close")
                    .xsmall()
                    .ghost()
                    .icon(Lucide::X)
                    .label("Back to the folder")
                    .on_click(cx.listener(|this, _, _, cx| this.close_search(cx))),
            );
        let body = match found {
            Some(Err(e)) => error_box(e.clone(), cx).into_any_element(),
            Some(Ok(f)) => uniform_list(
                "found",
                f.paths.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    let Some((_, Some(Ok(found)))) = this.place().and_then(|p| p.search.as_ref()) else {
                        return Vec::new();
                    };
                    let theme = cx.theme();
                    range
                        .filter_map(|i| {
                            let path = found.paths.get(i)?.clone();
                            let shown = files::display_name(&path);
                            Some(
                                h_flex()
                                    .id(("found", i))
                                    .w_full()
                                    .px_2()
                                    .py_0p5()
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.list_hover))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_found(path.clone(), window, cx);
                                    }))
                                    .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(shown))
                                    .into_any_element(),
                            )
                        })
                        .collect()
                }),
            )
            .flex_1()
            .min_h_0()
            .into_any_element(),
            None => div().into_any_element(),
        };
        v_flex().size_full().child(head).child(body).into_any_element()
    }

    /// The entry picked: what it is, and its text when it is a text file.
    fn render_preview(&self, place: &Place, cx: &mut Context<Self>) -> Option<AnyElement> {
        let name = place.selected.clone()?;
        let theme = cx.theme().clone();
        let entry = place.selected_entry();
        let path = place.selected_path()?;
        let facts = entry.map(|e| {
            let size = if e.kind == FileKind::File { files::human_size(e.size) } else { e.kind.label().to_string() };
            format!("{size} · {} · {}", e.mode_string(), e.owner())
        });
        let note =
            |text: String| div().p_2().text_sm().text_color(theme.muted_foreground).child(text).into_any_element();
        let body = match (&place.preview, entry) {
            (Some(Ok(preview)), _) => match preview.text() {
                Some(_) => v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .child(div().flex_1().min_h_0().child(Editor::new(&self.preview).readonly(true).h_full()))
                    .when(preview.truncated, |d| {
                        d.child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                            "The first {} are shown: download the file for the rest.",
                            files::human_size(PREVIEW_BYTES as u64)
                        )))
                    })
                    .into_any_element(),
                None => note(format!("Binary file ({}): not shown. Download it to open it.", preview.kind())),
            },
            (Some(Err(e)), _) => error_box(e.clone(), cx).into_any_element(),
            (None, Some(e)) if !readable(e) => {
                note(format!("A {}: not read (reading it could block, or never end).", e.kind.label()))
            }
            (None, _) => note("Reading…".to_string()),
        };
        let downloadable = entry.is_none_or(readable);
        let close = cx.listener(|this, _, _, cx| this.close_preview(cx));
        let download = cx.listener(move |this, _, window, cx| this.download(path.clone(), false, window, cx));
        Some(
            v_flex()
                .w(px(560.))
                .h_full()
                .flex_shrink_0()
                .gap_1()
                .pl_2()
                .border_l_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(Icon::new(Lucide::FileText).size_3p5())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(files::display_name(&name)),
                        )
                        .when(downloadable, |d| {
                            d.child(
                                Button::new("files-download")
                                    .xsmall()
                                    .outline()
                                    .icon(Lucide::Download)
                                    .label("Download")
                                    .tooltip("Save it to ~/Downloads")
                                    .on_click(download),
                            )
                        })
                        .child(Button::new("files-preview-close").xsmall().ghost().icon(Lucide::X).on_click(close)),
                )
                .when_some(facts, |d, f| d.child(div().text_xs().text_color(theme.muted_foreground).child(f)))
                .child(div().flex_1().min_h_0().flex().flex_col().child(body))
                .into_any_element(),
        )
    }
}

impl Render for FileBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(place) = self.place().filter(|p| p.dir.is_some()) else {
            return self.render_start(cx);
        };
        let main = match &place.search {
            Some((looked, found)) => self.render_search(looked, found.as_ref(), cx),
            None => self.render_listing(place, cx),
        };
        let preview = if place.search.is_none() { self.render_preview(place, cx) } else { None };
        v_flex()
            .size_full()
            .gap_2()
            .child(self.render_toolbar(place, cx))
            .children(self.render_copy_note(place, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(div().flex_1().min_w_0().h_full().child(main))
                    .children(preview),
            )
            .into_any_element()
    }
}
