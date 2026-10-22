//! Create or update objects from YAML (several documents), with a server dry run first.
//! YAML comes from templates (built-in or your folder), a file (picked or dropped),
//! a URL, your $EDITOR, or forms for Secrets and ConfigMaps.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState, Position};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::k8s::{self, Kind};
use crate::ui::data_edit::{DataEditor, DataSaved};

const HEADER: &str = "# Paste one or more YAML documents, pick a template, open or drop a file, then Dry run and Apply (server-side apply).\n";

/// A problem found in the YAML, located when possible (0-based line).
#[derive(Clone)]
struct Problem {
    ok: bool,
    text: String,
    line: Option<u32>,
}

/// What identifies a version of the $EDITOR file: modification time, length, content hash.
type Stamp = (Option<std::time::SystemTime>, u64, u64);

fn stamp(path: &std::path::Path) -> Option<(Stamp, String)> {
    use std::hash::{Hash as _, Hasher as _};
    let text = std::fs::read_to_string(path).ok()?;
    let meta = std::fs::metadata(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    Some(((meta.modified().ok(), meta.len(), h.finish()), text))
}

/// Reload only once a change has been the same across two polls, so a file the editor
/// is still writing is never read half-way.
#[derive(Default)]
struct Settle {
    seen: Option<Stamp>,
    pending: Option<Stamp>,
}

impl Settle {
    /// True when `now` is a change that has settled and should be loaded.
    fn poll(&mut self, now: Stamp) -> bool {
        if self.seen == Some(now) {
            self.pending = None;
            return false;
        }
        if self.pending == Some(now) {
            self.seen = Some(now);
            self.pending = None;
            return true;
        }
        self.pending = Some(now);
        false
    }
}

/// A directory only we can read (mode 0700, fresh random name), removed when dropped.
struct PrivateDir(std::path::PathBuf);

impl PrivateDir {
    fn create() -> std::io::Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("recouvrance-{}", uuid::Uuid::new_v4().simple()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        // Not recursive: fails if the path already exists, so we never reuse someone else's.
        b.create(&dir)?;
        Ok(Self(dir))
    }

    /// Write a file readable only by us (mode 0600).
    fn write(&self, name: &str, text: &str) -> std::io::Result<std::path::PathBuf> {
        use std::io::Write as _;
        let path = self.0.join(name);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(&path)?.write_all(text.as_bytes())?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct External {
    dir: PrivateDir,
    path: std::path::PathBuf,
    settle: Settle,
}

pub struct ApplyView {
    context: String,
    client: Client,
    kinds: Vec<Kind>,
    namespace: Option<String>,
    editor: Entity<EditorState>,
    url: Entity<InputState>,
    results: Vec<Problem>,
    /// A Secret/ConfigMap form shown instead of the YAML editor.
    form: Option<Entity<DataEditor>>,
    /// File being edited in $EDITOR, in a private directory removed with this view.
    external: Option<External>,
    busy: bool,
    docs: Option<Entity<crate::ui::field_docs::FieldDocs>>,
    schema_form: Option<Entity<crate::ui::schema_form::SchemaForm>>,
    _task: Option<Task<()>>,
    /// The file picker or URL fetch, kept apart so it never drops a running apply.
    _load: Option<Task<()>>,
    _watch: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl ApplyView {
    pub fn new(
        context: String,
        client: Client,
        kinds: Vec<Kind>,
        namespace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let first = crate::templates::all(namespace.as_deref().unwrap_or("default"))
            .into_iter()
            .find(|t| t.name == "ConfigMap")
            .map(|t| t.body)
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).language("yaml").line_number(true).searchable(true);
            s.set_value(format!("{HEADER}{first}"), window, cx);
            s
        });
        let url = cx.new(|cx| InputState::new(window, cx).placeholder("https://… raw YAML URL, Enter to load"));
        let subs = vec![cx.subscribe_in(&url, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.load_url(window, cx);
            }
        })];
        #[allow(unused_mut)]
        let mut this = Self {
            context,
            client,
            kinds,
            namespace,
            editor,
            url,
            results: Vec::new(),
            form: None,
            external: None,
            busy: false,
            docs: None,
            schema_form: None,
            _task: None,
            _load: None,
            _watch: None,
            _subs: subs,
        };
        #[cfg(feature = "dev")]
        if std::env::var("RECOUVRANCE_APPLY_DRY").is_ok() {
            this.run(true, window, cx);
        }
        this
    }

    fn ns(&self) -> String {
        self.namespace.clone().unwrap_or_else(|| "default".into())
    }

    pub fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.form = None;
        self.results.clear();
        self.editor.update(cx, |e, cx| {
            e.set_value(text, window, cx);
            if let Some(d) = e.diagnostics_mut() {
                d.clear();
            }
        });
        cx.notify();
    }

    pub fn toggle_schema_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schema_form = match self.schema_form.take() {
            Some(_) => None,
            None => {
                self.docs = None;
                let (ctx, client, editor) = (self.context.clone(), self.client.clone(), self.editor.clone());
                Some(cx.new(|cx| crate::ui::schema_form::SchemaForm::new(ctx, client, editor, window, cx)))
            }
        };
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn schema_form(&self) -> Option<Entity<crate::ui::schema_form::SchemaForm>> {
        self.schema_form.clone()
    }

    pub fn toggle_docs(&mut self, cx: &mut Context<Self>) {
        self.schema_form = None;
        self.docs = match self.docs.take() {
            Some(_) => None,
            None => {
                let (ctx, client, editor) = (self.context.clone(), self.client.clone(), self.editor.clone());
                Some(cx.new(|cx| crate::ui::field_docs::FieldDocs::new(ctx, client, editor, cx)))
            }
        };
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn cursor_to(&mut self, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.set_cursor_position(Position::new(line, 6), window, cx));
    }

    pub fn use_template(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = crate::templates::all(&self.ns()).into_iter().find(|t| t.name == name) {
            self.set_text(t.body, window, cx);
        }
    }

    pub fn show_form(&mut self, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(k) = self.kinds.iter().find(|k| k.is("", kind)).cloned() else { return };
        let (ctx, client, ns) = (self.context.clone(), self.client.clone(), Some(self.ns()));
        let form = cx.new(|cx| DataEditor::new(ctx, client, k, ns, None, window, cx));
        self._subs.push(cx.subscribe_in(&form, window, |this, _, ev: &DataSaved, _, cx| {
            this.results = vec![Problem { ok: true, text: ev.0.clone(), line: None }];
            this.form = None;
            cx.notify();
        }));
        self.form = Some(form);
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn form(&self) -> Option<Entity<DataEditor>> {
        self.form.clone()
    }

    fn open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open YAML".into()),
        });
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let _ = this.update_in(cx, |this, window, cx| this.load_paths(&paths, window, cx));
        }));
    }

    /// Files picked or dropped: their documents, joined.
    pub fn load_paths(&mut self, paths: &[std::path::PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let mut text = String::new();
        for p in paths {
            match std::fs::read_to_string(p) {
                Ok(t) => {
                    if !text.is_empty() {
                        text.push_str("\n---\n");
                    }
                    text.push_str(&format!("# {}\n{}", p.display(), t));
                }
                Err(e) => {
                    self.results = vec![Problem { ok: false, text: format!("{}: {e}", p.display()), line: None }];
                    cx.notify();
                    return;
                }
            }
        }
        if !text.is_empty() {
            self.set_text(text, window, cx);
        }
    }

    #[cfg(feature = "dev")]
    pub fn set_url(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.url.update(cx, |s, cx| s.set_value(url.to_string(), window, cx));
    }

    pub fn load_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let url = self.url.read(cx).value().trim().to_string();
        if !url_allowed(&url) {
            // Plain http could be altered on the way, and what it brings gets applied.
            self.results = vec![Problem {
                ok: false,
                text: "Enter an https:// URL (plain http only for localhost).".into(),
                line: None,
            }];
            cx.notify();
            return;
        }
        self.busy = true;
        cx.notify();
        let fetch = k8s::run(crate::assistant::get_url(url.clone()));
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let r = fetch.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match r {
                    Ok(text) => this.set_text(format!("# {url}\n{text}"), window, cx),
                    Err(e) => {
                        this.results =
                            vec![Problem { ok: false, text: format!("Could not load {url}: {e:#}"), line: None }]
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Open the YAML in $VISUAL/$EDITOR; changes saved there come back here.
    pub fn open_external(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        // Reuse this view's private directory when there is one.
        let dir = match self.external.take() {
            Some(x) => Ok(x.dir),
            None => PrivateDir::create(),
        };
        let written = dir.and_then(|dir| dir.write("apply.yaml", &text).map(|path| (dir, path)));
        let (dir, path) = match written {
            Ok(x) => x,
            Err(e) => {
                self.results = vec![Problem { ok: false, text: format!("Could not write the file: {e}"), line: None }];
                cx.notify();
                return;
            }
        };
        if let Err(e) = launch_editor(&path) {
            self.results = vec![Problem { ok: false, text: e, line: None }];
            cx.notify();
            return;
        }
        let settle = Settle { seen: stamp(&path).map(|(s, _)| s), pending: None };
        self.external = Some(External { dir, path, settle });
        self._watch = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                let ok = this.update_in(cx, |this, window, cx| {
                    let Some(x) = this.external.as_mut() else { return false };
                    let Some((now, text)) = stamp(&x.path) else { return true };
                    if x.settle.poll(now) {
                        this.editor.update(cx, |e, cx| e.set_value(text, window, cx));
                        this.results =
                            vec![Problem { ok: true, text: "Reloaded from your editor.".into(), line: None }];
                        cx.notify();
                    }
                    true
                });
                if !matches!(ok, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Dry run straight away. A real apply runs the dry run first, then asks for
    /// confirmation listing each object (and the context name on protected contexts).
    pub fn run(&mut self, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let docs = match parse_docs(&self.editor.read(cx).value()) {
            Ok(d) => d,
            Err(p) => {
                self.results = vec![p];
                self.mark(window, cx);
                return;
            }
        };
        if docs.is_empty() {
            self.results = vec![Problem { ok: false, text: "No objects to apply.".into(), line: None }];
            cx.notify();
            return;
        }
        if dry_run {
            self.send(docs, true, window, cx);
            return;
        }
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        // The server checks every object first; only then is the real apply offered.
        let (client, kinds, default_ns) = (self.client.clone(), self.kinds.clone(), self.namespace.clone());
        self.busy = true;
        self.results.clear();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let results = apply_docs(client, kinds, default_ns, docs.clone(), true).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                let passed = results.iter().all(|p| p.ok);
                this.results = results;
                this.mark(window, cx);
                if passed {
                    this.confirm_apply(docs, window, cx);
                }
            });
        }));
        cx.notify();
    }

    /// What will be applied, object by object, then the real apply once confirmed.
    fn confirm_apply(&mut self, docs: Vec<Doc>, window: &mut Window, cx: &mut Context<Self>) {
        let default_ns = self.namespace.as_deref();
        let mut uses_page_ns = false;
        let objects: Vec<String> = docs
            .iter()
            .map(|d| {
                let kind_name = d.value["kind"].as_str().unwrap_or("?");
                let name = d.value.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("?");
                let ns = resolve_kind(&self.kinds, &d.value).ok().and_then(|k| {
                    uses_page_ns |= k.namespaced && d.value.pointer("/metadata/namespace").is_none();
                    target_ns(&d.value, k.namespaced, default_ns)
                });
                format!("{kind_name} {}{name}", ns.map(|n| format!("{n}/")).unwrap_or_default())
            })
            .collect();
        // Objects without a namespace go where the page is, as `-n` would send them.
        let ns_flag = uses_page_ns.then(|| default_ns.unwrap_or("default"));
        let cmd = crate::details::kubectl(&self.context, ns_flag, "apply --server-side -f -");
        let guard = crate::settings::is_protected(&self.context).then(|| {
            let ctx = self.context.clone();
            (cx.new(|cx| InputState::new(window, cx).placeholder(ctx.clone())), ctx)
        });
        let n = docs.len();
        let title = format!("Apply {n} object{} to {}?", if n == 1 { "" } else { "s" }, self.context);
        let me = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (me, guard, docs) = (me.clone(), guard.clone(), docs.clone());
            let muted = cx.theme().muted_foreground;
            alert
                .title(title.clone())
                .description("The server dry run passed. Server-side apply creates or updates each object:")
                .child(
                    v_flex().gap_0p5().max_h(px(220.)).overflow_hidden().children(
                        objects
                            .iter()
                            .take(20)
                            .cloned()
                            .chain((n > 20).then(|| format!("…and {} more", n - 20)))
                            .map(|o| div().text_sm().font_family("monospace").text_color(muted).child(o)),
                    ),
                )
                .child(crate::details::confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Apply")
                .on_ok(move |_, window, cx| {
                    if !crate::details::guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let docs = docs.clone();
                    let _ = me.update(cx, |this, cx| this.send(docs, false, window, cx));
                    true
                })
        });
    }

    fn send(&mut self, docs: Vec<Doc>, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (client, kinds, default_ns) = (self.client.clone(), self.kinds.clone(), self.namespace.clone());
        self.busy = true;
        self.results.clear();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let results = apply_docs(client, kinds, default_ns, docs, dry_run).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.results = results;
                this.busy = false;
                this.mark(window, cx);
            });
        }));
        cx.notify();
    }

    /// Underline located problems in the editor and move the cursor to the first one.
    fn mark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let problems: Vec<Problem> = self.results.iter().filter(|p| !p.ok && p.line.is_some()).cloned().collect();
        self.editor.update(cx, |e, cx| {
            let lines: Vec<String> = e.value().lines().map(String::from).collect();
            if let Some(d) = e.diagnostics_mut() {
                d.clear();
                for p in &problems {
                    let l = p.line.unwrap_or(0);
                    let len = lines.get(l as usize).map_or(1, |s| s.chars().count().max(1)) as u32;
                    d.push(Diagnostic {
                        range: Position::new(l, 0)..Position::new(l, len),
                        severity: DiagnosticSeverity::Error,
                        message: p.text.clone().into(),
                        ..Default::default()
                    });
                }
            }
            if let Some(l) = problems.first().and_then(|p| p.line) {
                e.set_cursor_position(Position::new(l, 0), window, cx);
            }
            cx.notify();
        });
        cx.notify();
    }
}

