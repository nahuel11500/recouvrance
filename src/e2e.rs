//! The write guards against a real cluster: write mode, the agents' rules and the kill
//! switch, end to end through the real clients and the MCP server. These are the guarantees
//! people rely on most, and unit tests stop short of the API server.
//!
//! Ignored unless asked for: CI's kind job runs them with
//! `RECOUVRANCE_E2E_CONTEXT=kind-recouvrance cargo test --features dev -- --ignored --test-threads 1 e2e::`
//! and a throwaway `XDG_CONFIG_HOME` / `XDG_DATA_HOME` (they create agents and change write mode).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result, bail};
use k8s_openapi::api::core::v1::{ConfigMap, Node};
use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use serde_json::json;

use crate::k8s::{self, gate};
use crate::mcp::{self, Mode, Verb, rules_file};

const NS: &str = "default";

fn context() -> String {
    std::env::var("RECOUVRANCE_E2E_CONTEXT").expect("RECOUVRANCE_E2E_CONTEXT names the kind context to test against")
}

/// Run a test body on the app's runtime, as the app does.
fn run(body: impl Future<Output = Result<()>> + Send + 'static) {
    futures::executor::block_on(k8s::run(body)).unwrap();
}

fn config_map(name: &str) -> ConfigMap {
    serde_json::from_value(json!({"metadata": {"name": name, "namespace": NS}, "data": {"a": "1"}})).unwrap()
}

