//! A form generated from the cluster's schema for the YAML document under the
//! cursor: scalar fields become inputs, objects and lists open as sub-forms.
//! "Write to YAML" puts the values changed in the form back into the document (found
//! again by apiVersion, kind and name; comments in it are lost). Lists of strings or
//! integers are edited one item per line.

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
    _task: Option<Task<()>>,
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
        .map(|(i, (a, b))| (i, parse_first(&lines[a..b].join("\n"))))
        .filter(|(_, v)| identity(v) == want);
    let describe = || {
        format!(
            "{} {}",
            want.1.unwrap_or_default(),
            want.2.map(|n| format!("`{n}`")).unwrap_or_else(|| "(no name)".into())
        )
    };
    match (hits.next(), hits.next()) {
        (Some(hit), None) => Ok(hit),
        (None, _) => Err(format!("{} is no longer in the YAML (changed since Reload?). Reload the form.", describe())),
        (Some(_), Some(_)) => Err(format!("{} appears more than once in the YAML; not writing.", describe())),
    }
}

fn doc_ranges(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, l) in lines.iter().enumerate() {
        if l.trim_end() == "---" {
            out.push((start, i));
            start = i + 1;
        }
    }
    out.push((start, lines.len()));
    out
}

fn pointer(path: &[String]) -> String {
    path.iter().map(|p| format!("/{}", p.replace('~', "~0").replace('/', "~1"))).collect()
}

fn scalar_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))
            .collect::<Vec<_>>()
            .join("\n"),
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
        "integer" => t.parse::<i64>().map(Value::from).unwrap_or_else(|_| t.into()),
        "number" => t.parse::<f64>().map(Value::from).unwrap_or_else(|_| t.into()),
        // Anything else is kept as typed, for the server to refuse.
        "boolean" => match t.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Value::Bool(true),
            "false" | "no" | "off" | "0" => Value::Bool(false),
            _ => t.into(),
        },
        "int-or-string" => t.parse::<i64>().map(Value::from).unwrap_or_else(|_| t.into()),
        // One item per line, so items may contain commas.
        "[]string" => Value::Array(t.lines().map(str::trim).filter(|s| !s.is_empty()).map(Value::from).collect()),
        "[]integer" => Value::Array(
            t.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<i64>().map(Value::from).unwrap_or_else(|_| s.into()))
                .collect(),
        ),
        _ => t.into(),
    })
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
            _task: None,
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
            if let Some((api_version, kind)) = kind {
                let (ctx, client) = (self.context.clone(), self.client.clone());
                self._task = Some(cx.spawn_in(window, async move |this, cx| {
                    let r = crate::schema::document(&ctx, client, &api_version)
                        .await
                        .map_err(|e| format!("{e:#}"))
                        .and_then(|doc| {
                            Schema::find(doc, &api_version, &kind)
                                .ok_or_else(|| format!("No schema for {api_version} {kind}"))
                        });
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.schema = Some(r);
                        this.build_rows(window, cx);
                    });
                }));
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
            if is_scalar(&f.type_name) {
                let text = scalar_text(value);
                let hint = if f.enum_values.is_empty() { f.type_name.clone() } else { f.enum_values.join(" | ") };
                let input = if f.type_name.starts_with("[]") {
                    let hint = format!("{hint}, one item per line");
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
        // Make sure the objects on the path exist.
        if !doc.is_object() {
            doc = Value::Object(Default::default());
        }
        for i in 0..self.path.len() {
            let parent = pointer(&self.path[..i]);
            let seg = &self.path[i];
            let Some(p) = doc.pointer_mut(&parent) else { break };
            if let Some(o) = p.as_object_mut() {
                let child = o.entry(seg.clone()).or_insert_with(|| Value::Object(Default::default()));
                if child.is_null() {
                    *child = Value::Object(Default::default());
                }
            }
        }
        let Some(obj) = doc.pointer_mut(&pointer(&self.path)).and_then(Value::as_object_mut) else {
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
        let ptr = pointer(&self.path);
        if self.doc.pointer(&ptr).is_none_or(|v| !v.is_object()) {
            return;
        }
        let list = self
            .doc
            .pointer_mut(&ptr)
            .and_then(Value::as_object_mut)
            .map(|o| o.entry(name.clone()).or_insert_with(|| Value::Array(Vec::new())));
        let Some(Value::Array(items)) = list else { return };
        items.push(Value::Object(Default::default()));
        let ix = items.len() - 1;
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
        let mut out: Vec<String> = lines[..a].iter().map(|s| s.to_string()).collect();
        out.extend(yaml.trim_end().lines().map(String::from));
        out.extend(lines[b..].iter().map(|s| s.to_string()));
        let joined = out.join("\n") + "\n";
        self.editor.update(cx, |e, cx| e.set_value(joined, window, cx));
    }
}

impl Render for SchemaForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let can_write = matches!(self.schema, Some(Ok(_))) && !self.rows.is_empty();
        let kind = self.kind.as_ref().map(|k| k.1.clone()).unwrap_or_else(|| "?".into());
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
                                        .map(String::from)
                                        .unwrap_or_else(|| format!("#{ix}"));
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
}