/// One non-empty YAML document: the 0-based line its text starts on, its text, its value.
#[derive(Clone)]
struct Doc {
    start: u32,
    body: String,
    value: serde_json::Value,
}

/// The cluster's type for a document (by apiVersion and kind), or why there is none.
fn resolve_kind(kinds: &[Kind], doc: &serde_json::Value) -> Result<Kind, String> {
    let api_version = doc["apiVersion"].as_str().unwrap_or_default();
    let kind_name = doc["kind"].as_str().unwrap_or_default();
    if let Some(k) = kinds.iter().find(|k| k.ar.api_version == api_version && k.ar.kind == kind_name) {
        return Ok(k.clone());
    }
    let hint = kinds
        .iter()
        .find(|k| k.ar.kind == kind_name)
        .map(|k| format!(" (did you mean apiVersion: {}?)", k.ar.api_version))
        .unwrap_or_default();
    Err(format!("unknown type {api_version} {kind_name}{hint}"))
}

/// Where a document goes: its own namespace, else the page's, else `default`;
/// none for cluster-scoped types.
fn target_ns(doc: &serde_json::Value, namespaced: bool, default_ns: Option<&str>) -> Option<String> {
    namespaced.then(|| {
        doc.pointer("/metadata/namespace").and_then(|v| v.as_str()).or(default_ns).unwrap_or("default").to_string()
    })
}

