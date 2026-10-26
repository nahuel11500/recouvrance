//! MCP tools for nodes: cordon, uncordon and their usage, like `kubectl cordon` and
//! `kubectl top nodes`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for, format_table};
use crate::k8s;
use crate::mcp::Verb;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CordonParams {
    pub context: String,
    pub node: String,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TopNodesParams {
    pub context: String,
    #[schemars(description = "\"cpu\" (default) or \"memory\"")]
    pub sort_by: Option<String>,
}

#[tool_router(router = node_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Mark a node unschedulable, like `kubectl cordon`: running pods stay, new ones go elsewhere. Needs the drain verb. Supports dry_run."
    )]
    async fn cordon_node(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<CordonParams>,
    ) -> Result<String, String> {
        self.set_cordon(&parts, "cordon_node", p, true).await
    }

    #[tool(
        description = "Make a node schedulable again, like `kubectl uncordon`. Needs the drain verb. Supports dry_run."
    )]
    async fn uncordon_node(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<CordonParams>,
    ) -> Result<String, String> {
        self.set_cordon(&parts, "uncordon_node", p, false).await
    }

    #[tool(
        description = "Nodes' CPU and memory use against what they can allocate, like `kubectl top nodes`.",
        annotations(read_only_hint = true)
    )]
    async fn top_nodes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TopNodesParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "sort_by": p.sort_by});
        let mut call = self.begin(&parts, "top_nodes", args)?;
        let (client, kind) = self.kind(&call, &p.context, "nodes").await?;
        let access = access_for(&kind, &p.context, Verb::List, None, None);
        self.authorize_list(&mut call, &access, true).await?;
        let usage = match k8s::node_usage(client).await {
            Ok(u) => u,
            // No metrics API: say what is missing, not "404 page not found".
            Err(e) if k8s::api_status(&e) == Some(404) => {
                return call
                    .finish(Err(anyhow::anyhow!("no node metrics: metrics-server is not installed in this cluster")));
            }
            Err(e) => return call.finish(Err(e)),
        };
        let pct = |used: f64, alloc: f64| if alloc > 0.0 { used / alloc * 100.0 } else { 0.0 };
        // (name, cpu, cpu %, memory, memory %)
        let mut rows: Vec<(String, f64, f64, f64, f64)> = usage
            .into_iter()
            .filter(|(name, _)| call.may_see(&access_for(&kind, &p.context, Verb::List, None, Some(name.clone()))))
            .map(|(name, ((cpu, mem), (acpu, amem)))| (name, cpu, pct(cpu, acpu), mem, pct(mem, amem)))
            .collect();
        let by_mem = p.sort_by.as_deref() == Some("memory");
        rows.sort_by(|a, b| if by_mem { b.4.total_cmp(&a.4) } else { b.2.total_cmp(&a.2) });
        let table: Vec<Vec<String>> = rows
            .into_iter()
            .map(|(name, cpu, cpu_pct, mem, mem_pct)| {
                vec![
                    name,
                    k8s::format_cpu(cpu),
                    format!("{cpu_pct:.0}%"),
                    k8s::format_bytes(mem),
                    format!("{mem_pct:.0}%"),
                ]
            })
            .collect();
        call.finish(Ok(format_table(&["NODE", "CPU", "CPU%", "MEMORY", "MEMORY%"], &table)))
    }
}

impl KubeMcp {
    async fn set_cordon(
        &self,
        parts: &Parts,
        tool: &'static str,
        p: CordonParams,
        cordon: bool,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "node": p.node, "dry_run": p.dry_run});
        let mut call = self.begin(parts, tool, args)?;
        let (client, kind) = self.kind(&call, &p.context, "nodes").await?;
        let access = access_for(&kind, &p.context, Verb::Drain, None, Some(p.node.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        let dry_run = p.dry_run.unwrap_or(false);
        let patch = json!({"spec": {"unschedulable": cordon}});
        let done = k8s::merge_patch(client, kind, None, p.node.clone(), patch, dry_run).await;
        let what = if cordon { "cordoned" } else { "uncordoned" };
        call.finish(done.map(|_| format!("{}node {} {what}", if dry_run { "[dry run] " } else { "" }, p.node)))
    }
}
