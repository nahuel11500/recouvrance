//! `get_changes`: what changed on an object, from what the cluster keeps (see `crate::history`).

use std::sync::Arc;

use http::request::Parts;
use k8s_openapi::api::core::v1::Secret;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for, check_identity, require_ns, visible};
use crate::history::{self, Allowed, Read};
use crate::k8s::{self, Kind, ObjectRef};
use crate::mcp::{Policy, Verb};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ChangesParams {
    pub context: String,
    #[schemars(description = "resource type like kubectl: deployment, pod, application, ...")]
    pub kind: String,
    pub name: String,
    #[schemars(description = "required for namespaced resources")]
    pub namespace: Option<String>,
    #[schemars(description = "the most changes returned, newest first (default 30, at most 200)")]
    pub limit: Option<u32>,
}

#[tool_router(router = history_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "What changed on an object, newest first, each with its source, what changed and a short diff: the revisions of the workload that rolls it out (a pod's: its owner's), of the Helm release that installed it (values masked), an Argo CD Application's deployments, which field managers last wrote which fields and when (managedFields), its events, and the changes Recouvrance recorded while it watched the object (source Recorded: ConfigMaps, Services, scale changes... that the cluster keeps no history of; a Secret's values never, only which keys changed). Needs get on the object; a pod's owner needs get, the Helm revisions list on secrets, the events list on events.",
        annotations(read_only_hint = true)
    )]
    async fn get_changes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ChangesParams>,
    ) -> Result<String, String> {
        let args =
            json!({"context": p.context, "kind": p.kind, "name": p.name, "namespace": p.namespace, "limit": p.limit});
        let mut call = self.begin(&parts, "get_changes", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, p.namespace.as_deref())?;
        let access = access_for(&kind, &p.context, Verb::Get, p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let ns = if kind.namespaced { p.namespace.clone() } else { None };
        let obj = ObjectRef { kind: kind.clone(), name: p.name.clone(), namespace: ns.clone(), uid: String::new() };
        // With its managedFields: who wrote it, and when.
        let value = match k8s::get_object_opts(client.clone(), obj.clone(), true).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        if let Err(e) = check_identity(&kind, ns.as_deref(), &p.name, &value) {
            return call.finish(Err(e));
        }
        let allowed = by_rules(call.policy().clone(), call.approved_rules().to_vec(), p.context.clone());
        let fetched = history::fetch(client.clone(), kind.clone(), ns.clone(), value.clone(), allowed).await;
        let mut errors = fetched.errors;
        let subject = format!("{} {}", kind.ar.kind, p.name);
        let mut lists = vec![fetched.changes, history::own_changes(&kind, &value)];
        // What the app recorded while it watched this object (this one: by its uid).
        crate::timeline::opened(&p.context).await;
        let uid = value.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default();
        lists.push(crate::timeline::changes_for(&p.context, &kind.kubectl_name(), ns.as_deref(), &p.name, uid));
        let events_kind = self.kind(&call, &p.context, "events").await?.1;
        if call.may_see(&access_for(&events_kind, &p.context, Verb::List, ns.clone(), None)) {
            let uid = value.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            match k8s::events_for(client, ObjectRef { uid, ..obj }).await {
                Ok(mut events) => {
                    // The check above has no name: an Allow of some event names passes it.
                    events.retain(|e| {
                        let a =
                            access_for(&events_kind, &p.context, Verb::List, e.namespace.clone(), Some(e.name.clone()));
                        call.may_see(&a)
                    });
                    lists.push(history::event_changes(&events, &subject));
                }
                Err(e) => errors.push(format!("events: {}", k8s::short_error(&e))),
            }
        }
        let changes = history::merge(lists);
        let limit = p.limit.unwrap_or(30).clamp(1, 200) as usize;
        call.finish(Ok(history::report(&subject, &changes, &errors, limit)))
    }
}

/// What `history::fetch` may read for an agent: an owner it may get, Helm's Secrets it may list.
fn by_rules(policy: Policy, approved: Vec<usize>, context: String) -> Allowed {
    let secrets = Kind { ar: k8s::ar_of::<Secret>(), namespaced: true, section: "", label: "Secret".into() };
    Arc::new(move |read: Read<'_>| match read {
        Read::Get { kind, ns, name } => {
            visible(&policy, &approved, &access_for(kind, &context, Verb::Get, Some(ns.into()), Some(name.into())))
        }
        // Listing them at all: some rule must allow some of them (each is then checked).
        Read::Helm { ns, secret: None } => {
            let a = access_for(&secrets, &context, Verb::List, Some(ns.into()), None);
            visible(&policy, &approved, &a) || policy.allow_somewhere(&a).is_some()
        }
        Read::Helm { ns, secret: Some(name) } => {
            visible(&policy, &approved, &access_for(&secrets, &context, Verb::List, Some(ns.into()), Some(name.into())))
        }
    })
}