/// Server-side apply each document (or a dry run of it), one result each.
async fn apply_docs(
    client: Client,
    kinds: Vec<Kind>,
    default_ns: Option<String>,
    docs: Vec<Doc>,
    dry_run: bool,
) -> Vec<Problem> {
    let mut results = Vec::new();
    for Doc { start: base, body, value: doc } in docs {
        let kind_name = doc["kind"].as_str().unwrap_or_default().to_string();
        let name = doc.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let kind = match resolve_kind(&kinds, &doc) {
            Ok(k) => k,
            Err(e) => {
                results.push(Problem { ok: false, text: format!("{kind_name} {name}: {e}"), line: Some(base) });
                continue;
            }
        };
        let ns = target_ns(&doc, kind.namespaced, default_ns.as_deref());
        match k8s::apply(client.clone(), kind, ns, doc, dry_run).await {
            Ok(msg) => results.push(Problem { ok: true, text: msg, line: None }),
            Err(e) => {
                let msg = format!("{e:#}");
                let line = field_from_error(&msg).and_then(|path| line_of_path(&body, &path)).map(|l| l + base);
                let at = line.map(|l| format!(" (line {})", l + 1)).unwrap_or_default();
                results.push(Problem { ok: false, text: format!("{kind_name} {name}{at}: {}", explain(&msg)), line });
            }
        }
    }
    results
}

