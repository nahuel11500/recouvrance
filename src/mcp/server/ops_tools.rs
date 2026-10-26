//! MCP tools for day-to-day operations beyond get and change: port-forwards.

use futures::StreamExt as _;
use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for};
use crate::k8s::{self, ForwardMsg, ObjectRef};
use crate::mcp::Verb;
use crate::mcp::forwards::{DEFAULT_MINUTES, ForwardInfo, MAX_MINUTES, service_target_port};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PortForwardParams {
    pub context: String,
    pub namespace: String,
    #[schemars(description = "the pod to forward to; give this or service")]
    pub pod: Option<String>,
    #[schemars(description = "a Service: forwards to one of its pods, and follows them when they are replaced")]
    pub service: Option<String>,
    #[schemars(description = "the port: on the pod, or a port of the Service (its targetPort is used)")]
    pub port: u16,
    #[schemars(description = "local port on 127.0.0.1; omit for a free one")]
    pub local_port: Option<u16>,
    #[schemars(description = "how long it runs, in minutes (default 60, at most 480)")]
    pub minutes: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct StopForwardParams {
    #[schemars(description = "the forward's id, from port_forward or list_port_forwards")]
    pub id: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

/// How long `port_forward` waits for the listener before giving up.
const LISTEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[tool_router(router = ops_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Forward a local port on 127.0.0.1 to a pod or Service port, like `kubectl port-forward`. Returns the local address; the forward runs until stopped (stop_port_forward), its time is up (`minutes`), or agent access is switched off. The user sees it in the app."
    )]
    async fn port_forward(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<PortForwardParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "pod": p.pod, "service": p.service, "port": p.port, "local_port": p.local_port, "minutes": p.minutes});
        let mut call = self.begin(&parts, "port_forward", args)?;
        let (kind_name, name) = match (&p.pod, &p.service) {
            (Some(pod), None) => ("pods", pod.clone()),
            (None, Some(svc)) => ("services", svc.clone()),
            _ => return call.finish(Err(anyhow::anyhow!("give either pod or service"))),
        };
        if p.port == 0 {
            return call.finish(Err(anyhow::anyhow!("port must not be 0")));
        }
        let (client, kind) = self.kind(&call, &p.context, kind_name).await?;
        let access = access_for(&kind, &p.context, Verb::Forward, Some(p.namespace.clone()), Some(name.clone()));
        self.authorize(&mut call, &access).await?;

        // The pod, and the port on it.
        let (pod, remote) = match &p.service {
            None => (name.clone(), p.port),
            Some(svc) => {
                let pod = match k8s::resolve_forward_pod(
                    client.clone(),
                    p.namespace.clone(),
                    Some(svc.clone()),
                    String::new(),
                    None,
                )
                .await
                {
                    Ok(pod) => pod,
                    Err(e) => return call.finish(Err(e)),
                };
                let (svc_kind, pod_kind) = match (
                    self.kind(&call, &p.context, "services").await,
                    self.kind(&call, &p.context, "pods").await,
                ) {
                    (Ok((_, s)), Ok((_, p))) => (s, p),
                    (Err(e), _) | (_, Err(e)) => return Err(e),
                };
                let get = |kind, name: &str| {
                    k8s::get_object(
                        client.clone(),
                        ObjectRef {
                            kind,
                            name: name.to_string(),
                            namespace: Some(p.namespace.clone()),
                            uid: String::new(),
                        },
                    )
                };
                let (svc_obj, pod_obj) = match futures::try_join!(get(svc_kind, svc), get(pod_kind, &pod)) {
                    Ok(v) => v,
                    Err(e) => return call.finish(Err(e)),
                };
                match service_target_port(&svc_obj, p.port, &pod_obj) {
                    Ok(port) => (pod, port),
                    Err(e) => return call.finish(Err(anyhow::anyhow!(e))),
                }
            }
        };

        self.recheck(&mut call, &access)?;
        let (guard, mut rx) = k8s::port_forward(
            client,
            p.namespace.clone(),
            pod.clone(),
            p.service.clone(),
            "127.0.0.1".into(),
            p.local_port.unwrap_or(0),
            remote,
        );
        let local = match tokio::time::timeout(LISTEN_TIMEOUT, rx.next()).await {
            Ok(Some(ForwardMsg::Listening(port))) => port,
            Ok(Some(ForwardMsg::Error(e))) => return call.finish(Err(anyhow::anyhow!(e))),
            _ => return call.finish(Err(anyhow::anyhow!("the forward did not start"))),
        };
        let minutes = p.minutes.unwrap_or(DEFAULT_MINUTES).clamp(1, MAX_MINUTES);
        let forwards = self.hub.agent_forwards();
        let info = ForwardInfo {
            id: forwards.next_id(),
            agent_id: call.agent.id.clone(),
            agent: call.agent.name.clone(),
            context: p.context.clone(),
            namespace: p.namespace.clone(),
            target: format!("{}/{name}", if p.service.is_some() { "service" } else { "pod" }),
            remote,
            local,
            until: jiff::Timestamp::now() + jiff::SignedDuration::from_mins(i64::from(minutes)),
            connections: 0,
            error: None,
        };
        let (id, text) = (info.id, info.describe());
        forwards.add(info, guard);
        // Follow its connections and errors, and stop it when its time is up.
        let hub = self.hub.clone();
        let watcher = k8s::spawn_guarded(async move {
            let expiry = tokio::time::sleep(std::time::Duration::from_secs(u64::from(minutes) * 60));
            tokio::pin!(expiry);
            loop {
                tokio::select! {
                    msg = rx.next() => {
                        let Some(msg) = msg else { break };
                        hub.agent_forwards().update(id, |f| match msg {
                            ForwardMsg::Connection => {
                                f.connections += 1;
                                f.error = None;
                            }
                            ForwardMsg::Error(e) => f.error = Some(e),
                            ForwardMsg::Listening(_) | ForwardMsg::Repointed(_) => {}
                        });
                        hub.forwards_changed();
                    }
                    () = &mut expiry => break,
                }
            }
            hub.stop_forwards(|f| f.id == id);
        });
        forwards.set_watcher(id, watcher);
        self.hub.forwards_changed();
        call.finish(Ok(format!("Forwarding {text}")))
    }

    #[tool(
        description = "The port-forwards this agent has open (from port_forward).",
        annotations(read_only_hint = true)
    )]
    async fn list_port_forwards(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(_): Parameters<NoParams>,
    ) -> Result<String, String> {
        let call = self.begin(&parts, "list_port_forwards", json!({}))?;
        let mine: Vec<String> =
            self.hub.forwards().iter().filter(|f| f.agent_id == call.agent.id).map(forward_line).collect();
        call.finish(Ok(if mine.is_empty() { "No port-forwards open.".into() } else { mine.join("\n") }))
    }

    #[tool(description = "Stop one of this agent's port-forwards.")]
    async fn stop_port_forward(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<StopForwardParams>,
    ) -> Result<String, String> {
        let call = self.begin(&parts, "stop_port_forward", json!({"id": p.id}))?;
        let agent = call.agent.id.clone();
        let stopped = self.hub.stop_forwards(|f| f.id == p.id && f.agent_id == agent);
        call.finish(if stopped > 0 {
            Ok(format!("Stopped port-forward {}", p.id))
        } else {
            Err(anyhow::anyhow!("no port-forward {} of this agent", p.id))
        })
    }
}

fn forward_line(f: &ForwardInfo) -> String {
    let mut line = format!("{} — {} connection(s)", f.describe(), f.connections);
    if let Some(e) = &f.error {
        line.push_str(&format!(", last error: {e}"));
    }
    line
}
