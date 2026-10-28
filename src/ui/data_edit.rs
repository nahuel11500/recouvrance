//! Edit the values of a ConfigMap or Secret as key/value rows (no base64 by hand),
//! or create one from a form (Secret types: Opaque, docker registry, TLS, basic auth).
//! Saving validates with a server dry run first.

use base64::Engine as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::k8s::{self, Kind};

pub const SECRET_TYPES: [(&str, &str); 4] = [
    ("Opaque", "Key/value pairs"),
    ("kubernetes.io/dockerconfigjson", "Registry credentials for imagePullSecrets"),
    ("kubernetes.io/tls", "A certificate and its private key"),
    ("kubernetes.io/basic-auth", "A username and password"),
];

struct Pair {
    key: Entity<InputState>,
    value: Entity<TextareaState>,
    /// A Secret value that is not UTF-8 text (a keystore, an image…): shown read-only and
    /// saved as the original base64, untouched. Holds that base64 and the label to show.
    kept: Option<(String, String)>,
    /// A Secret's existing value is masked until the user reveals that row.
    revealed: bool,
}

/// Saved (or created): the parent closes the editor and reloads.
pub struct DataSaved(pub String);

impl EventEmitter<DataSaved> for DataEditor {}

pub struct DataEditor {
    context: String,
    client: Client,
    kind: Kind,
    namespace: Option<String>,
    /// The object being edited; None when creating.
    original: Option<serde_json::Value>,
    name: Entity<InputState>,
    secret_type: usize,
    pairs: Vec<Pair>,
    /// Docker registry form: server, username, password, email.
    registry: [Entity<InputState>; 4],
    status: Option<(bool, String)>,
    busy: bool,
    _task: Option<Task<()>>,
}

/// A stored value as the form shows it.
#[derive(Debug, PartialEq)]
enum Decoded {
    /// Editable text.
    Text(String),
    /// Not UTF-8 (or not even valid base64): kept as the original base64, with a label.
    Kept { b64: String, label: String },
}

fn decode(v: &serde_json::Value, secret: bool) -> Decoded {
    let s = v.as_str().unwrap_or_default();
    if !secret {
        return Decoded::Text(s.to_string());
    }
    match base64::engine::general_purpose::STANDARD.decode(s) {
        Ok(b) => match String::from_utf8(b) {
            Ok(t) => Decoded::Text(t),
            Err(e) => Decoded::Kept { b64: s.to_string(), label: format!("binary, {} bytes", e.as_bytes().len()) },
        },
        Err(_) => Decoded::Kept { b64: s.to_string(), label: "not valid base64, kept as it is".into() },
    }
}

/// Whether a stored value is shown in clear when the editor opens: a Secret's is masked
/// (an empty one has nothing to hide); a ConfigMap's is not secret.
fn starts_revealed(secret: bool, value: &str) -> bool {
    !secret || value.is_empty()
}

/// Create or replace, as one future type.
async fn send(
    creating: bool,
    client: Client,
    kind: Kind,
    ns: Option<String>,
    obj: serde_json::Value,
    dry_run: bool,
) -> Result<String> {
    if creating {
        k8s::create(client, kind, ns, obj, dry_run).await
    } else {
        k8s::replace(client, kind, ns, obj, dry_run).await
    }
}