/// YAML is loaded over https only; plain http just from this machine.
fn url_allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Drop the port; an IPv6 address keeps its brackets.
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if !port.contains(']') => h,
        _ => authority,
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Split on `---` lines into (0-based line the piece starts on, its text); line `k` of a
/// piece is line `start + k` of the whole text. Empty documents stay in the list, so
/// positions never drift.
fn split_docs(text: &str) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, Vec<&str>)> = vec![(0, Vec::new())];
    for (i, l) in text.lines().enumerate() {
        match l.strip_prefix("---").filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t'])) {
            // Text after `--- ` on the same line belongs to the new document.
            Some(rest) => out.push((i as u32, vec![rest])),
            None => out.last_mut().expect("never empty").1.push(l),
        }
    }
    out.into_iter().map(|(start, lines)| (start, lines.join("\n"))).collect()
}

/// The documents to apply (empty ones dropped, each with its own start line), or the
/// first YAML error, located in the whole text.
fn parse_docs(text: &str) -> Result<Vec<Doc>, Problem> {
    let mut docs = Vec::new();
    for (start, body) in split_docs(text) {
        match crate::yaml::documents(&body) {
            Ok(values) => {
                for value in values.into_iter().filter(|v| !v.is_null()) {
                    docs.push(Doc { start, body: body.clone(), value });
                }
            }
            Err(e) => {
                let line = e.location().map(|l| l.line().saturating_sub(1) as u32 + start);
                let col = e.location().map(|l| l.column()).unwrap_or(0);
                let msg = crate::yaml::message(&e);
                let text = match line {
                    Some(l) => format!("Invalid YAML at line {}, column {col}: {msg}", l + 1),
                    None => format!("Invalid YAML: {msg}"),
                };
                return Err(Problem { ok: false, text, line });
            }
        }
    }
    Ok(docs)
}

