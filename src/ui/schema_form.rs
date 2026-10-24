//! A form generated from the cluster's schema for the YAML document under the
//! cursor: scalar fields become inputs, objects and lists open as sub-forms.
//! "Write to YAML" puts the values changed in the form back into the document (found
//! again by apiVersion, kind and name; comments in it are lost). Lists of strings or
//! integers are edited one item per line; strings with line breaks get several lines.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{EditorState, Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;
use serde_json::Value;

use crate::schema::{Field, Schema};

/// One line, or several for lists (one item per line).
enum ValueInput {
    Line(Entity<InputState>),
    Lines(Entity<TextareaState>),
}

impl ValueInput {
    fn text(&self, cx: &App) -> String {
        match self {
            Self::Line(i) => i.read(cx).value().to_string(),
            Self::Lines(i) => i.read(cx).value().to_string(),
        }
    }
}

enum Row {
    /// A value, and its text as loaded: only rows whose text changed are written.
    Scalar(Field, ValueInput, String),
    /// A list whose items have line breaks: one item per line would split them.
    Fixed(Field),
    /// An object or map: opened as its own form.
    Object(Field),
    /// A list of objects: its items open as forms.
    List(Field, usize),
}

pub struct SchemaForm {
    context: String,
    client: Client,
    editor: Entity<EditorState>,
    /// Which YAML document (0-based) the form edits.
    doc_ix: usize,
    doc: Value,
    kind: Option<(String, String)>,
    schema: Option<Result<Schema, String>>,
    path: Vec<String>,
    rows: Vec<Row>,
    status: Option<(bool, String)>,
    /// The schema of the current document's kind; dropped when the cursor moves to another.
    schema_load: crate::latest::Latest,
}

/// What finds a document again in the text: apiVersion, kind, metadata.name.
fn identity(doc: &Value) -> (Option<&str>, Option<&str>, Option<&str>) {
    (doc["apiVersion"].as_str(), doc["kind"].as_str(), doc.pointer("/metadata/name").and_then(Value::as_str))
}

fn parse_first(body: &str) -> Value {
    crate::yaml::documents(body).ok().and_then(|d| d.into_iter().next()).unwrap_or(Value::Null)
}

/// The one document of `text` with the same identity as `doc`: its index and value.
fn find_doc(text: &str, doc: &Value) -> Result<(usize, Value), String> {
    let want = identity(doc);
    if want.0.is_none() || want.1.is_none() {
        return Err("The form has no document; put the cursor in one and Reload.".into());
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = doc_ranges(text)
        .into_iter()
        .enumerate()
        .map(|(i, (a, b))| (i, a, b, parse_first(&lines[a..b].join("\n"))))
        .filter(|(_, _, _, v)| identity(v) == want);
    let describe = || {
        format!("{} {}", want.1.unwrap_or_default(), want.2.map_or_else(|| "(no name)".into(), |n| format!("`{n}`")))
    };
    match (hits.next(), hits.next()) {
        (Some((i, a, b, v)), None) => {
            // Text after `---` that is not a comment, or a second document in the range,
            // would be lost or mangled when the range is written back.
            let after_sep = a.checked_sub(1).map_or("", |s| lines[s].get(3..).unwrap_or_default().trim());
            let several = crate::yaml::documents(&lines[a..b].join("\n")).is_ok_and(|d| d.len() > 1);
            if !(after_sep.is_empty() || after_sep.starts_with('#')) || several {
                return Err(format!("{} is not alone between `---` lines; not writing.", describe()));
            }
            Ok((i, v))
        }
        (None, _) => Err(format!("{} is no longer in the YAML (changed since Reload?). Reload the form.", describe())),
        (Some(_), Some(_)) => Err(format!("{} appears more than once in the YAML; not writing.", describe())),
    }
}

/// Each document's lines (start, end), without the `---` lines between them; split
/// exactly as Apply splits, so `--- # comment` starts a document here too.
fn doc_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (sep, _) in crate::ui::apply_view::split_docs(text).into_iter().skip(1) {
        out.push((start, sep as usize));
        start = sep as usize + 1;
    }
    out.push((start, text.lines().count()));
    out
}