impl DataEditor {
    pub fn new(
        context: String,
        client: Client,
        kind: Kind,
        namespace: Option<String>,
        original: Option<serde_json::Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let secret = kind.is("", "Secret");
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("name"));
        let registry = [
            cx.new(|cx| InputState::new(window, cx).placeholder("registry, e.g. ghcr.io")),
            cx.new(|cx| InputState::new(window, cx).placeholder("username")),
            cx.new(|cx| InputState::new(window, cx).placeholder("password or token").masked(true)),
            cx.new(|cx| InputState::new(window, cx).placeholder("email (optional)")),
        ];
        let mut this = Self {
            context,
            client,
            kind,
            namespace,
            original: original.clone(),
            name,
            secret_type: 0,
            pairs: Vec::new(),
            registry,
            status: None,
            busy: false,
            _task: None,
        };
        match original.as_ref().and_then(|o| o.get("data")).and_then(|d| d.as_object()) {
            Some(data) => {
                for (k, v) in data {
                    match decode(v, secret) {
                        Decoded::Text(t) => {
                            this.add_pair(k, &t, window, cx);
                            // Not shown in clear on open (screen sharing, someone looking on).
                            if let Some(p) = this.pairs.last_mut() {
                                p.revealed = starts_revealed(secret, &t);
                            }
                        }
                        Decoded::Kept { b64, label } => {
                            this.add_pair(k, "", window, cx);
                            if let Some(p) = this.pairs.last_mut() {
                                p.kept = Some((b64, label));
                            }
                        }
                    }
                }
            }
            None => this.add_pair("", "", window, cx),
        }
        if let Some(t) = original.as_ref().and_then(|o| o["type"].as_str()) {
            this.secret_type = SECRET_TYPES.iter().position(|(n, _)| *n == t).unwrap_or(0);
        }
        this
    }

    fn add_pair(&mut self, key: &str, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (key, value) = (key.to_string(), value.to_string());
        let k = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("key");
            s.set_value(key, window, cx);
            s
        });
        let v = cx.new(|cx| {
            let mut s = TextareaState::new(window, cx).auto_grow(1, 10).placeholder("value");
            s.set_value(value, window, cx);
            s
        });
        self.pairs.push(Pair { key: k, value: v, kept: None, revealed: true });
        cx.notify();
    }

    fn set_type(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.secret_type = ix;
        // Prefill the keys that type expects.
        let keys: &[&str] = match SECRET_TYPES[ix].0 {
            "kubernetes.io/tls" => &["tls.crt", "tls.key"],
            "kubernetes.io/basic-auth" => &["username", "password"],
            _ => &[],
        };
        if !keys.is_empty() {
            self.pairs.clear();
            for k in keys {
                self.add_pair(k, "", window, cx);
            }
        }
        cx.notify();
    }

    fn is_secret(&self) -> bool {
        self.kind.is("", "Secret")
    }

    /// The registry form only creates a new docker Secret; an existing one (which may hold
    /// several registries) is edited as key/value rows like any other.
    fn registry_form(&self) -> bool {
        self.is_secret()
            && self.original.is_none()
            && SECRET_TYPES[self.secret_type].0 == "kubernetes.io/dockerconfigjson"
    }

    /// The object to send, or what is wrong with the form.
    fn build(&self, cx: &App) -> Result<serde_json::Value, String> {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        let secret = self.is_secret();
        let mut data = serde_json::Map::new();
        if self.registry_form() {
            let [server, user, pass, email] = self.registry.each_ref().map(|i| i.read(cx).value().trim().to_string());
            if server.is_empty() || user.is_empty() || pass.is_empty() {
                return Err("Registry, username and password are required.".into());
            }
            let auth = serde_json::json!({ "auths": { server: {
                "username": user, "password": pass, "email": email, "auth": b64(&format!("{user}:{pass}")),
            }}});
            data.insert(".dockerconfigjson".into(), b64(&auth.to_string()).into());
        } else {
            for p in &self.pairs {
                let k = p.key.read(cx).value().trim().to_string();
                let v = p.value.read(cx).value().to_string();
                if k.is_empty() {
                    if v.is_empty() && p.kept.is_none() {
                        continue;
                    }
                    return Err("A value has no key.".into());
                }
                if !k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                    return Err(format!("Key `{k}`: only letters, digits, '-', '_' and '.' are allowed."));
                }
                if data.contains_key(&k) {
                    return Err(format!("Key `{k}` appears twice."));
                }
                let value = match &p.kept {
                    Some((orig, _)) => orig.clone(),
                    None if secret => b64(&v),
                    None => v,
                };
                data.insert(k, value.into());
            }
        }
        if let Some(o) = &self.original {
            let mut o = o.clone();
            o["data"] = data.into();
            if let Some(m) = o.as_object_mut() {
                m.remove("stringData");
            }
            Ok(o)
        } else {
            let name = self.name.read(cx).value().trim().to_string();
            if name.is_empty() {
                return Err("A name is required.".into());
            }
            let mut o = serde_json::json!({
                "apiVersion": "v1",
                "kind": self.kind.ar.kind,
                "metadata": { "name": name, "namespace": self.namespace.clone().unwrap_or_else(|| "default".into()) },
                "data": data,
            });
            if secret {
                o["type"] = SECRET_TYPES[self.secret_type].0.into();
            }
            Ok(o)
        }
    }

    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let obj = match self.build(cx) {
            Ok(o) => o,
            Err(e) => {
                self.status = Some((false, e));
                cx.notify();
                return;
            }
        };
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        let creating = self.original.is_none();
        let ns = self
            .namespace
            .clone()
            .or_else(|| obj.pointer("/metadata/namespace").and_then(|v| v.as_str()).map(String::from));
        let (client, kind) = (self.client.clone(), self.kind.clone());
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let cmd = format!(
            "kubectl --context {}{} {} {} {name}",
            self.context,
            ns.as_ref().map(|n| format!(" -n {n}")).unwrap_or_default(),
            if creating { "create" } else { "replace" },
            self.kind.ar.kind.to_lowercase()
        );
        let guard = crate::settings::is_protected(&self.context).then(|| {
            let ctx = self.context.clone();
            (cx.new(|cx| InputState::new(window, cx).placeholder(ctx.clone())), ctx)
        });
        let run_as = crate::details::RunAs::offer(&self.context, cx);
        let context = self.context.clone();
        self.busy = true;
        self.status = None;
        cx.notify();
        let dry = send(creating, client.clone(), kind.clone(), ns.clone(), obj.clone(), true);
        let me = cx.entity().downgrade();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let check = dry.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                cx.notify();
                // Refused to you, with an identity to run as: the confirmation offers it.
                let description = match &check {
                    Ok(_) => "The server dry run passed.".to_string(),
                    Err(e) if crate::details::refused_but_can_run_as(e, &context) => format!(
                        "The server refused it to you in a dry run ({}): turn on Run as to save it as that identity.",
                        k8s::short_error(e)
                    ),
                    Err(e) => {
                        this.status = Some((false, format!("The server refused it: {e:#}")));
                        return;
                    }
                };
                if !crate::details::writes_still_on(window, cx) {
                    return;
                }
                let title = format!("{} {} {name}?", if creating { "Create" } else { "Save" }, this.kind.ar.kind);
                window.open_alert_dialog(cx, move |alert, _, cx| {
                    let (me, guard, obj, kind, ns, client, run_as) = (
                        me.clone(),
                        guard.clone(),
                        obj.clone(),
                        kind.clone(),
                        ns.clone(),
                        client.clone(),
                        run_as.clone(),
                    );
                    alert
                        .title(title.clone())
                        .description(description.clone())
                        .child(crate::details::confirm_with(&cmd, guard.as_ref(), &run_as, cx))
                        .show_cancel(true)
                        .ok_text(if creating { "Create" } else { "Save" })
                        .on_ok(move |_, window, cx| {
                            if !crate::details::guard_ok(guard.as_ref(), cx) {
                                return false;
                            }
                            if !crate::details::writes_still_on(window, cx) {
                                return true;
                            }
                            let client = run_as.client(&client, cx);
                            let fut = send(creating, client, kind.clone(), ns.clone(), obj.clone(), false);
                            let _ = me.update(cx, |this, cx| this.write(fut, window, cx));
                            true
                        })
                });
            });
        }));
    }

    fn write(
        &mut self,
        fut: impl Future<Output = Result<String>> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let r = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match r {
                    Ok(msg) => {
                        window.push_notification(component::notification::Notification::success(msg.clone()), cx);
                        cx.emit(DataSaved(msg));
                    }
                    Err(e) => this.status = Some((false, format!("{e:#}"))),
                }
                cx.notify();
            });
        }));
    }

    /// Snapshot hook: fill the form (`key=value` pairs separated by `;`, name first when creating).
    #[cfg(feature = "dev")]
    pub fn demo_fill(&mut self, spec: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut parts = spec.split(';');
        if self.original.is_none()
            && let Some(n) = parts.next()
        {
            self.name.update(cx, |s, cx| s.set_value(n.to_string(), window, cx));
            self.pairs.clear();
        }
        for p in parts {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            match self.pairs.iter().find(|x| x.key.read(cx).value().as_ref() == k) {
                Some(pair) => pair.value.update(cx, |s, cx| s.set_value(v.to_string(), window, cx)),
                None => self.add_pair(k, v, window, cx),
            }
        }
        cx.notify();
    }
}

