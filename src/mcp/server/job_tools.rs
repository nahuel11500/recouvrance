//! MCP tools to run a CronJob now and to debug a pod with an ephemeral container, like
//! `kubectl create job --from=cronjob/…` and `kubectl debug`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for};
use crate::k8s;
use crate::mcp::Verb;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TriggerParams {
    pub context: String,
    pub namespace: String,
    #[schemars(description = "the CronJob")]
    pub name: String,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DebugParams {
    pub context: String,
    pub namespace: String,
    pub pod: String,
    #[schemars(description = "the container whose processes to see (shares its process namespace); omit for none")]
    pub target: Option<String>,
    #[schemars(description = "the debug image (default busybox:1.37)")]
    pub image: Option<String>,
    pub dry_run: Option<bool>,
}

/// The debug image when the agent doesn't name one.
const DEBUG_IMAGE: &str = "busybox:1.37";

#[tool_router(router = job_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Start a Job from a CronJob now, like `kubectl create job --from=cronjob/NAME`. Needs apply on jobs (the Job is named NAME-manual-<number>). Supports dry_run."
    )]
    async fn trigger_cronjob(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TriggerParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "name": p.name, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "trigger_cronjob", args)?;
        let (_, cronjobs) = self.kind(&call, &p.context, "cronjobs").await?;
        let read = access_for(&cronjobs, &p.context, Verb::Get, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &read).await?;
        let (client, jobs) = self.kind(&call, &p.context, "jobs").await?;
        let job_name = k8s::manual_job_name(&p.name);
        let create = access_for(&jobs, &p.context, Verb::Apply, Some(p.namespace.clone()), Some(job_name.clone()));
        self.authorize(&mut call, &create).await?;
        self.recheck(&mut call, &create)?;
        let done = k8s::trigger_cronjob_named(client, p.namespace, p.name, job_name, p.dry_run.unwrap_or(false)).await;
        call.finish(done)
    }

    #[tool(
        description = "Add an ephemeral debug container to a pod, like `kubectl debug --target`, and return its name: then run commands in it with exec_command (container = that name). It stays until the pod goes. Needs the exec verb on the pod. Supports dry_run."
    )]
    async fn debug_pod(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<DebugParams>,
    ) -> Result<String, String> {
        let image = p.image.clone().unwrap_or_else(|| DEBUG_IMAGE.into());
        let args = json!({"context": p.context, "namespace": p.namespace, "pod": p.pod, "container": p.target, "image": image, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "debug_pod", args)?;
        let (client, kind) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&kind, &p.context, Verb::Exec, Some(p.namespace.clone()), Some(p.pod.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        let dry_run = p.dry_run.unwrap_or(false);
        let done = k8s::add_debug_container(client, p.namespace, p.pod.clone(), image, p.target, dry_run).await;
        call.finish(done.map(|name| {
            if dry_run {
                name
            } else {
                format!("Debug container {name} runs in pod {}; use exec_command with container {name:?}.", p.pod)
            }
        }))
    }
}
