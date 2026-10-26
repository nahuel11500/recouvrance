//! `diff_manifest`: what applying a manifest would change, like `kubectl diff`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde_json::{Value, json};

use super::{ApplyParams, KubeMcp, access_for};
use crate::k8s::{self, ObjectRef};
use crate::mcp::{Mode, Verb};

/// Fields the server sets on every write, which a diff would only show as noise.
const NOISE: [&str; 6] = [
    "/metadata/managedFields",
    "/metadata/resourceVersion",
    "/metadata/generation",
    "/metadata/uid",
    "/metadata/creationTimestamp",
    "/status",
];

#[tool_router(router = diff_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "What applying a manifest would change, as a unified diff per object, like `kubectl diff` (a server-side dry run: nothing changes). Needs get on each object; Secret values are masked unless you may reveal them.",
        annotations(read_only_hint = true)
    )]
    async fn diff_manifest(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ApplyParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "bytes": p.manifest.len()});
        let mut call = self.begin(&parts, "diff_manifest", args)?;
        let (client, planned) = self.plan_manifest(&call, &p.context, p.namespace.as_ref(), &p.manifest).await?;
        for (kind, ns, name, _) in &planned {
            let access = access_for(kind, &p.context, Verb::Get, ns.clone(), Some(name.clone()));
            self.authorize(&mut call, &access).await?;
        }
        let mut out = Vec::new();
        for (kind, ns, name, doc) in planned {
            let reveal = {
                let a = access_for(&kind, &p.context, Verb::Reveal, ns.clone(), Some(name.clone()));
                call.policy().decide(&a).mode == Mode::Allow
            };
            let secret = kind.is("", "Secret");
            let obj = ObjectRef { kind: kind.clone(), name: name.clone(), namespace: ns.clone(), uid: String::new() };
            let live = match k8s::get_object(client.clone(), obj).await {
                Ok(v) => Some(v),
                Err(e) if k8s::is_not_found(&e) => None,
                Err(e) => return call.finish(Err(e)),
            };
            // Fields another manager owns make the apply fail: say so, and show the diff a
            // forced apply would make.
            let mut note = "";
            let after = match k8s::apply_value(client.clone(), kind.clone(), ns.clone(), doc.clone(), true, false).await
            {
                Err(e) if k8s::api_status(&e) == Some(409) => {
                    note = " (apply_manifest would fail: another field manager owns some of these fields; shown as if forced)";
                    k8s::apply_value(client.clone(), kind.clone(), ns.clone(), doc, true, true).await
                }
                other => other,
            };
            let after = match after {
                Ok(v) => v,
                Err(e) => return call.finish(Err(e.context(format!("{} {name}", kind.ar.kind)))),
            };
            let shown = |v: &Value| k8s::to_yaml(&without_noise(v), secret, reveal).unwrap_or_default();
            let (before, after) = (live.as_ref().map(shown).unwrap_or_default(), shown(&after));
            let title = format!("{} {}{name}{note}", kind.ar.kind, ns.map(|n| format!("{n}/")).unwrap_or_default());
            out.push(unified(&title, &before, &after, live.is_none()));
        }
        call.finish(Ok(out.join("\n")))
    }
}

fn without_noise(v: &Value) -> Value {
    let mut v = v.clone();
    for path in NOISE {
        if let Some((parent, key)) = path.rsplit_once('/')
            && let Some(obj) =
                (if parent.is_empty() { Some(&mut v) } else { v.pointer_mut(parent) }).and_then(Value::as_object_mut)
        {
            obj.remove(key);
        }
    }
    v
}

/// A unified diff of one object's YAML, or a line saying it doesn't change.
fn unified(title: &str, before: &str, after: &str, created: bool) -> String {
    if before == after {
        return format!("{title}: no change");
    }
    let diff = similar::TextDiff::from_lines(before, after);
    let head = if created { format!("{title}: created") } else { format!("{title}: changed") };
    format!("{head}\n{}", diff.unified_diff().context_radius(3).header("live", "after apply"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{unified, without_noise};

    #[test]
    fn noise_goes_and_changes_show() {
        let v = json!({"metadata": {"name": "a", "resourceVersion": "9", "managedFields": []}, "status": {}, "data": {"k": "v"}});
        assert_eq!(without_noise(&v), json!({"metadata": {"name": "a"}, "data": {"k": "v"}}));
        assert_eq!(unified("ConfigMap a", "x: 1\n", "x: 1\n", false), "ConfigMap a: no change");
        let d = unified("ConfigMap a", "x: 1\n", "x: 2\n", false);
        assert!(d.contains("-x: 1") && d.contains("+x: 2"), "{d}");
    }
}
