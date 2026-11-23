//! `diff_manifest`: what applying a manifest would change, like `kubectl diff`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{KubeMcp, access_for};
use crate::k8s;
use crate::mcp::preview::{CONFLICT, dry_run_apply, without_noise};
use crate::mcp::{Mode, Verb};

/// What `apply_manifest` takes, without `dry_run`: a diff is always a dry run, and its schema
/// must not offer a choice that does nothing.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct DiffParams {
    pub context: String,
    #[schemars(description = "one or more YAML documents, as apply_manifest takes them")]
    pub manifest: String,
    #[schemars(description = "namespace for objects that don't set one")]
    pub namespace: Option<String>,
}

#[tool_router(router = diff_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "What applying a manifest would change, as a unified diff per object, like `kubectl diff` (a server-side dry run: nothing changes). Needs get on each object; Secret values are masked unless you may reveal them.",
        annotations(read_only_hint = true)
    )]
    async fn diff_manifest(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<DiffParams>,
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
            let kind = match self.served(&p.context, &kind).await {
                Ok(Some(k)) => k,
                // Its CRD comes with the manifest: the server can't check the object yet.
                Ok(None) => {
                    let after = k8s::to_yaml(&without_noise(&doc), false, false).unwrap_or_default();
                    let ns = ns.map_or_default(|n| format!("{n}/"));
                    let title = format!("{} {ns}{name} (not checked: its CRD is not on the cluster yet)", kind.ar.kind);
                    out.push(unified(&title, "", &after, true));
                    continue;
                }
                Err(e) => return call.finish(Err(anyhow::anyhow!("{} {name}: {e}", kind.ar.kind))),
            };
            let reveal = {
                let a = access_for(&kind, &p.context, Verb::Reveal, ns.clone(), Some(name.clone()));
                call.policy().decide(&a).mode == Mode::Allow
            };
            let secret = kind.is("", "Secret");
            // Fields another manager owns make the apply fail: say so, and show the diff a
            // forced apply would make.
            let applied = match dry_run_apply(&client, &kind, ns.clone(), &name, doc).await {
                Ok(a) => a,
                Err(e) => return call.finish(Err(e.context(format!("{} {name}", kind.ar.kind)))),
            };
            let note = if applied.conflict { format!(" ({CONFLICT})") } else { String::new() };
            let shown = |v: &Value| k8s::to_yaml(&without_noise(v), secret, reveal).unwrap_or_default();
            let (before, after) = (applied.live.as_ref().map_or_default(shown), shown(&applied.after));
            let title = format!("{} {}{name}{note}", kind.ar.kind, ns.map_or_default(|n| format!("{n}/")));
            out.push(unified(&title, &before, &after, applied.live.is_none()));
        }
        call.finish(Ok(out.join("\n")))
    }
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

    /// `diff_manifest` says a field manager conflict in the words it used before the approval
    /// banner shared them.
    #[test]
    fn the_conflict_note_reads_as_before() {
        assert_eq!(
            format!(" ({})", super::CONFLICT),
            " (apply_manifest would fail: another field manager owns some of these fields; shown as if forced)"
        );
    }

    #[test]
    fn noise_goes_and_changes_show() {
        let v = json!({"metadata": {"name": "a", "resourceVersion": "9", "managedFields": []}, "status": {}, "data": {"k": "v"}});
        assert_eq!(without_noise(&v), json!({"metadata": {"name": "a"}, "data": {"k": "v"}}));
        let reordered = json!({"data": {"k": "v"}, "metadata": {"resourceVersion": "9", "name": "a"}});
        let text = |v: &serde_json::Value| crate::k8s::to_yaml(&without_noise(v), false, false).unwrap();
        assert_eq!(text(&v), text(&reordered));
        assert_eq!(unified("ConfigMap a", "x: 1\n", "x: 1\n", false), "ConfigMap a: no change");
        let d = unified("ConfigMap a", "x: 1\n", "x: 2\n", false);
        assert!(d.contains("-x: 1") && d.contains("+x: 2"), "{d}");
    }
}