/// An agent of a fresh hub with these rules (`allow`, then `deny` strings), its token, and
/// the hub's MCP server on a free port.
async fn agent(allow: &[&str], deny: &[&str]) -> Result<(Arc<mcp::Hub>, mcp::Agent, String, k8s::TaskGuard)> {
    let hub = mcp::Hub::load();
    hub.set_headless(true);
    hub.set_agents_enabled(true)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    hub.set_port(port)?;
    let (agent, token) = hub.create_agent(format!("e2e-{}", uuid::Uuid::new_v4().simple()))?;
    let rules: Vec<mcp::Rule> = allow
        .iter()
        .map(|r| rules_file::parse_rule(Mode::Allow, r))
        .chain(deny.iter().map(|r| rules_file::parse_rule(Mode::Deny, r)))
        .collect::<Result<_>>()?;
    hub.update_agent(&agent.id, |a| a.policy.rules = rules)?;
    let server = mcp::server::start(hub.clone());
    for _ in 0..50 {
        if let Some(status) = hub.server_status() {
            status.map_err(anyhow::Error::msg)?;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let agent = hub.agent(&agent.id).context("agent just created")?;
    Ok((hub, agent, token, server))
}

/// With write mode off, the UI's client refuses a write when it is sent, and nothing
/// reaches the cluster; dry runs and reads still go through. On, the same write lands.
#[test]
#[ignore]
fn write_mode_off_refuses_the_ui_s_writes() {
    run(async {
        let ui: Api<ConfigMap> = Api::namespaced(k8s::connect(context()).await?, NS);
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(context()).await?, NS);
        let name = "e2e-write-mode";
        let _ = truth.delete(name, &DeleteParams::default()).await;

        gate::set_ui_writes(false);
        let refused = ui.create(&PostParams::default(), &config_map(name)).await;
        let err = format!("{:#}", anyhow::Error::from(refused.expect_err("a write while write mode is off")));
        assert!(err.contains(gate::WRITES_OFF), "{err}");
        assert!(truth.get_opt(name).await?.is_none(), "the refused write reached the cluster");
        // A dry run and reads don't need write mode.
        ui.create(&PostParams { dry_run: true, ..Default::default() }, &config_map(name)).await?;
        ui.list(&Default::default()).await?;

        gate::set_ui_writes(true);
        ui.create(&PostParams::default(), &config_map(name)).await?;
        gate::set_ui_writes(false);
        // Turned off again: the delete is refused too, and the object stays.
        assert!(ui.delete(name, &DeleteParams::default()).await.is_err());
        assert!(truth.get_opt(name).await?.is_some());
        truth.delete(name, &DeleteParams::default()).await?;
        Ok(())
    });
}

/// An agent's rules decide on the real cluster through the MCP server: a delete its rules
/// don't allow is refused and the object stays; the kill switch refuses even reads; with a
/// delete rule, the same call deletes (so the refusals above are not a broken setup).
#[test]
#[ignore]
fn an_agent_gets_only_what_its_rules_allow() {
    run(async {
        let ctx = context();
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-agent-rules";
        let _ = truth.delete(name, &DeleteParams::default()).await;
        truth.create(&PostParams::default(), &config_map(name)).await?;

        let (hub, agent, token, _server) = agent(&["get,list *"], &["* secrets"]).await?;
        let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
        let delete = json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": name});
        let get = delete.clone();

        let out = client.call("delete_resource", delete.clone()).await;
        assert!(out.contains("Denied"), "{out}");
        assert!(truth.get_opt(name).await?.is_some(), "a denied delete reached the cluster");
        let out = client
            .call("get_resource", json!({"context": ctx, "kind": "secrets", "namespace": "kube-system", "name": "x"}))
            .await;
        assert!(out.contains("Denied"), "{out}");

        hub.set_agents_enabled(false)?;
        let out = client.call("get_resource", get.clone()).await;
        assert!(out.contains("switched off"), "{out}");
        hub.set_agents_enabled(true)?;
        let out = client.call("get_resource", get).await;
        assert!(out.contains(name), "{out}");

        let allow = rules_file::parse_rule(Mode::Allow, "delete configmaps/e2e-* -n default")?;
        hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
        let out = client.call("delete_resource", delete).await;
        assert!(!out.contains("Denied"), "{out}");
        for _ in 0..50 {
            if truth.get_opt(name).await?.is_none() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        bail!("the allowed delete did not happen: {out}")
    });
}

/// Turning agent access off while an agent's drain runs stops it before the next
/// eviction; so does turning write mode off during a drain started in the app.
#[test]
#[ignore]
fn turning_access_off_stops_a_drain() {
    run(async {
        let ctx = context();
        let agents_client = k8s::connect_for_agents(ctx.clone()).await?;
        let nodes: Api<Node> = Api::all(agents_client.clone());
        // The node with the most pods a drain evicts (a controller's, not a DaemonSet's).
        let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::all(agents_client.clone());
        let mut evictable: std::collections::HashMap<String, usize> = Default::default();
        for p in pods.list(&Default::default()).await?.items {
            let owners = p.metadata.owner_references.unwrap_or_default();
            let managed = owners.iter().any(|o| o.controller == Some(true) && o.kind != "DaemonSet");
            if let Some(n) = p.spec.and_then(|s| s.node_name).filter(|_| managed) {
                *evictable.entry(n).or_default() += 1;
            }
        }
        let (node, count) = evictable.into_iter().max_by_key(|(_, c)| *c).context("no pod to evict")?;
        assert!(count >= 2, "the drain test needs two pods to evict on one node, {node} has {count}");
        let uncordon = || async {
            let patch = Patch::Merge(json!({"spec": {"unschedulable": false}}));
            nodes.patch(&node, &PatchParams::default(), &patch).await.map(|_| ())
        };

        let (hub, agent, _token, _server) = agent(&["* *"], &[]).await?;
        let kinds = k8s::discover(agents_client.clone()).await?;
        let node_kind = k8s::find_kind(&kinds, "nodes").context("nodes kind")?.clone();
        let access = mcp::server::access_for(&node_kind, &ctx, Verb::Drain, None, Some(node.clone()));
        let check = mcp::server::stop_check(hub.clone(), agent.id.clone(), false, access);
        let asked = Arc::new(AtomicUsize::new(0));
        let (hub2, asked2) = (hub.clone(), asked.clone());
        // Agent access goes off right after the first eviction.
        let stop = move || {
            if asked2.fetch_add(1, Ordering::SeqCst) == 1 {
                let _ = hub2.set_agents_enabled(false);
            }
            check()
        };
        let drained = k8s::drain(agents_client, node.clone(), stop).await;
        hub.set_agents_enabled(true)?;
        uncordon().await?;
        let err = format!("{:#}", drained.expect_err("the drain went on after access was switched off"));
        assert!(err.contains("stopped after 1 evictions") && err.contains("switched off"), "{err}");

        // In the app: write mode off stops it at once.
        gate::set_ui_writes(true);
        let ui = k8s::connect(ctx).await?;
        let asked = Arc::new(AtomicUsize::new(0));
        let stop = move || {
            if asked.fetch_add(1, Ordering::SeqCst) == 0 {
                gate::set_ui_writes(false);
            }
            (!gate::ui_writes()).then(|| gate::WRITES_OFF.to_string())
        };
        let drained = k8s::drain(ui, node.clone(), stop).await;
        uncordon().await?;
        let err = format!("{:#}", drained.expect_err("the drain went on after write mode was turned off"));
        assert!(err.contains("stopped after 0 evictions"), "{err}");
        Ok(())
    });
}