/// The field path an API error names: `.spec.template.spec.contianers` or
/// `unknown field "spec.replicas"`.
fn field_from_error(msg: &str) -> Option<Vec<String>> {
    let re = regex::Regex::new(r#"(?:^|[\s"(])\.?((?:[A-Za-z0-9_-]+(?:\[\d+\])?\.)+[A-Za-z0-9_-]+)(?:"|: )"#).ok()?;
    let path = re.captures_iter(msg).map(|c| c[1].to_string()).next()?;
    Some(path.split('.').map(|p| p.split('[').next().unwrap_or(p).to_string()).filter(|p| !p.is_empty()).collect())
}

/// Find the line of a dotted path in a YAML document: each key nested deeper than the last.
fn line_of_path(doc: &str, path: &[String]) -> Option<u32> {
    let mut indent: i64 = -1;
    let mut found = None;
    let lines: Vec<&str> = doc.lines().collect();
    let mut from = 0;
    for key in path {
        let mut hit = None;
        for (i, l) in lines.iter().enumerate().skip(from) {
            let body = l.trim_start();
            let ind = (l.len() - body.len()) as i64;
            let body = body.strip_prefix("- ").unwrap_or(body);
            let ind = if l.trim_start().starts_with("- ") { ind + 2 } else { ind };
            if ind <= indent && !body.is_empty() && !body.starts_with('#') && found.is_some() && ind < indent {
                break;
            }
            if ind > indent && (body.starts_with(&format!("{key}:")) || body.starts_with(&format!("\"{key}\":"))) {
                hit = Some((i, ind));
                break;
            }
        }
        match hit {
            Some((i, ind)) => {
                found = Some(i as u32);
                indent = ind;
                from = i + 1;
            }
            None => break,
        }
    }
    found
}

/// API errors made shorter and clearer.
fn explain(msg: &str) -> String {
    // kube's ApiError repeats the message inside a debug-printed Status: keep the message.
    let m = msg.trim().trim_start_matches("ApiError: ");
    let m = m
        .split(" (Status {")
        .next()
        .unwrap_or(m)
        .split(" (ErrorResponse {")
        .next()
        .unwrap_or(m)
        .trim_end_matches(':')
        .trim();
    let m = match m.rsplit_once("): ") {
        Some((head, tail)) if head.starts_with("failed to create typed patch object") => tail,
        _ => m,
    };
    if m.contains("field not declared in schema") || m.contains("unknown field") {
        return format!(
            "{}. Check the spelling against the field docs (kubectl explain).",
            m.trim_end_matches(['.', ':'])
        );
    }
    if m.contains("AlreadyExists") || m.contains("already exists") {
        return format!("{m}. Apply updates it in place; Create refuses existing names.");
    }
    m.to_string()
}

/// Start $VISUAL/$EDITOR on a file; terminal editors open in a terminal window.
fn launch_editor(path: &std::path::Path) -> Result<(), String> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "xdg-open".into());
    let mut parts = editor.split_whitespace().map(String::from).collect::<Vec<_>>();
    let bin = parts.first().cloned().unwrap_or_default();
    let base = std::path::Path::new(&bin).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let gui = matches!(
        base.as_str(),
        "code"
            | "codium"
            | "cursor"
            | "subl"
            | "gedit"
            | "gnome-text-editor"
            | "kate"
            | "kwrite"
            | "zed"
            | "zeditor"
            | "gvim"
            | "xdg-open"
            | "mousepad"
            | "pluma"
            | "xed"
            | "emacsclient"
            | "idea"
            | "goland"
    );
    parts.push(path.display().to_string());
    let spawn = |args: &[String]| {
        std::process::Command::new(&args[0])
            .args(&args[1..])
            .stdin(std::process::Stdio::null())
            .spawn()
            .map(|mut child| {
                // Reaped in the background so the editor never lingers as a zombie.
                let _ = std::thread::Builder::new().name("reap".into()).spawn(move || child.wait());
            })
            .map_err(|e| e.to_string())
    };
    if gui {
        return spawn(&parts).map_err(|e| format!("Could not start {bin}: {e}"));
    }
    let terminals: Vec<Vec<String>> = std::env::var("TERMINAL")
        .ok()
        .map(|t| vec![t, "-e".into()])
        .into_iter()
        .chain(
            [
                ["x-terminal-emulator", "-e"],
                ["gnome-terminal", "--"],
                ["konsole", "-e"],
                ["kitty", ""],
                ["alacritty", "-e"],
                ["foot", ""],
                ["wezterm", "start"],
                ["xterm", "-e"],
            ]
            .iter()
            .map(|[t, f]| {
                let mut v = vec![t.to_string()];
                if !f.is_empty() {
                    v.push(f.to_string());
                }
                v
            }),
        )
        .collect();
    for mut term in terminals {
        term.extend(parts.iter().cloned());
        if spawn(&term).is_ok() {
            return Ok(());
        }
    }
    Err(format!("No terminal found to run {bin}; set $TERMINAL or a GUI $VISUAL (e.g. `code --wait`)."))
}