fn pointer(path: &[String]) -> String {
    path.iter().map(|p| format!("/{}", p.replace('~', "~0").replace('/', "~1"))).collect()
}

/// The object at `path` in `doc`, made where it is missing or null. A list item is reached
/// by its index (`containers/0`). None when something on the way is not an object or a list,
/// or an index is past the end of its list.
fn object_at<'a>(doc: &'a mut Value, path: &[String]) -> Option<&'a mut serde_json::Map<String, Value>> {
    if doc.is_null() {
        *doc = Value::Object(Default::default());
    }
    let mut v = doc;
    for seg in path {
        let child = match v {
            Value::Object(o) => o.entry(seg.clone()).or_insert(Value::Null),
            Value::Array(items) => items.get_mut(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
        if child.is_null() {
            *child = Value::Object(Default::default());
        }
        v = child;
    }
    v.as_object_mut()
}

/// Add an empty item to the list `name` of the object at `path`; its index.
fn add_list_item(doc: &mut Value, path: &[String], name: &str) -> Result<usize, String> {
    let obj = object_at(doc, path).ok_or("This part of the document is not an object.")?;
    let list = obj.entry(name.to_string()).or_insert(Value::Null);
    if list.is_null() {
        *list = Value::Array(Vec::new());
    }
    let Value::Array(items) = list else { return Err(format!("{name} is not a list in the YAML.")) };
    items.push(Value::Object(Default::default()));
    Ok(items.len() - 1)
}

fn scalar_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => {
            a.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), String::from)).collect::<Vec<_>>().join("\n")
        }
        Some(v) => v.to_string(),
    }
}

/// Text typed in an input, as the JSON the schema type wants.
fn typed(type_name: &str, text: &str) -> Option<Value> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    Some(match type_name {
        // A multi-line string (a script, a query) is kept exactly, final line break included.
        "string" if text.contains('\n') => text.into(),
        "integer" => t.parse::<i64>().map_or_else(|_| t.into(), Value::from),
        "number" => t.parse::<f64>().map_or_else(|_| t.into(), Value::from),
        // Anything else is kept as typed, for the server to refuse.
        "boolean" => match t.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Value::Bool(true),
            "false" | "no" | "off" | "0" => Value::Bool(false),
            _ => t.into(),
        },
        "int-or-string" => t.parse::<i64>().map_or_else(|_| t.into(), Value::from),
        // One item per line, so items may contain commas.
        "[]string" => Value::Array(t.lines().map(str::trim).filter(|s| !s.is_empty()).map(Value::from).collect()),
        "[]integer" => Value::Array(
            t.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<i64>().map_or_else(|_| s.into(), Value::from))
                .collect(),
        ),
        _ => t.into(),
    })
}

/// Whether a list can be edited one item per line: none of its items has a line break.
fn one_item_per_line(v: Option<&Value>) -> bool {
    v.and_then(Value::as_array).is_none_or(|a| a.iter().all(|x| !x.as_str().is_some_and(|s| s.contains('\n'))))
}

fn is_scalar(type_name: &str) -> bool {
    matches!(type_name, "string" | "integer" | "number" | "boolean" | "int-or-string" | "[]string" | "[]integer")
}

