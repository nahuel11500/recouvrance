//! MCP tools to read Helm releases, like `helm list`, `helm history` and `helm get values`.
//! Helm keeps each revision in a Secret, so these need the agent's rules on secrets.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for, format_table};
use crate::helm::{self, Release};
use crate::mcp::{Mode, Verb};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HelmListParams {
    pub context: String,
    #[schemars(description = "namespace; omit for all namespaces you may read")]
    pub namespace: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HelmReleaseParams {
    pub context: String,
    pub namespace: String,
    #[schemars(description = "the release")]
    pub name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HelmValuesParams {
    pub context: String,
    pub namespace: String,
    #[schemars(description = "the release")]
    pub name: String,
    #[schemars(description = "the revision; omit for the current one")]
    pub revision: Option<i64>,
}

/// The Secret Helm keeps a revision in.
fn secret_name(r: &Release) -> String {
    format!("sh.helm.release.v1.{}.v{}", r.name, r.revision)
}

fn updated(r: &Release) -> String {
    r.updated.map(|t| t.strftime("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

#[tool_router(router = helm_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Helm releases with their chart, app version and status, like `helm list`. Reads Helm's Secrets: needs list on secrets.",
        annotations(read_only_hint = true)
    )]
    async fn list_helm_releases(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<HelmListParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace});
        let mut call = self.begin(&parts, "list_helm_releases", args)?;
        let (client, kind) = self.kind(&call, &p.context, "secrets").await?;
        let access = access_for(&kind, &p.context, Verb::List, p.namespace.clone(), None);
        self.authorize_list(&mut call, &access, p.namespace.is_none()).await?;
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        if let Err(e) = helm::fill_releases(client, p.namespace.clone(), tx).await {
            return call.finish(Err(e));
        }
        let mut releases = Vec::new();
        while let Ok(page) = rx.try_recv() {
            releases.extend(page);
        }
        let mut releases = helm::latest_revisions(releases);
        releases.retain(|r| {
            call.may_see(&access_for(&kind, &p.context, Verb::List, Some(r.namespace.clone()), Some(secret_name(r))))
        });
        let rows: Vec<Vec<String>> = releases
            .iter()
            .map(|r| {
                vec![
                    r.namespace.clone(),
                    r.name.clone(),
                    r.revision.to_string(),
                    r.status.clone(),
                    format!("{}-{}", r.chart, r.chart_version),
                    r.app_version.clone(),
                    updated(r),
                ]
            })
            .collect();
        call.finish(Ok(format_table(
            &["NAMESPACE", "NAME", "REVISION", "STATUS", "CHART", "APP VERSION", "UPDATED"],
            &rows,
        )))
    }

    #[tool(
        description = "A Helm release's revisions, newest first, like `helm history`. Needs list on secrets in its namespace.",
        annotations(read_only_hint = true)
    )]
    async fn helm_history(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<HelmReleaseParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "name": p.name});
        let mut call = self.begin(&parts, "helm_history", args)?;
        let (client, kind) = self.kind(&call, &p.context, "secrets").await?;
        let access = access_for(&kind, &p.context, Verb::List, Some(p.namespace.clone()), None);
        self.authorize_list(&mut call, &access, false).await?;
        let revs = match helm::history(client, p.namespace.clone(), p.name.clone()).await {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        let rows: Vec<Vec<String>> = revs
            .iter()
            .filter(|r| {
                call.may_see(&access_for(
                    &kind,
                    &p.context,
                    Verb::List,
                    Some(r.namespace.clone()),
                    Some(secret_name(r)),
                ))
            })
            .map(|r| {
                vec![
                    r.revision.to_string(),
                    r.status.clone(),
                    format!("{}-{}", r.chart, r.chart_version),
                    r.app_version.clone(),
                    updated(r),
                    r.description.clone(),
                ]
            })
            .collect();
        if rows.is_empty() {
            return call.finish(Err(anyhow::anyhow!("no release {} in {}", p.name, p.namespace)));
        }
        call.finish(Ok(format_table(&["REVISION", "STATUS", "CHART", "APP VERSION", "UPDATED", "DESCRIPTION"], &rows)))
    }

    #[tool(
        description = "The values a Helm release was installed with, like `helm get values`. Secret-looking values are masked unless you may reveal secrets. Needs get on the release's Secret.",
        annotations(read_only_hint = true)
    )]
    async fn helm_get_values(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<HelmValuesParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "name": p.name, "revision": p.revision});
        let mut call = self.begin(&parts, "helm_get_values", args)?;
        let (client, kind) = self.kind(&call, &p.context, "secrets").await?;
        // Which revision is current is only known from the Secrets: may it list them first.
        let list = access_for(&kind, &p.context, Verb::List, Some(p.namespace.clone()), None);
        self.authorize_list(&mut call, &list, false).await?;
        let revs = match helm::history(client, p.namespace.clone(), p.name.clone()).await {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        let Some(rev) = (match p.revision {
            Some(n) => revs.into_iter().find(|r| r.revision == n),
            None => revs.into_iter().next(),
        }) else {
            return call.finish(Err(anyhow::anyhow!("no such release or revision")));
        };
        let secret = secret_name(&rev);
        let access = access_for(&kind, &p.context, Verb::Get, Some(p.namespace.clone()), Some(secret.clone()));
        self.authorize(&mut call, &access).await?;
        let reveal = {
            let a = access_for(&kind, &p.context, Verb::Reveal, Some(p.namespace.clone()), Some(secret));
            call.policy().decide(&a).mode == Mode::Allow
        };
        let mut values = rev.values;
        if !reveal {
            helm::mask_secrets(&mut values);
        }
        let yaml = crate::yaml::to_string(&values).map_err(anyhow::Error::from);
        call.finish(yaml.map(|y| format!("# {} revision {}\n{y}", p.name, rev.revision)))
    }
}