impl Render for ApplyView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let writes = crate::details::writes_enabled(cx);
        let templates = crate::templates::all(&self.ns());
        let muted = theme.muted_foreground;
        let side_item = |id: SharedString, label: String, user: bool| {
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded_sm()
                .text_sm()
                .cursor_pointer()
                .hover(|d| d.bg(theme.list_hover))
                .child(
                    h_flex()
                        .gap_1()
                        .child(label)
                        .when(user, |d| d.child(div().text_xs().text_color(muted).child("yours"))),
                )
        };
        let sidebar = v_flex()
            .id("apply-side")
            .w(px(230.))
            .h_full()
            .overflow_y_scroll()
            .gap_0p5()
            .pr_2()
            .border_r_1()
            .border_color(theme.border)
            .child(div().px_2().pb_1().text_xs().text_color(muted).child("FORMS"))
            .child(
                side_item("form-secret".into(), "New Secret…".into(), false)
                    .on_click(cx.listener(|this, _, w, cx| this.show_form("Secret", w, cx))),
            )
            .child(
                side_item("form-cm".into(), "New ConfigMap…".into(), false)
                    .on_click(cx.listener(|this, _, w, cx| this.show_form("ConfigMap", w, cx))),
            )
            .child(div().px_2().pt_3().pb_1().text_xs().text_color(muted).child("TEMPLATES"))
            .children(templates.into_iter().enumerate().map(|(i, t)| {
                let name = t.name.clone();
                side_item(SharedString::from(format!("tpl-{i}")), t.name, t.user)
                    .on_click(cx.listener(move |this, _, w, cx| this.use_template(&name, w, cx)))
            }))
            .child(
                div().pt_2().child(
                    Button::new("tpl-folder")
                        .xsmall()
                        .ghost()
                        .icon(Lucide::FolderOpen)
                        .label("Your templates folder")
                        .tooltip(crate::templates::folder().display().to_string())
                        .on_click(|_, _, _| {
                            if let Ok(dir) = crate::templates::ensure_folder() {
                                crate::forwards::spawn_reaped(std::process::Command::new("xdg-open").arg(dir));
                            }
                        }),
                ),
            );
        let main = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("open-file")
                            .small()
                            .outline()
                            .icon(Lucide::FileUp)
                            .label("Open file…")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, w, cx| this.open_file(w, cx))),
                    )
                    .child(div().w(px(340.)).child(Input::new(&self.url).small()))
                    .child(
                        Button::new("load-url")
                            .small()
                            .ghost()
                            .icon(Lucide::Globe)
                            .label("Load")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, w, cx| this.load_url(w, cx))),
                    )
                    .child(
                        Button::new("ext-editor")
                            .small()
                            .ghost()
                            .icon(Lucide::SquarePen)
                            .label(if self.external.is_some() { "Open in $EDITOR (synced)" } else { "Open in $EDITOR" })
                            .tooltip("Edit in $VISUAL / $EDITOR; each save there updates this editor")
                            .on_click(cx.listener(|this, _, w, cx| this.open_external(w, cx))),
                    )
                    .child(
                        Button::new("field-docs")
                            .small()
                            .ghost()
                            .icon(Lucide::BookOpen)
                            .label("Field docs")
                            .when(self.docs.is_some(), |b| b.selected(true))
                            .tooltip("Explain the field under the cursor (kubectl explain)")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_docs(cx))),
                    )
                    .child(
                        Button::new("schema-form")
                            .small()
                            .ghost()
                            .icon(Lucide::ListChecks)
                            .label("Form")
                            .when(self.schema_form.is_some(), |b| b.selected(true))
                            .tooltip("Edit the document under the cursor as a form generated from its schema")
                            .on_click(cx.listener(|this, _, w, cx| this.toggle_schema_form(w, cx))),
                    )
                    .child(div().flex_1())
                    .when(self.form.is_none(), |d| {
                        d.child(
                            Button::new("apply-dry")
                                .small()
                                .outline()
                                .icon(Lucide::FlaskConical)
                                .label("Dry run")
                                .loading(self.busy)
                                .on_click(cx.listener(|this, _, w, cx| this.run(true, w, cx))),
                        )
                        .child(
                            Button::new("apply-go")
                                .small()
                                .primary()
                                .icon(Lucide::Upload)
                                .label(if writes { "Apply" } else { "Apply (enable writes first)" })
                                .disabled(!writes || self.busy)
                                .on_click(cx.listener(|this, _, w, cx| this.run(false, w, cx))),
                        )
                    }),
            )
            .when(!self.results.is_empty(), |d| {
                d.child(v_flex().gap_0p5().p_2().rounded_md().bg(theme.secondary).children(self.results.iter().map(
                    |p| {
                        div().text_sm().text_color(if p.ok { theme.success } else { theme.danger }).child(format!(
                            "{} {}",
                            if p.ok { "✓" } else { "✗" },
                            p.text
                        ))
                    },
                )))
            })
            .map(|d| match self.form.clone() {
                Some(form) => d.child(v_flex().gap_2().child(form).child(div().child(
                    Button::new("back-yaml").xsmall().ghost().icon(Lucide::ArrowLeft).label("Back to YAML").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.form = None;
                            cx.notify();
                        }),
                    ),
                ))),
                None => d.child(
                    div()
                        .id("apply-drop")
                        .flex_1()
                        .min_h_0()
                        .rounded_md()
                        .drag_over::<ExternalPaths>(|s, _, _, cx| {
                            s.bg(cx.theme().drop_target).border_2().border_color(cx.theme().blue)
                        })
                        .on_drop(cx.listener(|this, paths: &ExternalPaths, w, cx| {
                            let paths = paths.paths().to_vec();
                            this.load_paths(&paths, w, cx);
                        }))
                        .child(
                            h_flex()
                                .size_full()
                                .gap_2()
                                .items_start()
                                .child(div().flex_1().min_w_0().h_full().child(Editor::new(&self.editor).h_full()))
                                .children(self.docs.clone().map(|d| div().w(px(380.)).h_full().child(d)))
                                .children(self.schema_form.clone().map(|d| div().w(px(460.)).h_full().child(d))),
                        ),
                ),
            });
        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Create / apply"))
                    .child(div().text_sm().text_color(muted).child(format!(
                        "into {}{} · kubectl apply --server-side · drop YAML files on the editor",
                        self.context,
                        self.namespace.as_ref().map(|n| format!(" / {n} (default namespace)")).unwrap_or_default()
                    ))),
            )
            .child(h_flex().flex_1().min_h_0().gap_3().items_start().child(sidebar).child(main))
    }
}

