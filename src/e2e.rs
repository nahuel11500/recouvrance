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

/// A fresh busybox pod `name` in the test namespace running `script`, once it runs.
async fn busybox_pod(pods: &Api<k8s_openapi::api::core::v1::Pod>, name: &str, script: &str) -> Result<()> {
    let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
    for _ in 0..60 {
        if pods.get_opt(name).await?.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let pod = serde_json::from_value(json!({
        "metadata": {"name": name, "namespace": NS},
        "spec": {"containers": [{"name": "main", "image": "busybox:1.37", "command": ["sh", "-c", script]}]},
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    for _ in 0..120 {
        let phase = pods.get(name).await?.status.and_then(|s| s.phase);
        if phase.as_deref() == Some("Running") {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    bail!("{name} did not start")
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

/// A write confirmed to run as another identity is sent as it: here a user with no rights,
/// so the server refuses what the app's own user may do. Reads still go as the user. Write
/// mode off, or the identity changed in the settings since, refuses it before it is sent.
/// An agent allowed everything never runs as it, whatever the settings say.
#[test]
#[ignore]
fn a_write_runs_as_the_identity_chosen_for_it_only() {
    run(async {
        let ctx = context();
        let ui = k8s::connect(ctx.clone()).await?;
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-run-as";
        let _ = truth.delete(name, &DeleteParams::default()).await;
        let nobody = k8s::Identity { user: "e2e-nobody".into(), groups: Vec::new() };
        crate::settings::set_run_as(&ctx, Some(nobody.clone()));
        gate::set_ui_writes(true);
        let elevated: Api<ConfigMap> = Api::namespaced(k8s::run_as(&ui, &ctx, nobody.clone()), NS);
        let error = |e: kube::Error| format!("{:#}", anyhow::Error::from(e));

        let refused = elevated.create(&PostParams::default(), &config_map(name)).await;
        let err = error(refused.expect_err("e2e-nobody may not create ConfigMaps"));
        assert!(err.contains("e2e-nobody") && err.contains("forbidden"), "{err}");
        assert!(truth.get_opt(name).await?.is_none(), "the refused write reached the cluster");
        // e2e-nobody may not list either: this read went as the user.
        elevated.list(&Default::default()).await?;
        // As the user, the same write lands.
        Api::<ConfigMap>::namespaced(ui.clone(), NS).create(&PostParams::default(), &config_map(name)).await?;

        gate::set_ui_writes(false);
        let err = error(elevated.delete(name, &DeleteParams::default()).await.expect_err("write mode is off"));
        assert!(err.contains(k8s::RUN_AS_WRITES_OFF), "{err}");
        gate::set_ui_writes(true);
        crate::settings::set_run_as(&ctx, Some(k8s::Identity { user: "e2e-other".into(), groups: Vec::new() }));
        let err = error(elevated.delete(name, &DeleteParams::default()).await.expect_err("the identity changed"));
        assert!(err.contains(k8s::RUN_AS_CHANGED), "{err}");
        assert!(truth.get_opt(name).await?.is_some());

        // An agent allowed everything deletes as the user: had it run as e2e-nobody, the
        // server would have refused.
        crate::settings::set_run_as(&ctx, Some(nobody));
        let (hub, _agent, token, _server) = agent(&["* *"], &[]).await?;
        let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
        let out = client
            .call("delete_resource", json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": name}))
            .await;
        crate::settings::set_run_as(&ctx, None);
        gate::set_ui_writes(false);
        assert!(!out.contains("Denied") && !out.contains("e2e-nobody"), "{out}");
        for _ in 0..50 {
            if truth.get_opt(name).await?.is_none() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        bail!("the agent's delete did not happen: {out}")
    });
}

/// A shell in a container answers what is typed, and says when it ends; an attach shows
/// the container's own output. (The terminal view only draws what these sessions send.)
#[test]
#[ignore]
fn shells_and_attach_reach_the_container() {
    use futures::StreamExt as _;
    use k8s_openapi::api::core::v1::Pod;

    use crate::k8s::{ExecInput, ExecOutput, Session};

    /// What a session prints until `until` shows up (or it ends, or 60 s pass).
    async fn read_until(session: &mut Session, until: &str) -> String {
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        while !text.contains(until) {
            match tokio::time::timeout_at(deadline, session.output.next()).await {
                Ok(Some(ExecOutput::Data(b))) => text.push_str(&String::from_utf8_lossy(&b)),
                Ok(Some(ExecOutput::Exit(reason))) => {
                    text.push_str(&format!("[exit: {reason}]"));
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }
        text
    }

    run(async {
        let client = k8s::connect_for_agents(context()).await?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
        let name = "e2e-shell";
        let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        for _ in 0..60 {
            if pods.get_opt(name).await?.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let pod: Pod = serde_json::from_value(json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"containers": [{
                "name": "main", "image": "busybox:1.37",
                "command": ["sh", "-c", "echo attached-output; while true; do sleep 1; done"],
            }]},
        }))?;
        pods.create(&PostParams::default(), &pod).await?;
        let running = async {
            for _ in 0..120 {
                let phase = pods.get(name).await?.status.and_then(|s| s.phase);
                if phase.as_deref() == Some("Running") {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            bail!("{name} did not start")
        };
        let checked = async {
            running.await?;
            let mut shell = k8s::exec_shell(client.clone(), NS.into(), name.into(), None);
            let _ = shell.input.unbounded_send(ExecInput::Resize { cols: 80, rows: 24 });
            let _ = shell.input.unbounded_send(ExecInput::Bytes(b"echo $((20+22))-answered\n".to_vec()));
            let text = read_until(&mut shell, "42-answered").await;
            assert!(text.contains("42-answered"), "{text}");
            let _ = shell.input.unbounded_send(ExecInput::Bytes(b"exit\n".to_vec()));
            let text = read_until(&mut shell, "[exit:").await;
            assert!(text.contains("[exit: Session ended]"), "{text}");

            let mut attached = k8s::attach_container(client.clone(), NS.into(), name.into(), None);
            let text = read_until(&mut attached, "output only").await;
            assert!(text.contains("Attached to the container's main process"), "{text}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        pods.delete(name, &DeleteParams::default().grace_period(0)).await?;
        result
    });
}

/// An agent rolls a Deployment back only with a patch rule; the template it goes back to
/// is the previous revision's, and the rollout status says when it is done.
#[test]
#[ignore]
fn an_agent_rolls_back_a_deployment() {
    use k8s_openapi::api::apps::v1::Deployment;

    run(async {
        let ctx = context();
        let deps: Api<Deployment> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-rollout";
        let _ = deps.delete(name, &DeleteParams::default()).await;
        for _ in 0..60 {
            if deps.get_opt(name).await?.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let dep = |cmd: &str| {
            json!({
                "metadata": {"name": name, "namespace": NS},
                "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}}, "template": {
                    "metadata": {"labels": {"app": name}},
                    "spec": {"terminationGracePeriodSeconds": 0, "containers": [{"name": "main", "image": "busybox:1.37", "command": ["sh", "-c", cmd]}]},
                }},
            })
        };
        deps.create(&PostParams::default(), &serde_json::from_value(dep("sleep 3600"))?).await?;
        let command = |d: &Deployment| {
            d.spec.as_ref().and_then(|s| s.template.spec.as_ref()).map(|s| s.containers[0].command.clone())
        };
        let checked = async {
            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let target = json!({"context": ctx, "kind": "deployments", "namespace": NS, "name": name});
            // Wait for the first rollout, then change the template: revision 2.
            let mut status = target.clone();
            status["wait_seconds"] = json!(120);
            let out = client.call("rollout_status", status.clone()).await;
            assert!(out.contains("rolled out"), "{out}");
            deps.patch(name, &PatchParams::default(), &Patch::Merge(dep("sleep 7200"))).await?;
            let out = client.call("rollout_status", status.clone()).await;
            assert!(out.contains("rolled out"), "{out}");
            let out = client.call("rollout_history", target.clone()).await;
            assert!(out.contains("revision 2 (current)") && out.contains("revision 1"), "{out}");
            // The same revisions as changes, the template's diff with them.
            let out = client.call("get_changes", target.clone()).await;
            assert!(
                out.contains("(revision 2): changed spec.containers[main].command")
                    && out.lines().any(|l| l.trim_start().starts_with('+') && l.contains("sleep 7200"))
                    && !out.contains("pod-template-hash"),
                "{out}"
            );

            let out = client.call("rollout_undo", target.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            let allow = rules_file::parse_rule(Mode::Allow, "patch deployments/e2e-* -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let out = client.call("rollout_undo", target).await;
            assert!(out.contains("to revision 1"), "{out}");
            let back = command(&deps.get(name).await?);
            assert_eq!(back, Some(Some(vec!["sh".into(), "-c".into(), "sleep 3600".into()])));
            let out = client.call("rollout_status", status).await;
            assert!(out.contains("rolled out"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        deps.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// Running a CronJob now needs apply on jobs; a debug container needs exec on the pod, and
/// the agent then runs commands in it.
#[test]
#[ignore]
fn an_agent_runs_a_cronjob_and_debugs_a_pod() {
    use k8s_openapi::api::batch::v1::{CronJob, Job};
    use k8s_openapi::api::core::v1::Pod;

    run(async {
        let ctx = context();
        let client = k8s::connect_for_agents(ctx.clone()).await?;
        let (cronjobs, jobs): (Api<CronJob>, Api<Job>) =
            (Api::namespaced(client.clone(), NS), Api::namespaced(client.clone(), NS));
        let pods: Api<Pod> = Api::namespaced(client, NS);
        let (cron, pod) = ("e2e-cron", "e2e-debug");
        let _ = cronjobs.delete(cron, &DeleteParams::default()).await;
        let cj = json!({
            "metadata": {"name": cron, "namespace": NS},
            "spec": {"schedule": "0 0 1 1 *", "jobTemplate": {"spec": {"template": {"spec": {
                "restartPolicy": "Never",
                "containers": [{"name": "main", "image": "busybox:1.37", "command": ["echo", "ran"]}],
            }}}}},
        });
        cronjobs.create(&PostParams::default(), &serde_json::from_value(cj)?).await?;
        let checked = async {
            busybox_pod(&pods, pod, "while true; do sleep 1; done").await?;
            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let trigger = json!({"context": ctx, "namespace": NS, "name": cron});
            let out = mcp.call("trigger_cronjob", trigger.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            let debug = json!({"context": ctx, "namespace": NS, "pod": pod, "target": "main"});
            let out = mcp.call("debug_pod", debug.clone()).await;
            assert!(out.contains("Denied"), "{out}");

            for rule in ["apply jobs/e2e-cron-manual-* -n default", "exec pods/e2e-* -n default"] {
                let allow = rules_file::parse_rule(Mode::Allow, rule)?;
                hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            }
            let out = mcp.call("trigger_cronjob", trigger).await;
            assert!(out.contains("created from CronJob"), "{out}");
            let made = jobs.list(&Default::default()).await?.items.into_iter().filter_map(|j| j.metadata.name);
            assert!(made.into_iter().any(|n| n.starts_with("e2e-cron-manual-")), "no manual Job");

            let out = mcp.call("debug_pod", debug).await;
            let name = out.split_whitespace().nth(2).map(String::from).with_context(|| out.clone())?;
            assert!(name.starts_with("debugger-"), "{out}");
            let exec = json!({"context": ctx, "namespace": NS, "pod": pod, "container": name, "command": ["echo", "from-debug"]});
            let out = mcp.call("exec_command", exec).await;
            assert!(out.contains("from-debug"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        let _ = pods.delete(pod, &DeleteParams::default().grace_period(0)).await;
        cronjobs.delete(cron, &DeleteParams::foreground()).await?;
        result
    });
}

/// An agent's port-forward needs a rule for it, reaches the pod, and closes when agent
/// access is switched off.
#[test]
#[ignore]
fn an_agent_s_port_forward_follows_its_rules() {
    use k8s_openapi::api::core::v1::Pod;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// What `GET /` on the local port answers.
    async fn fetch(port: u16) -> Result<String> {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        socket.write_all(b"GET / HTTP/1.0\r\n\r\n").await?;
        let mut text = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(20), socket.read_to_string(&mut text)).await??;
        Ok(text)
    }

    run(async {
        let ctx = context();
        let pods: Api<Pod> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-forward";
        let checked = async {
            busybox_pod(
                &pods,
                name,
                "mkdir -p /www && echo forwarded-ok > /www/index.html && httpd -f -p 8080 -h /www",
            )
            .await?;
            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let args = json!({"context": ctx, "namespace": NS, "pod": name, "port": 8080});
            let out = client.call("port_forward", args.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            assert!(hub.forwards().is_empty());

            let allow = rules_file::parse_rule(Mode::Allow, "forward pods/e2e-* -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let out = client.call("port_forward", args).await;
            let port = hub.forwards().first().map(|f| f.local).with_context(|| format!("no forward: {out}"))?;
            let page = fetch(port).await?;
            assert!(page.contains("forwarded-ok"), "{page}");

            hub.set_agents_enabled(false)?;
            assert!(hub.forwards().is_empty(), "the kill switch stops agents' forwards");
            assert!(fetch(port).await.is_err(), "the port still answers after the kill switch");
            Ok(())
        };
        let result: Result<()> = checked.await;
        pods.delete(name, &DeleteParams::default().grace_period(0)).await?;
        result
    });
}

/// Cordoning and uncordoning a node need the agent's drain rule; `top_nodes` reads usage.
#[test]
#[ignore]
fn an_agent_cordons_a_node_with_its_drain_rule() {
    run(async {
        let ctx = context();
        let nodes: Api<Node> = Api::all(k8s::connect_for_agents(ctx.clone()).await?);
        let node = nodes.list(&Default::default()).await?.items.into_iter().rev().find_map(|n| n.metadata.name);
        let node = node.context("no node")?;
        let cordoned = || async {
            let n = nodes.get(&node).await?;
            anyhow::Ok(n.spec.and_then(|s| s.unschedulable).unwrap_or(false))
        };
        let checked = async {
            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let args = json!({"context": ctx, "node": node});
            let out = client.call("cordon_node", args.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            assert!(!cordoned().await?);

            let allow = rules_file::parse_rule(Mode::Allow, &format!("drain nodes/{node}"))?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let out = client.call("cordon_node", args.clone()).await;
            assert!(out.contains("cordoned"), "{out}");
            assert!(cordoned().await?);
            let out = client.call("uncordon_node", args).await;
            assert!(out.contains("uncordoned"), "{out}");
            assert!(!cordoned().await?);

            let out = client.call("top_nodes", json!({"context": ctx})).await;
            assert!(out.contains(&node) || out.contains("metrics"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        let patch = Patch::Merge(json!({"spec": {"unschedulable": false}}));
        nodes.patch(&node, &PatchParams::default(), &patch).await?;
        result
    });
}

/// Helm releases are read from their Secrets: denied by a Deny on secrets, listed with the
/// chart otherwise, and their secret-looking values masked unless the agent may reveal.
#[test]
#[ignore]
fn an_agent_reads_helm_releases_by_its_secret_rules() {
    use k8s_openapi::api::core::v1::Secret;

    run(async {
        let ctx = context();
        let secrets: Api<Secret> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let (release, name) = ("e2e-chart", "sh.helm.release.v1.e2e-chart.v1");
        let _ = secrets.delete(name, &DeleteParams::default()).await;
        let mut payload = crate::helm::tests::payload(release, NS, 1);
        payload["config"] = json!({"db": {"password": "hunter2", "host": "db.local"}});
        secrets
            .create(&PostParams::default(), &crate::helm::tests::helm_secret((name, NS, release), &payload, true))
            .await?;
        let checked = async {
            let (hub, agent, token, _server) = agent(&["get,list *"], &["* secrets"]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let list = json!({"context": ctx, "namespace": NS});
            let out = mcp.call("list_helm_releases", list.clone()).await;
            assert!(out.contains("Denied"), "{out}");

            hub.update_agent(&agent.id, |a| a.policy.rules.retain(|r| r.effect != Mode::Deny))?;
            let out = mcp.call("list_helm_releases", list).await;
            assert!(out.contains(release) && out.contains("c-1.0.0"), "{out}");
            let out = mcp.call("helm_history", json!({"context": ctx, "namespace": NS, "name": release})).await;
            assert!(out.contains("deployed"), "{out}");
            let values = json!({"context": ctx, "namespace": NS, "name": release});
            let out = mcp.call("helm_get_values", values.clone()).await;
            assert!(out.contains("db.local") && !out.contains("hunter2"), "{out}");
            let reveal = rules_file::parse_rule(Mode::Allow, "reveal secrets -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(reveal))?;
            let out = mcp.call("helm_get_values", values).await;
            assert!(out.contains("hunter2"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        secrets.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// A Deployment's logs come from all its pods, merged; pods whose logs the rules deny are
/// left out and counted.
#[test]
#[ignore]
fn an_agent_reads_a_workload_s_logs() {
    use k8s_openapi::api::apps::v1::Deployment;
    use k8s_openapi::api::core::v1::Pod;

    run(async {
        let ctx = context();
        let client = k8s::connect_for_agents(ctx.clone()).await?;
        let deps: Api<Deployment> = Api::namespaced(client.clone(), NS);
        let pods: Api<Pod> = Api::namespaced(client, NS);
        let name = "e2e-logs";
        let _ = deps.delete(name, &DeleteParams::default()).await;
        let dep = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {"terminationGracePeriodSeconds": 0, "containers": [{"name": "main", "image": "busybox:1.37",
                    "command": ["sh", "-c", "while true; do echo tick-$HOSTNAME; sleep 1; done"]}]},
            }},
        });
        deps.create(&PostParams::default(), &serde_json::from_value(dep)?).await?;
        let checked = async {
            let lp = kube::api::ListParams::default().labels(&format!("app={name}"));
            for _ in 0..120 {
                let running = pods
                    .list(&lp)
                    .await?
                    .items
                    .iter()
                    .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
                    .count();
                if running == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let (hub, agent, token, _server) = agent(&["get,list,logs *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let args = json!({"context": ctx, "kind": "deployments", "namespace": NS, "name": name, "grep": "tick"});
            let out = mcp.call("workload_logs", args.clone()).await;
            let prefixes: std::collections::HashSet<&str> = out.lines().filter_map(|l| l.split(']').next()).collect();
            assert_eq!(prefixes.len(), 2, "{out}");

            let deny = rules_file::parse_rule(Mode::Deny, "logs pods/e2e-logs-* -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(deny))?;
            let out = mcp.call("workload_logs", args).await;
            assert!(out.contains("2 of 2 pods left out"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        deps.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// Pausing, resuming and setting an image need the agent's patch rule; the image lands on
/// the container named, and a paused Deployment says so.
#[test]
#[ignore]
fn an_agent_pauses_and_sets_an_image() {
    use k8s_openapi::api::apps::v1::Deployment;

    run(async {
        let ctx = context();
        let deps: Api<Deployment> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-image";
        let _ = deps.delete(name, &DeleteParams::default()).await;
        for _ in 0..60 {
            if deps.get_opt(name).await?.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let dep = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}}, "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {"terminationGracePeriodSeconds": 0, "containers": [{"name": "main", "image": "busybox:1.37", "command": ["sleep", "3600"]}]},
            }},
        });
        deps.create(&PostParams::default(), &serde_json::from_value(dep)?).await?;
        let checked = async {
            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let target = json!({"context": ctx, "namespace": NS, "name": name});
            let out = mcp.call("rollout_pause", target.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            let allow = rules_file::parse_rule(Mode::Allow, "patch deployments/e2e-* -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let out = mcp.call("rollout_pause", target.clone()).await;
            assert!(out.contains("paused"), "{out}");
            let paused = deps.get(name).await?.spec.and_then(|s| s.paused);
            assert_eq!(paused, Some(true));
            let mut image = target.clone();
            image["kind"] = json!("deployments");
            image["container"] = json!("main");
            image["image"] = json!("busybox:1.36");
            let out = mcp.call("set_image", image.clone()).await;
            assert!(out.contains("main → busybox:1.36"), "{out}");
            let now = deps.get(name).await?.spec.and_then(|s| s.template.spec).map(|s| s.containers[0].image.clone());
            assert_eq!(now, Some(Some("busybox:1.36".into())));
            image["container"] = json!("nope");
            let out = mcp.call("set_image", image).await;
            assert!(out.contains("no container"), "{out}");
            let out = mcp.call("rollout_resume", target).await;
            assert!(out.contains("resumed"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        deps.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// `diff_manifest` shows what an apply would change, with get rules only, and changes nothing.
#[test]
#[ignore]
fn an_agent_diffs_a_manifest_without_changing_anything() {
    run(async {
        let ctx = context();
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let (name, new) = ("e2e-diff", "e2e-diff-new");
        for n in [name, new] {
            let _ = truth.delete(n, &DeleteParams::default()).await;
        }
        truth.create(&PostParams::default(), &config_map(name)).await?;
        let checked = async {
            let (hub, _agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let manifest = format!(
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {name}\n  namespace: {NS}\ndata:\n  a: \"2\"\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {new}\n  namespace: {NS}\ndata:\n  b: \"1\"\n"
            );
            let out = mcp.call("diff_manifest", json!({"context": ctx, "manifest": manifest})).await;
            assert!(out.contains("-  a: '1'") || out.contains("-  a: \"1\""), "{out}");
            assert!(out.contains(&format!("ConfigMap {NS}/{new}: created")), "{out}");
            assert!(truth.get_opt(new).await?.is_none(), "a diff created an object");
            let a = truth.get(name).await?.data.and_then(|d| d.get("a").cloned());
            assert_eq!(a.as_deref(), Some("1"), "a diff changed an object");
            Ok(())
        };
        let result: Result<()> = checked.await;
        truth.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// The read-only questions about the cluster: explain, can-i, version and wait.
#[test]
#[ignore]
fn an_agent_asks_the_cluster() {
    run(async {
        let ctx = context();
        let (hub, _agent, token, _server) = agent(&["get,list *"], &[]).await?;
        let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
        let out = mcp
            .call("explain_resource", json!({"context": ctx, "kind": "deployments", "field": "spec.replicas"}))
            .await;
        assert!(out.contains("FIELD: spec.replicas <integer>"), "{out}");
        let out = mcp.call("can_i", json!({"context": ctx, "verb": "get", "kind": "pods", "namespace": NS})).await;
        assert!(out.starts_with("yes") || out.contains("yes"), "{out}");
        let out = mcp.call("cluster_version", json!({"context": ctx})).await;
        assert!(out.contains("v1."), "{out}");
        let nodes: Api<Node> = Api::all(k8s::connect_for_agents(ctx.clone()).await?);
        let node = nodes.list(&Default::default()).await?.items.into_iter().find_map(|n| n.metadata.name);
        let wait = json!({"context": ctx, "kind": "nodes", "name": node, "condition": "Ready", "timeout_seconds": 30});
        let out = mcp.call("wait_for", wait).await;
        assert!(out.contains(": Ready"), "{out}");
        Ok(())
    });
}