impl Render for DataEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let secret = self.is_secret();
        let creating = self.original.is_none();
        let docker = self.registry_form();
        let binary = self
            .original
            .as_ref()
            .and_then(|o| o.get("binaryData"))
            .and_then(|d| d.as_object())
            .map_or(0, serde_json::Map::len);
        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(if creating {
                        format!("New {}", self.kind.ar.kind)
                    } else {
                        "Edit values".to_string()
                    }))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(if secret {
                        "Values are plain text (existing ones masked until revealed); base64-encoded when saved."
                    } else {
                        ""
                    })),
            )
            .when(creating, |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(90.)).text_sm().child("Name"))
                        .child(div().w(px(320.)).child(Input::new(&self.name).small()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("in {}", self.namespace.clone().unwrap_or_else(|| "default".into()))),
                        ),
                )
                .when(secret, |d| {
                    d.child(h_flex().gap_2().child(div().w(px(90.)).text_sm().child("Type")).children(
                        SECRET_TYPES.iter().enumerate().map(|(i, (t, desc))| {
                            Button::new(("stype", i))
                                .xsmall()
                                .label(t.trim_start_matches("kubernetes.io/").to_string())
                                .tooltip(*desc)
                                .when(i == self.secret_type, ButtonVariants::primary)
                                .when(i != self.secret_type, ButtonVariants::ghost)
                                .on_click(cx.listener(move |this, _, w, cx| this.set_type(i, w, cx)))
                        }),
                    ))
                })
            })
            .when(docker, |d| {
                d.children(["Registry", "Username", "Password", "Email"].into_iter().zip(self.registry.iter()).map(
                    |(label, input)| {
                        h_flex()
                            .gap_2()
                            .child(div().w(px(90.)).text_sm().child(label))
                            .child(div().w(px(320.)).child(Input::new(input).small()))
                    },
                ))
            })
            .when(!docker, |d| {
                d.children(self.pairs.iter().enumerate().map(|(i, p)| {
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(div().w(px(220.)).child(Input::new(&p.key).small()))
                        .child(match &p.kept {
                            Some((_, label)) => div()
                                .flex_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!("{label} (kept as it is)")),
                            None if p.revealed => div().flex_1().child(Textarea::new(&p.value).small()),
                            None => div().flex_1().text_sm().text_color(theme.muted_foreground).child("••••••"),
                        })
                        .when(secret && p.kept.is_none(), |d| {
                            d.child(
                                Button::new(("reveal-pair", i))
                                    .xsmall()
                                    .ghost()
                                    .icon(if p.revealed { Lucide::EyeOff } else { Lucide::Eye })
                                    .tooltip(if p.revealed { "Mask this value" } else { "Reveal this value" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(p) = this.pairs.get_mut(i) {
                                            p.revealed = !p.revealed;
                                        }
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(
                            Button::new(("rm-pair", i))
                                .xsmall()
                                .ghost()
                                .icon(Lucide::X)
                                .tooltip("Remove this key")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pairs.remove(i);
                                    cx.notify();
                                })),
                        )
                }))
                .child(
                    h_flex().child(
                        Button::new("add-pair")
                            .xsmall()
                            .ghost()
                            .icon(Lucide::Plus)
                            .label("Add key")
                            .on_click(cx.listener(|this, _, w, cx| this.add_pair("", "", w, cx))),
                    ),
                )
            })
            .when(binary > 0, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{binary} binary key(s) are kept as they are.")),
                )
            })
            .when_some(self.status.clone(), |d, (ok, msg)| {
                d.child(div().text_sm().text_color(if ok { theme.success } else { theme.danger }).child(msg))
            })
            .child(
                h_flex().gap_2().child(
                    Button::new("data-save")
                        .small()
                        .primary()
                        .label(if creating { "Create…" } else { "Save…" })
                        .loading(self.busy)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, w, cx| this.save(w, cx))),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Decoded, decode, starts_revealed};
    use base64::Engine as _;
    use serde_json::json;

    #[test]
    fn existing_secret_values_start_masked() {
        assert!(!starts_revealed(true, "example-value"));
        assert!(starts_revealed(true, ""));
        assert!(starts_revealed(false, "plain config"));
    }

    #[test]
    fn utf8_secret_values_decode_to_text() {
        assert_eq!(decode(&json!("aGVsbG8="), true), Decoded::Text("hello".into()));
        assert_eq!(decode(&json!("plain"), false), Decoded::Text("plain".into()));
    }

    #[test]
    fn binary_secret_values_are_kept() {
        // 0xff 0xfe 0x00: not UTF-8.
        let b64 = base64::engine::general_purpose::STANDARD.encode([0xffu8, 0xfe, 0x00]);
        assert_eq!(decode(&json!(b64), true), Decoded::Kept { b64, label: "binary, 3 bytes".into() });
        assert!(matches!(decode(&json!("@@not base64@@"), true), Decoded::Kept { .. }));
    }
}