#[cfg(test)]
mod tests {
    use super::{Settle, field_from_error, line_of_path, parse_docs, split_docs, target_ns, url_allowed};

    #[test]
    fn urls_are_https() {
        assert!(url_allowed("https://raw.example.com/a.yaml"));
        assert!(url_allowed("http://localhost:8000/a.yaml"));
        assert!(url_allowed("http://127.0.0.1/a.yaml"));
        assert!(url_allowed("http://[::1]:8080/a.yaml"));
        assert!(!url_allowed("http://example.com/a.yaml"));
        assert!(!url_allowed("http://localhost.evil.com/a.yaml"));
        assert!(!url_allowed("http://127.0.0.1.evil.com/"));
        assert!(!url_allowed("ftp://localhost/a"));
        assert!(!url_allowed("file:///etc/passwd"));
    }

    #[test]
    fn namespaces_resolve_like_the_apply() {
        let with = serde_json::json!({"metadata": {"name": "a", "namespace": "team-a"}});
        let without = serde_json::json!({"metadata": {"name": "a"}});
        assert_eq!(target_ns(&with, true, Some("page")).as_deref(), Some("team-a"));
        assert_eq!(target_ns(&without, true, Some("page")).as_deref(), Some("page"));
        assert_eq!(target_ns(&without, true, None).as_deref(), Some("default"));
        assert_eq!(target_ns(&with, false, Some("page")), None);
    }