impl SchemaForm {
    pub fn new(
        context: String,
        client: Client,
        editor: Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            context,
            client,
            editor,
            doc_ix: 0,
            doc: Value::Null,
            kind: None,
            schema: None,
            path: Vec::new(),
            rows: Vec::new(),
            status: None,
            schema_load: Default::default(),
        };
        this.reload(window, cx);
        this
    }

    /// Read the document under the cursor again.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, line) = {
            let e = self.editor.read(cx);
            (e.value().to_string(), e.cursor_position().line as usize)
        };
        let ranges = doc_ranges(&text);
        self.doc_ix = ranges.iter().position(|(a, b)| line >= *a && line <= *b).unwrap_or(0);
        let (a, b) = ranges[self.doc_ix];
        let body = text.lines().skip(a).take(b - a).collect::<Vec<_>>().join("\n");
        self.doc = parse_first(&body);
        self.status = None;
        let kind = Some((
            self.doc["apiVersion"].as_str().unwrap_or_default().to_string(),
            self.doc["kind"].as_str().unwrap_or_default().to_string(),
        ))
        .filter(|(a, k)| !a.is_empty() && !k.is_empty());
        if kind != self.kind {
            self.kind = kind.clone();
            self.schema = None;
            self.path.clear();
            // A document without a kind has no schema: a load still running for the
            // previous one must not land on it.
            self.schema_load.cancel();
            if let Some((api_version, kind)) = kind {
                let (ctx, client) = (self.context.clone(), self.client.clone());
                let load = async move {
                    crate::schema::document(&ctx, client, &api_version).await.map_err(|e| format!("{e:#}")).and_then(
                        |doc| {
                            Schema::find(doc, &api_version, &kind)
                                .ok_or_else(|| format!("No schema for {api_version} {kind}"))
                        },
                    )
                };
                self.schema_load.spawn_in(window, cx, load, |this, r, window, cx| {
                    this.schema = Some(r);
                    this.build_rows(window, cx);
                });
            }
        }
        self.build_rows(window, cx);
    }

    pub fn open(&mut self, path: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.path = path;
        self.status = None;
        self.build_rows(window, cx);
    }

    fn build_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rows.clear();
        let Some(Ok(schema)) = &self.schema else {
            cx.notify();
            return;
        };
        let Some(x) = schema.explain(&self.path) else {
            cx.notify();
            return;
        };
        let here = self.doc.pointer(&pointer(&self.path)).cloned().unwrap_or(Value::Null);
        for f in x.children {
            let value = here.get(&f.name);
            if f.type_name.starts_with("[]") && is_scalar(&f.type_name) && !one_item_per_line(value) {
                self.rows.push(Row::Fixed(f));
            } else if is_scalar(&f.type_name) {
                let text = scalar_text(value);
                let hint = if f.enum_values.is_empty() { f.type_name.clone() } else { f.enum_values.join(" | ") };
                let input = if f.type_name.starts_with("[]") || text.contains('\n') {
                    let hint = if f.type_name.starts_with("[]") { format!("{hint}, one item per line") } else { hint };
                    ValueInput::Lines(cx.new(|cx| {
                        let mut s = TextareaState::new(window, cx).auto_grow(1, 8).placeholder(hint);
                        s.set_value(text.clone(), window, cx);
                        s
                    }))
                } else {
                    ValueInput::Line(cx.new(|cx| {
                        let mut s = InputState::new(window, cx).placeholder(hint);
                        s.set_value(text.clone(), window, cx);
                        s
                    }))
                };
                self.rows.push(Row::Scalar(f, input, text));
            } else if f.type_name.starts_with("[]") {
                let n = value.and_then(Value::as_array).map_or(0, Vec::len);
                self.rows.push(Row::List(f, n));
            } else {
                self.rows.push(Row::Object(f));
            }
        }
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn demo_set(&mut self, name: &str, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        for row in &self.rows {
            if let Row::Scalar(f, input, _) = row
                && f.name == name
            {
                match input {
                    ValueInput::Line(i) => i.update(cx, |s, cx| s.set_value(value.to_string(), window, cx)),
                    ValueInput::Lines(i) => i.update(cx, |s, cx| s.set_value(value.to_string(), window, cx)),
                }
            }
        }
    }

    /// Find the form's document again in the editor's current text (it may have been
    /// edited, or documents added or removed, since Reload) and make it the form's.
    fn refresh_doc(&mut self, cx: &mut Context<Self>) -> bool {
        let text = self.editor.read(cx).value().to_string();
        match find_doc(&text, &self.doc) {
            Ok((ix, doc)) => {
                self.doc_ix = ix;
                self.doc = doc;
                true
            }
            Err(e) => {
                self.status = Some((false, e));
                cx.notify();
                false
            }
        }
    }

    /// Put the values changed in the form into the document and the document into the editor.
    pub fn write(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() || !matches!(self.schema, Some(Ok(_))) {
            return;
        }
        let changed: Vec<(String, Option<Value>)> = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Scalar(f, input, loaded) => {
                    let now = input.text(cx);
                    (now != *loaded).then(|| (f.name.clone(), typed(&f.type_name, &now)))
                }
                _ => None,
            })
            .collect();
        if changed.is_empty() {
            self.status = Some((true, "Nothing changed in the form.".into()));
            cx.notify();
            return;
        }
        if !self.refresh_doc(cx) {
            return;
        }
        let mut doc = self.doc.clone();
        let Some(obj) = object_at(&mut doc, &self.path) else {
            self.status = Some((false, "This part of the document is not an object.".into()));
            cx.notify();
            return;
        };
        for (name, v) in changed {
            match v {
                Some(v) => {
                    obj.insert(name, v);
                }
                None => {
                    obj.remove(&name);
                }
            }
        }
        // What is written becomes what is loaded.
        for row in &mut self.rows {
            if let Row::Scalar(_, input, loaded) = row {
                *loaded = input.text(cx);
            }
        }
        self.doc = doc;
        self.replace_doc(window, cx);
        self.status = Some((true, "Written to the YAML.".into()));
        cx.notify();
    }

    fn add_item(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.refresh_doc(cx) {
            return;
        }
        let mut doc = self.doc.clone();
        let ix = match add_list_item(&mut doc, &self.path, &name) {
            Ok(ix) => ix,
            Err(e) => {
                self.status = Some((false, e));
                cx.notify();
                return;
            }
        };
        self.doc = doc;
        self.replace_doc(window, cx);
        let mut path = self.path.clone();
        path.extend([name, ix.to_string()]);
        self.open(path, window, cx);
    }

    fn replace_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        let ranges = doc_ranges(&text);
        let Some(&(a, b)) = ranges.get(self.doc_ix) else { return };
        let lines: Vec<&str> = text.lines().collect();
        let yaml = crate::yaml::to_string(&self.doc).unwrap_or_default();
        let mut out: Vec<String> = lines[..a].iter().map(ToString::to_string).collect();
        out.extend(yaml.trim_end().lines().map(String::from));
        out.extend(lines[b..].iter().map(ToString::to_string));
        let joined = out.join("\n") + "\n";
        self.editor.update(cx, |e, cx| e.set_value(joined, window, cx));
    }
}