    #[test]
    fn locates_fields() {
        let doc = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  template:\n    spec:\n      contianers:\n        - name: a\n";
        let p = field_from_error(".spec.template.spec.contianers: field not declared in schema").unwrap();
        assert_eq!(p, ["spec", "template", "spec", "contianers"]);
        assert_eq!(line_of_path(doc, &p), Some(7));
        let p = field_from_error("Deployment.apps \"web\" is invalid: spec.replicas: Invalid value: -1").unwrap();
        assert_eq!(p, ["spec", "replicas"]);
        assert_eq!(line_of_path(doc, &p), Some(4));
        assert_eq!(split_docs("a: 1\n---\nb: 2\n"), vec![(0, "a: 1".to_string()), (1, "\nb: 2".to_string())]);
    }

    #[test]
    fn empty_documents_keep_lines_aligned() {
        // A leading `---`, an empty document and a comment-only one before the real ones.
        let text = "---\na: 1\n---\n---\n# nothing\n---\nkind: X\nspec:\n  bad: [\n";
        let err = parse_docs(text).err().expect("invalid");
        assert!(matches!(err.line, Some(8 | 9)), "{:?}", err.line);
        let text = "---\na: 1\n---\n---\n# nothing\n---\nkind: X\nspec:\n  ok: 1\n";
        let docs = parse_docs(text).ok().expect("valid");
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1].start, 5);
        let p = vec!["spec".to_string(), "ok".to_string()];
        assert_eq!(line_of_path(&docs[1].body, &p).map(|l| l + docs[1].start), Some(8));
        assert_eq!(text.lines().nth(8), Some("  ok: 1"));
    }

    #[test]
    fn reloads_only_settled_changes() {
        let a = (None, 1, 1);
        let b = (None, 2, 2);
        let c = (None, 3, 3);
        let mut s = Settle { seen: Some(a), pending: None };
        assert!(!s.poll(a));
        assert!(!s.poll(b)); // changed: wait one more poll
        assert!(!s.poll(c)); // still changing
        assert!(s.poll(c)); // stable: load
        assert!(!s.poll(c));
    }
}