impl Render for SchemaForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let can_write = matches!(self.schema, Some(Ok(_))) && !self.rows.is_empty();
        let kind = self.kind.as_ref().map_or_else(|| "?".into(), |k| k.1.clone());
        let mut crumbs = vec![(kind, Vec::new())];
        for i in 0..self.path.len() {
            crumbs.push((self.path[i].clone(), self.path[..=i].to_vec()));
        }
        let header = h_flex().flex_wrap().gap_1().text_xs().children(crumbs.into_iter().enumerate().map(
            |(i, (label, path))| {
                h_flex().gap_1().when(i > 0, |d| d.child(div().text_color(muted).child("›"))).child(
                    div()
                        .id(("crumb", i))
                        .text_color(theme.blue)
                        .cursor_pointer()
                        .child(label)
                        .on_click(cx.listener(move |this, _, w, cx| this.open(path.clone(), w, cx))),
                )
            },
        ));
        let body: AnyElement = match &self.schema {
            None if self.kind.is_none() => div()
                .text_sm()
                .text_color(muted)
                .child("Put the cursor in a document with apiVersion and kind, then Reload.")
                .into_any_element(),
            None => div().text_sm().text_color(muted).child("Loading the schema…").into_any_element(),
            Some(Err(e)) => div().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some(Ok(_)) => v_flex()
                .gap_1()
                .children(self.rows.iter().enumerate().map(|(i, row)| {
                    let label = |f: &Field| {
                        h_flex()
                            .w(px(150.))
                            .flex_shrink_0()
                            .gap_1()
                            .text_sm()
                            .child(div().overflow_hidden().text_ellipsis().child(f.name.clone()))
                            .when(f.required, |d| d.child(div().text_color(theme.danger).child("*")))
                    };
                    match row {
                        Row::Scalar(f, input, _) => h_flex()
                            .gap_2()
                            .items_start()
                            .child(label(f))
                            .child(div().flex_1().child(match input {
                                ValueInput::Line(i) => Input::new(i).small().into_any_element(),
                                ValueInput::Lines(i) => Textarea::new(i).small().into_any_element(),
                            }))
                            .into_any_element(),
                        Row::Fixed(f) => h_flex()
                            .gap_2()
                            .child(label(f))
                            .child(div().text_sm().text_color(muted).child("Items with line breaks: edit in the YAML"))
                            .into_any_element(),
                        Row::Object(f) => {
                            let mut path = self.path.clone();
                            path.push(f.name.clone());
                            let set = self.doc.pointer(&pointer(&path)).is_some();
                            h_flex()
                                .gap_2()
                                .child(label(f))
                                .child(
                                    div()
                                        .id(("open-obj", i))
                                        .text_sm()
                                        .text_color(theme.blue)
                                        .cursor_pointer()
                                        .child(format!("{} {}", f.type_name, if set { "›" } else { "(not set) ›" }))
                                        .on_click(cx.listener(move |this, _, w, cx| this.open(path.clone(), w, cx))),
                                )
                                .into_any_element()
                        }
                        Row::List(f, n) => {
                            let name = f.name.clone();
                            let base = self.path.clone();
                            h_flex()
                                .gap_2()
                                .flex_wrap()
                                .child(label(f))
                                .children((0..*n).map(|ix| {
                                    let mut path = base.clone();
                                    path.extend([name.clone(), ix.to_string()]);
                                    let title = self
                                        .doc
                                        .pointer(&format!("{}/name", pointer(&path)))
                                        .and_then(Value::as_str)
                                        .map_or_else(|| format!("#{ix}"), String::from);
                                    div()
                                        .id(SharedString::from(format!("item-{i}-{ix}")))
                                        .text_sm()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(theme.secondary)
                                        .cursor_pointer()
                                        .child(format!("{title} ›"))
                                        .on_click(cx.listener(move |this, _, w, cx| this.open(path.clone(), w, cx)))
                                }))
                                .child(
                                    Button::new(("add-item", i))
                                        .xsmall()
                                        .ghost()
                                        .icon(Lucide::Plus)
                                        .label("Add")
                                        .on_click(
                                            cx.listener(move |this, _, w, cx| this.add_item(name.clone(), w, cx)),
                                        ),
                                )
                                .into_any_element()
                        }
                    }
                }))
                .into_any_element(),
        };
        v_flex()
            .id("schema-form")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_xs().text_color(muted).child("FORM · from the schema"))
                    .child(div().flex_1())
                    .child(
                        Button::new("form-reload")
                            .xsmall()
                            .ghost()
                            .icon(Lucide::RefreshCw)
                            .tooltip("Read the document under the cursor again")
                            .on_click(cx.listener(|this, _, w, cx| this.reload(w, cx))),
                    )
                    .child(
                        Button::new("form-write")
                            .xsmall()
                            .primary()
                            .label("Write to YAML")
                            .disabled(!can_write)
                            .on_click(cx.listener(|this, _, w, cx| this.write(w, cx))),
                    ),
            )
            .child(header)
            .when_some(self.status.clone(), |d, (ok, m)| {
                d.child(div().text_xs().text_color(if ok { theme.success } else { theme.danger }).child(m))
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn typing() {
        use super::typed;
        assert_eq!(typed("integer", "3"), Some(serde_json::json!(3)));
        assert_eq!(typed("boolean", "true"), Some(serde_json::json!(true)));
        assert_eq!(typed("boolean", "True"), Some(serde_json::json!(true)));
        assert_eq!(typed("boolean", "FALSE"), Some(serde_json::json!(false)));
        assert_eq!(typed("string", " "), None);
        assert_eq!(typed("int-or-string", "http"), Some(serde_json::json!("http")));
    }

    #[test]
    fn multi_line_strings_are_kept() {
        use super::{one_item_per_line, scalar_text, typed};
        let expr = serde_json::json!("sum(rate(x[5m]))\n  > 0\n");
        assert_eq!(typed("string", &scalar_text(Some(&expr))), Some(expr.clone()));
        assert_eq!(typed("string", " one line "), Some(serde_json::json!("one line")));
        assert_eq!(typed("string", "echo bye\n"), Some(serde_json::json!("echo bye\n")));
        assert!(one_item_per_line(Some(&serde_json::json!(["a", "b"]))));
        assert!(one_item_per_line(None));
        assert!(!one_item_per_line(Some(&serde_json::json!(["set -e\necho hi\n"]))));
    }

    #[test]
    fn lists_are_one_item_per_line() {
        use super::{scalar_text, typed};
        let v = serde_json::json!(["--flag=a,b", "c"]);
        let text = scalar_text(Some(&v));
        assert_eq!(text, "--flag=a,b\nc");
        assert_eq!(typed("[]string", &text), Some(v));
        assert_eq!(typed("[]string", "a\n\n b \n"), Some(serde_json::json!(["a", "b"])));
        assert_eq!(typed("[]integer", "1\n2"), Some(serde_json::json!([1, 2])));
    }

    #[test]
    fn finds_the_document_again() {
        use super::find_doc;
        let doc = serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "b"}});
        // A document was added before it since it was loaded.
        let text = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n---\nnew: 1\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: b\ndata:\n  k: edited\n";
        let (ix, found) = find_doc(text, &doc).unwrap();
        assert_eq!(ix, 2);
        assert_eq!(found["data"]["k"], "edited");
        assert!(find_doc("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\n", &doc).is_err());
        let twice = format!("{text}---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: b\n");
        assert!(find_doc(&twice, &doc).is_err());
    }

    #[test]
    fn adds_to_missing_or_empty_lists() {
        use super::add_list_item;
        let path = |p: &[&str]| p.iter().map(ToString::to_string).collect::<Vec<_>>();
        // `env:` and `containers: ~` are null lists.
        let mut doc = serde_json::json!({"spec": {"containers": null, "env": [{"name": "a"}]}});
        assert_eq!(add_list_item(&mut doc, &path(&["spec"]), "containers"), Ok(0));
        assert_eq!(doc["spec"]["containers"], serde_json::json!([{}]));
        assert_eq!(add_list_item(&mut doc, &path(&["spec"]), "env"), Ok(1));
        // The objects on the path are made when missing or null.
        let mut doc = serde_json::json!({"spec": null});
        assert_eq!(add_list_item(&mut doc, &path(&["spec", "template", "spec"]), "volumes"), Ok(0));
        assert_eq!(doc["spec"]["template"]["spec"]["volumes"], serde_json::json!([{}]));
        // Inside a list item, reached by its index.
        let mut doc = serde_json::json!({"spec": {"containers": [{"name": "a", "env": null}]}});
        assert_eq!(add_list_item(&mut doc, &path(&["spec", "containers", "0"]), "env"), Ok(0));
        assert_eq!(doc["spec"]["containers"][0]["env"], serde_json::json!([{}]));
        assert!(add_list_item(&mut doc, &path(&["spec", "containers", "1"]), "env").is_err());
        assert!(add_list_item(&mut serde_json::json!(["a"]), &path(&["spec"]), "env").is_err());
        // Not a list, or not an object on the way: said, not ignored.
        let mut doc = serde_json::json!({"spec": {"ports": "80", "x": 1}});
        assert!(add_list_item(&mut doc, &path(&["spec"]), "ports").is_err());
        assert!(add_list_item(&mut doc, &path(&["spec", "x"]), "ports").is_err());
    }

    #[test]
    fn splits_as_apply_does() {
        use super::{doc_ranges, find_doc};
        let text = "kind: A\n--- # web\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: b\n";
        assert_eq!(doc_ranges(text), vec![(0, 1), (2, 6)]);
        let doc = serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "b"}});
        assert_eq!(find_doc(text, &doc).unwrap().0, 1);
        // Content after `---` on the same line would be lost on write.
        let inline = "--- !!map\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: b\n";
        assert!(find_doc(inline, &doc).is_err());
    }
}
