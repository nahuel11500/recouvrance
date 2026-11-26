//! The write guards against a real cluster: write mode, the agents' rules and the kill
//! switch, end to end through the real clients and the MCP server. These are the guarantees
//! people rely on most, and unit tests stop short of the API server. So are the watches that
//! keep pages current, whose selectors and bookmarks only a real API server answers.
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

use crate::k8s::{self, gate, meter};
use crate::mcp::{self, Mode, Verb, rules_file};

const NS: &str = "default";

fn context() -> String {
    std::env::var("RECOUVRANCE_E2E_CONTEXT").expect("RECOUVRANCE_E2E_CONTEXT names the kind context to test against")
}

/// Run a test body on the app's runtime, as the app does.
fn run(body: impl Future<Output = Result<()>> + Send + 'static) {
    futures::executor::block_on(k8s::run(body)).unwrap();
}

/// What Settings → API requests counts as received from `ctx` by requests of `class` so far.
fn received(ctx: &str, class: meter::Class) -> u64 {
    let read = meter::snapshot(meter::now_minute());
    read.classes.iter().filter(|(c, k, _)| c == ctx && *k == class).map(|(_, _, s)| s.bytes).sum()
}

fn config_map(name: &str) -> ConfigMap {
    serde_json::from_value(json!({"metadata": {"name": name, "namespace": NS}, "data": {"a": "1"}})).unwrap()
}

/// A fresh busybox pod `name` in the test namespace running `script`, once it runs.
async fn busybox_pod(pods: &Api<k8s_openapi::api::core::v1::Pod>, name: &str, script: &str) -> Result<()> {
    running_pod(pods, name, "busybox:1.37", script).await
}

/// A fresh pod `name` of `image` in the test namespace running `script`, once it runs.
async fn running_pod(pods: &Api<k8s_openapi::api::core::v1::Pod>, name: &str, image: &str, script: &str) -> Result<()> {
    let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
    for _ in 0..60 {
        if pods.get_opt(name).await?.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let pod = serde_json::from_value(json!({
        "metadata": {"name": name, "namespace": NS},
        "spec": {"containers": [{"name": "main", "image": image, "command": ["sh", "-c", script]}]},
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
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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

/// A context set read-only takes no write from the window even with write mode on (the
/// client, helm), and no write from an agent whose rules allow it (refused as final, and
/// `check_access` says so); reads and the window's dry runs still go through. Set back, the
/// same writes land, so the refusals are not a broken setup.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn a_read_only_context_refuses_every_write() {
    run(async {
        let ctx = context();
        let ui: Api<ConfigMap> = Api::namespaced(k8s::connect(ctx.clone()).await?, NS);
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-read-only";
        let _ = truth.delete(name, &DeleteParams::default()).await;
        truth.create(&PostParams::default(), &config_map(name)).await?;
        let (hub, _agent, token, _server) = agent(&["* *"], &[]).await?;
        let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
        let patch =
            json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": name, "patch": {"data": {"a": "2"}}});
        let checked = async {
            crate::settings::set_read_only(&ctx, true);
            gate::set_ui_writes(true);
            let refused = ui.delete(name, &DeleteParams::default()).await;
            let err = format!("{:#}", anyhow::Error::from(refused.expect_err("a write to a read-only context")));
            assert!(err.contains(&gate::read_only_refusal(&ctx)), "{err}");
            let helm = k8s::helm(ctx.clone(), vec!["uninstall".into(), "e2e-none".into()]).await;
            let err = format!("{:#}", helm.expect_err("helm uninstall on a read-only context"));
            assert!(err.contains(&gate::read_only_refusal(&ctx)), "{err}");
            ui.create(&PostParams { dry_run: true, ..Default::default() }, &config_map("e2e-read-only-dry")).await?;
            ui.get(name).await?;

            let out = client.call("patch_resource", patch.clone()).await;
            assert!(out.contains("Denied") && out.contains("read-only") && out.contains("Final"), "{out}");
            let planned =
                json!({"requests": [{"context": ctx, "verb": "delete", "kind": "cm", "namespace": NS, "name": name}]});
            let out = client.call("check_access", planned).await;
            assert!(out.contains("read-only") && out.contains("0 allowed"), "{out}");
            let out = client
                .call("get_resource", json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": name}))
                .await;
            assert!(out.contains(name) && !out.contains("Denied"), "{out}");
            let data = truth.get(name).await?.data.unwrap_or_default();
            assert_eq!(data.get("a").map(String::as_str), Some("1"), "a write reached a read-only context");

            crate::settings::set_read_only(&ctx, false);
            let out = client.call("patch_resource", patch.clone()).await;
            assert!(!out.contains("Denied"), "{out}");
            let data = truth.get(name).await?.data.unwrap_or_default();
            assert_eq!(data.get("a").map(String::as_str), Some("2"), "{out}");
            ui.delete(name, &DeleteParams::default()).await?;
            anyhow::Ok(())
        };
        let result = checked.await;
        crate::settings::set_read_only(&ctx, false);
        gate::set_ui_writes(false);
        let _ = truth.delete(name, &DeleteParams::default()).await;
        result
    });
}

/// A row of a big list in every namespace comes from its cells alone, without a uid: the
/// table's delete reads the object's and deletes only the object listed. One newer than
/// the row (re-created under its name since the list) stays; the one listed goes.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn a_row_without_its_uid_deletes_only_the_object_listed() {
    run(async {
        let ui = k8s::connect(context()).await?;
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(context()).await?, NS);
        let name = "e2e-delete-row";
        let _ = truth.delete(name, &DeleteParams::default()).await;
        truth.create(&PostParams::default(), &config_map(name)).await?;
        let kinds = k8s::discover(ui.clone()).await?;
        let kind = k8s::find_kind(&kinds, "configmaps").cloned().context("no configmaps")?;
        // As `cells_row` builds it: namespace/name for a uid, the creation time from the Age.
        let row = |listed: jiff::Timestamp| {
            Arc::new(k8s::Row {
                uid: format!("{NS}/{name}"),
                name: name.into(),
                namespace: Some(NS.into()),
                created: Some(listed),
                cells: vec![name.into()],
                labels: "".into(),
                deleting: false,
                deleted_at: None,
            })
        };
        gate::set_ui_writes(true);
        let checked = async {
            // Listed ten minutes ago: the object there now was made after, so it isn't that one.
            let ago = jiff::Timestamp::now() - jiff::SignedDuration::from_mins(10);
            let refused = k8s::delete_row(ui.clone(), kind.clone(), row(ago)).await;
            let err = format!("{:#}", refused.expect_err("a delete of an object newer than the row"));
            assert!(err.contains("re-created"), "{err}");
            assert!(truth.get_opt(name).await?.is_some(), "the newer object was deleted");
            // Listed now: it is the one, and it goes.
            k8s::delete_row(ui.clone(), kind.clone(), row(jiff::Timestamp::now())).await?;
            for _ in 0..50 {
                if truth.get_opt(name).await?.is_none() {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            bail!("the object listed was not deleted")
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        let _ = truth.delete(name, &DeleteParams::default()).await;
        result
    });
}

/// An agent's rules decide on the real cluster through the MCP server: a delete its rules
/// don't allow is refused and the object stays; the kill switch refuses even reads; with a
/// delete rule, the same call deletes (so the refusals above are not a broken setup). A
/// refusal says whether it is final, or the rule that would allow the call, and that rule,
/// added, does; `check_access` says the same without making the call.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
        let grant = format!("delete configmaps/{name} -n {NS} --context {ctx}");
        assert!(out.contains("Grantable") && out.contains(&format!("`{grant}` under allow")), "{out}");
        let out = client
            .call("get_resource", json!({"context": ctx, "kind": "secrets", "namespace": "kube-system", "name": "x"}))
            .await;
        assert!(out.contains("Denied") && out.contains("Final"), "{out}");
        let planned = json!({"requests": [
            {"context": ctx, "verb": "delete", "kind": "cm", "namespace": NS, "name": name},
            {"context": ctx, "verb": "get", "kind": "secrets", "namespace": "kube-system", "name": "x"},
            {"context": ctx, "verb": "list", "kind": "pods"},
            {"context": "e2e-no-such-context", "verb": "get", "kind": "pods"},
        ]});
        let out = client.call("check_access", planned.clone()).await;
        assert!(
            out.contains(&format!("1. delete configmaps {name} -n {NS} --context {ctx}: denied. Grantable")),
            "{out}"
        );
        assert!(out.contains("2. get secrets x -n kube-system") && out.contains("denied by rule #2"), "{out}");
        assert!(out.contains("3. list pods (all namespaces / cluster)") && out.contains("allowed by rule #1"), "{out}");
        assert!(out.contains("4. get pods --context e2e-no-such-context: could not check"), "{out}");
        assert!(out.contains("1 allowed, 0 needing approval, 2 denied."), "{out}");

        hub.set_agents_enabled(false)?;
        let out = client.call("get_resource", get.clone()).await;
        assert!(out.contains("switched off"), "{out}");
        hub.set_agents_enabled(true)?;
        let out = client.call("get_resource", get).await;
        assert!(out.contains(name), "{out}");

        // The rule the refusal named is enough.
        let allow = rules_file::parse_rule(Mode::Allow, &grant)?;
        hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
        let out = client.call("check_access", planned).await;
        assert!(out.contains(&format!("{name} -n {NS} --context {ctx}: allowed by rule #3")), "{out}");
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

/// An approval ends as soon as its agent stops waiting: hanging up (the assistant's Stop, a
/// killed `claude -p`) or cancelling the call (MCP's `notifications/cancelled`) takes the
/// banner down at once, and an Approve clicked after that writes nothing. An approval the
/// agent still waits for writes (so the refusals are not a broken setup).
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_approval_ends_when_its_agent_stops_waiting() {
    use futures::StreamExt as _;
    use std::time::Duration;

    /// The next approval event within `within`, as (asked, id): `false` when it ended.
    async fn next_approval(
        events: &mut futures::channel::mpsc::UnboundedReceiver<Arc<mcp::HubEvent>>,
        within: Duration,
    ) -> Result<(bool, u64)> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(Some(event)) => match &*event {
                    mcp::HubEvent::ApprovalRequested(a) => return Ok((true, a.id)),
                    mcp::HubEvent::ApprovalResolved(id) => return Ok((false, *id)),
                    _ => {}
                },
                Ok(None) => bail!("the hub stopped sending events"),
                Err(_) => bail!("no approval event within {within:?}"),
            }
        }
    }

    run(async {
        let ctx = context();
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-abandoned";
        let _ = truth.delete(name, &DeleteParams::default()).await;
        truth.create(&PostParams::default(), &config_map(name)).await?;

        let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
        let ask = rules_file::parse_rule(Mode::Ask, &format!("patch configmaps/{name} -n {NS}"))?;
        hub.update_agent(&agent.id, |a| a.policy.rules.push(ask))?;
        // A window to approve in.
        hub.set_headless(false);
        let mut events = hub.subscribe();
        let patch =
            json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": name, "patch": {"data": {"a": "2"}}});

        for cancel in [false, true] {
            let client = Arc::new(crate::assistant::Mcp::connect(hub.port(), token.clone()).await?);
            let call = tokio::spawn({
                let (client, patch) = (client.clone(), patch.clone());
                async move { client.call("patch_resource", patch).await }
            });
            // The first call discovers the cluster's kinds before it asks.
            let (asked, id) = next_approval(&mut events, Duration::from_secs(60)).await?;
            assert!(asked, "approval {id} ended before it was asked for");
            if cancel {
                // What an MCP client may send to cancel a call (`Mcp::call` numbers its call 3).
                let params = json!({"requestId": 3, "reason": "e2e"});
                client
                    .call_raw(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": params}))
                    .await?;
            } else {
                // The assistant's Stop: its task, with its connection, dropped.
                call.abort();
            }
            let ended = next_approval(&mut events, Duration::from_secs(1)).await?;
            assert_eq!(ended, (false, id), "cancel={cancel}");
            assert!(!hub.approval_pending(id));
            // An Approve clicked after that reaches nothing.
            hub.resolve_approval(id, true);
            call.abort();
            let _ = call.await;
        }
        // Time for a write that must not happen.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let data = truth.get(name).await?.data.unwrap_or_default();
        assert_eq!(data.get("a").map(String::as_str), Some("1"), "an approval its agent stopped waiting for wrote");
        let abandoned = hub.recent_audit(10, |e| e.agent_id == agent.id && e.detail == mcp::Answer::Abandoned.reason());
        assert_eq!(abandoned.len(), 2, "the log says why each call was refused");

        // An approval its agent still waits for goes through.
        let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
        let approve = async {
            let (asked, id) = next_approval(&mut events, Duration::from_secs(60)).await?;
            assert!(asked, "approval {id} ended before it was asked for");
            hub.resolve_approval(id, true);
            anyhow::Ok(())
        };
        let (out, approved) = tokio::join!(client.call("patch_resource", patch), approve);
        approved?;
        assert!(!out.contains("Rejected") && !out.contains("Denied"), "{out}");
        let data = truth.get(name).await?.data.unwrap_or_default();
        assert_eq!(data.get("a").map(String::as_str), Some("2"), "{out}");
        truth.delete(name, &DeleteParams::default()).await?;
        Ok(())
    });
}

/// An agent's patch that an Ask rule covers shows, before it is approved, what it will
/// change: the field with its value now and after, and the diff, from a dry run that changes
/// nothing. The activity log keeps it with the call.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_approval_shows_what_a_patch_will_change() {
    use futures::StreamExt as _;
    use std::time::Duration;

    run(async {
        let ctx = context();
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-preview";
        let _ = truth.delete(name, &DeleteParams::default()).await;
        truth.create(&PostParams::default(), &config_map(name)).await?;

        let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
        let ask = rules_file::parse_rule(Mode::Ask, &format!("patch configmaps/{name} -n {NS}"))?;
        hub.update_agent(&agent.id, |a| a.policy.rules.push(ask))?;
        // A window to approve in.
        hub.set_headless(false);
        let mut events = hub.subscribe();
        let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
        let patch =
            json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": name, "patch": {"data": {"a": "2"}}});
        let approve = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                let event = match tokio::time::timeout_at(deadline, events.next()).await {
                    Ok(Some(event)) => event,
                    Ok(None) => bail!("the hub stopped sending events"),
                    Err(_) => bail!("no approval asked for within 60 s"),
                };
                if let mcp::HubEvent::ApprovalRequested(a) = &*event {
                    // Asked for once the dry run is done: the object is still as it was.
                    let data = truth.get(name).await?.data.unwrap_or_default();
                    hub.resolve_approval(a.id, true);
                    return anyhow::Ok((a.preview.clone(), data));
                }
            }
        };
        let (out, approved) = tokio::join!(client.call("patch_resource", patch), approve);
        let (preview, before) = approved?;
        let preview = preview.context("the approval shows no preview")?;
        assert_eq!(before.get("a").map(String::as_str), Some("1"), "the dry run changed the object");
        let text = preview.text();
        assert!(text.contains(&format!("ConfigMap {NS}/{name}: changed")), "{text}");
        assert!(text.contains("data.a: 1 → 2"), "{text}");
        let added = preview
            .sections
            .iter()
            .flat_map(|s| &s.diff)
            .any(|l| matches!(l, crate::history::DiffLine::Added(t) if t.contains("a:") && t.contains('2')));
        assert!(added, "{text}");
        assert!(!out.contains("Rejected") && !out.contains("Denied"), "{out}");
        let data = truth.get(name).await?.data.unwrap_or_default();
        assert_eq!(data.get("a").map(String::as_str), Some("2"), "{out}");
        // The log keeps what the approval showed.
        let logged = hub.recent_audit(5, |e| e.agent_id == agent.id && e.tool == "patch_resource");
        let change = logged.first().and_then(|e| e.change.clone()).unwrap_or_default();
        assert!(change.contains("data.a: 1 → 2"), "{change}");
        truth.delete(name, &DeleteParams::default()).await?;
        Ok(())
    });
}

/// A YAML save from the window is sent as of the version the edit started from: a change
/// made on the cluster meanwhile makes the server refuse it (its dry run too, and with the
/// text's own resourceVersion removed), and that change stays. The edit moved onto the latest
/// version then saves, and both changes are kept.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn a_yaml_save_over_a_newer_object_is_refused() {
    use crate::k8s::edit;

    run(async {
        let ctx = context();
        let client = k8s::connect(ctx.clone()).await?;
        let truth: Api<ConfigMap> = Api::namespaced(k8s::connect_for_agents(ctx).await?, NS);
        let name = "e2e-yaml-save";
        let _ = truth.delete(name, &DeleteParams::default()).await;
        truth.create(&PostParams::default(), &config_map(name)).await?;
        let kinds = k8s::discover(client.clone()).await?;
        let kind = k8s::find_kind(&kinds, "configmaps").cloned().context("no configmaps")?;
        let obj =
            k8s::ObjectRef { kind: kind.clone(), name: name.into(), namespace: Some(NS.into()), uid: String::new() };
        let ns = Some(NS.to_string());

        // The editor's copy, edited, while someone else writes to the object.
        let base = k8s::get_object_opts(client.clone(), obj.clone(), true).await?;
        let version = edit::resource_version(&base).map(str::to_string);
        let mut edited = base.clone();
        edited["data"]["b"] = json!("edited");
        let mut bare = edited.clone();
        bare["metadata"].as_object_mut().context("metadata")?.remove("resourceVersion");
        truth.patch(name, &PatchParams::default(), &Patch::Merge(json!({"data": {"a": "meanwhile"}}))).await?;

        gate::set_ui_writes(true);
        let dry =
            edit::save_edit(client.clone(), kind.clone(), ns.clone(), edited.clone(), version.clone(), true).await;
        let saved =
            edit::save_edit(client.clone(), kind.clone(), ns.clone(), edited.clone(), version.clone(), false).await;
        let saved_bare = edit::save_edit(client.clone(), kind.clone(), ns.clone(), bare, version, false).await;
        gate::set_ui_writes(false);
        for refused in [&dry, &saved, &saved_bare] {
            assert!(refused.as_ref().is_err_and(edit::is_conflict), "not refused as stale: {refused:?}");
        }
        let data = truth.get(name).await?.data.unwrap_or_default();
        assert_eq!(data.get("a").map(String::as_str), Some("meanwhile"), "the change made meanwhile was undone");
        assert!(!data.contains_key("b"), "the stale edit was saved");

        let live = k8s::get_object_opts(client.clone(), obj, true).await?;
        let merged = edit::rebase(&base, &edited, &live);
        let version = edit::resource_version(&live).map(str::to_string);
        gate::set_ui_writes(true);
        let saved = edit::save_edit(client, kind, ns, merged, version, false).await;
        gate::set_ui_writes(false);
        saved?;
        let data = truth.get(name).await?.data.unwrap_or_default();
        assert_eq!(data.get("a").map(String::as_str), Some("meanwhile"));
        assert_eq!(data.get("b").map(String::as_str), Some("edited"));
        truth.delete(name, &DeleteParams::default()).await?;
        Ok(())
    });
}

/// Turning agent access off while an agent's drain runs stops it before the next
/// eviction; so does turning write mode off during a drain started in the app.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
        let check = mcp::server::stop_check(hub.clone(), agent.id.clone(), false, access, Default::default());
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
        assert!(err.contains("stopped after 1 eviction:") && err.contains("switched off"), "{err}");

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
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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

/// A shell in a container answers what is typed, says when it ends, and is hung up when
/// closed; an attach shows the container's own output. (The terminal view only draws what
/// these sessions send.)
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
        let ctx = context();
        let streamed = || received(&ctx, meter::Class::Stream);
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
            let before = streamed();
            let mut shell = k8s::exec_shell(client.clone(), NS.into(), name.into(), None);
            let _ = shell.input.unbounded_send(ExecInput::Resize { cols: 80, rows: 24 });
            let _ = shell.input.unbounded_send(ExecInput::Bytes(b"echo $((20+22))-answered\n".to_vec()));
            let text = read_until(&mut shell, "42-answered").await;
            assert!(text.contains("42-answered"), "{text}");
            // What the shell printed came over its upgraded connection, and counts as received.
            let counted = streamed().saturating_sub(before);
            assert!(counted >= text.len() as u64, "{counted} bytes counted for {} printed", text.len());
            let _ = shell.input.unbounded_send(ExecInput::Bytes(b"exit\n".to_vec()));
            let text = read_until(&mut shell, "[exit:").await;
            assert!(text.contains("[exit: Session ended]"), "{text}");

            let mut attached = k8s::attach_container(client.clone(), NS.into(), name.into(), None);
            let text = read_until(&mut attached, "output only").await;
            assert!(text.contains("Attached to the container's main process"), "{text}");
            drop(attached);

            // Closing a shell ends it, with what runs in it, even once write mode is off: the
            // runtime only detaches a tty session whose connection is dropped.
            gate::set_ui_writes(true);
            let mut shell = k8s::exec_shell(k8s::connect(context()).await?, NS.into(), name.into(), None);
            let _ = shell.input.unbounded_send(ExecInput::Resize { cols: 80, rows: 24 });
            let _ = shell.input.unbounded_send(ExecInput::Bytes(b"echo $((40+2))-sleeping; sleep 4321\n".to_vec()));
            let text = read_until(&mut shell, "42-sleeping").await;
            gate::set_ui_writes(false);
            assert!(text.contains("42-sleeping"), "{text}");
            drop(shell);
            // The shell (`sh`: busybox has no bash) and its `sleep`, as `ps` lists them.
            let left = |ps: &str| {
                ps.lines()
                    .filter(|l| {
                        let command: Vec<&str> = l.split_whitespace().skip(3).collect();
                        command == ["sh"] || command == ["sleep", "4321"]
                    })
                    .count()
            };
            let mut ps = String::new();
            for _ in 0..30 {
                ps = k8s::exec_capture(client.clone(), NS.into(), name.into(), None, vec!["ps".into()], 30, 1 << 16)
                    .await?;
                if left(&ps) == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            assert_eq!(left(&ps), 0, "the closed shell is still running:\n{ps}");

            // A command's output counts as it is read, whether it is kept or not.
            let before = streamed();
            let zeros = vec!["head".into(), "-c".into(), "300000".into(), "/dev/zero".into()];
            let out = k8s::exec_capture(client.clone(), NS.into(), name.into(), None, zeros, 30, 1024).await?;
            assert!(out.contains("[truncated]"), "{out}");
            let counted = streamed().saturating_sub(before);
            assert!(counted >= 300_000, "{counted} bytes counted for 300000 printed");
            Ok(())
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        pods.delete(name, &DeleteParams::default().grace_period(0)).await?;
        result
    });
}

/// An agent rolls a Deployment back only with a patch rule; the template it goes back to
/// is the previous revision's, and the rollout status says when it is done.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
                out.lines()
                    .any(|l| l.contains("Revision 2 · ReplicaSet ")
                        && l.contains(": changed spec.containers[main].command"))
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
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
            let before = received(&ctx, meter::Class::Stream);
            let page = fetch(port).await?;
            assert!(page.contains("forwarded-ok"), "{page}");
            // The page came back through the forward's tunnel, and counts as received.
            let counted = received(&ctx, meter::Class::Stream).saturating_sub(before);
            assert!(counted >= page.len() as u64, "{counted} bytes counted for {} forwarded", page.len());

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
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
/// chart otherwise, and their secret-looking values masked unless the agent may reveal. The
/// activity log names the client of each call, and the calls that showed Secret values.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
            // The activity log names the client, and says when values were shown: not yet.
            let last = |tool: &'static str| {
                let logged = hub.recent_audit(1, |e| e.agent_id == agent.id && e.tool == tool);
                logged.into_iter().next().context("the call is logged")
            };
            let masked = last("helm_get_values")?;
            assert_eq!(masked.client.as_deref(), Some(concat!("recouvrance-assistant ", env!("CARGO_PKG_VERSION"))));
            assert!(masked.revealed.is_empty(), "{:?}", masked.revealed);
            let get = json!({"context": ctx, "kind": "secrets", "namespace": NS, "name": name});
            let out = mcp.call("get_resource", get.clone()).await;
            assert!(out.contains(name), "{out}");
            assert!(last("get_resource")?.revealed.is_empty());

            let reveal = rules_file::parse_rule(Mode::Allow, "reveal secrets -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(reveal))?;
            let out = mcp.call("helm_get_values", values).await;
            assert!(out.contains("hunter2"), "{out}");
            assert_eq!(last("helm_get_values")?.revealed, [format!("Helm values {NS}/{release} revision 1")]);
            let out = mcp.call("get_resource", get).await;
            assert!(out.contains(name), "{out}");
            assert_eq!(last("get_resource")?.revealed, [format!("Secret {NS}/{name}")]);
            Ok(())
        };
        let result: Result<()> = checked.await;
        secrets.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// A Deployment's logs come from all its pods, merged; pods whose logs the rules deny are
/// left out and counted. With no container named, each pod's default one is read (the API
/// server refuses a pod with several), and the others are named.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
                    "command": ["sh", "-c", "while true; do echo tick-$HOSTNAME; sleep 1; done"]},
                    {"name": "helper", "image": "busybox:1.37", "command": ["sleep", "3600"]}]},
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
            let before = received(&ctx, meter::Class::Logs);
            let out = mcp.call("workload_logs", args.clone()).await;
            let ticks = out.lines().filter(|l| l.contains("tick-"));
            let prefixes: std::collections::HashSet<&str> = ticks.filter_map(|l| l.split(']').next()).collect();
            assert_eq!(prefixes.len(), 2, "{out}");
            // The logs read count as received, as their bodies are read.
            assert!(received(&ctx, meter::Class::Logs) > before, "no log bytes counted");
            assert!(out.contains("logs of container main (the default), not of helper"), "{out}");
            let pod = pods.list(&kube::api::ListParams::default().labels(&format!("app={name}"))).await?.items;
            let pod = pod.first().and_then(|p| p.metadata.name.clone()).unwrap_or_default();
            let one = mcp.call("get_logs", json!({"context": ctx, "namespace": NS, "pod": pod, "grep": "tick"})).await;
            assert!(one.contains(&format!("tick-{pod}")) && one.contains("not of helper"), "{one}");

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
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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
/// A type the sidebar leaves out (a `FlowSchema`, which kwok's manifest holds) is planned like
/// any other: `apply_manifest` resolves types the same way.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_agent_diffs_a_manifest_without_changing_anything() {
    use k8s_openapi::api::flowcontrol::v1::FlowSchema;

    run(async {
        let ctx = context();
        let client = k8s::connect_for_agents(ctx.clone()).await?;
        let truth: Api<ConfigMap> = Api::namespaced(client.clone(), NS);
        let flows: Api<FlowSchema> = Api::all(client);
        let (name, new, flow) = ("e2e-diff", "e2e-diff-new", "e2e-diff-flow");
        for n in [name, new] {
            let _ = truth.delete(n, &DeleteParams::default()).await;
        }
        let _ = flows.delete(flow, &DeleteParams::default()).await;
        truth.create(&PostParams::default(), &config_map(name)).await?;
        let checked = async {
            let (hub, _agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let manifest = format!(
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {name}\n  namespace: {NS}\ndata:\n  a: \"2\"\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {new}\n  namespace: {NS}\ndata:\n  b: \"1\"\n---\napiVersion: flowcontrol.apiserver.k8s.io/v1\nkind: FlowSchema\nmetadata:\n  name: {flow}\nspec:\n  priorityLevelConfiguration:\n    name: global-default\n"
            );
            let out = mcp.call("diff_manifest", json!({"context": ctx, "manifest": manifest})).await;
            assert!(out.contains("-  a: '1'") || out.contains("-  a: \"1\""), "{out}");
            assert!(out.contains(&format!("ConfigMap {NS}/{new}: created")), "{out}");
            assert!(out.contains(&format!("FlowSchema {flow}: created")), "{out}");
            assert!(truth.get_opt(new).await?.is_none(), "a diff created an object");
            assert!(flows.get_opt(flow).await?.is_none(), "a diff created a FlowSchema");
            let a = truth.get(name).await?.data.and_then(|d| d.get("a").cloned());
            assert_eq!(a.as_deref(), Some("1"), "a diff changed an object");
            Ok(())
        };
        let result: Result<()> = checked.await;
        truth.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// Custom resources applied right after their CRD (#839): in the same manifest, even listed
/// before it, and in the call right after a CRD applied alone, they are applied once the API
/// server serves their type, not refused as unknown until discovery runs again a minute later.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_agent_applies_custom_resources_right_after_their_crd() {
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;

    fn crd(plural: &str, kind: &str) -> String {
        let singular = kind.to_lowercase();
        format!(
            "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: {plural}.e2e.recouvrance.dev\nspec:\n  group: e2e.recouvrance.dev\n  scope: Namespaced\n  names:\n    kind: {kind}\n    plural: {plural}\n    singular: {singular}\n  versions:\n  - name: v1\n    served: true\n    storage: true\n    schema:\n      openAPIV3Schema:\n        type: object\n        x-kubernetes-preserve-unknown-fields: true\n"
        )
    }
    fn object(kind: &str, name: &str) -> String {
        format!(
            "apiVersion: e2e.recouvrance.dev/v1\nkind: {kind}\nmetadata:\n  name: {name}\n  namespace: {NS}\nspec:\n  size: 1\n"
        )
    }

    run(async {
        let ctx = context();
        let crds: Api<CustomResourceDefinition> = Api::all(k8s::connect_for_agents(ctx.clone()).await?);
        let names = ["e2ewidgets.e2e.recouvrance.dev", "e2egadgets.e2e.recouvrance.dev"];
        for n in names {
            let _ = crds.delete(n, &DeleteParams::default()).await;
            for _ in 0..60 {
                if crds.get_opt(n).await?.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        let checked = async {
            let (hub, _agent, token, _server) = agent(&["get,list,apply *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            // One manifest, the object before its CRD. Discovery first runs in this call,
            // before the CRD exists.
            let manifest = format!("{}---\n{}", object("E2eWidget", "one"), crd("e2ewidgets", "E2eWidget"));
            let dry = mcp.call("apply_manifest", json!({"context": ctx, "manifest": manifest, "dry_run": true})).await;
            assert!(dry.contains("E2eWidget one not checked"), "{dry}");
            assert!(crds.get_opt(names[0]).await?.is_none(), "a dry run created a CRD");
            let out = mcp.call("apply_manifest", json!({"context": ctx, "manifest": manifest})).await;
            assert!(out.contains("E2eWidget one applied"), "{out}");
            // A CRD alone, then its object in the very next call.
            let out =
                mcp.call("apply_manifest", json!({"context": ctx, "manifest": crd("e2egadgets", "E2eGadget")})).await;
            assert!(out.contains(&format!("CustomResourceDefinition {} applied", names[1])), "{out}");
            let out = mcp.call("apply_manifest", json!({"context": ctx, "manifest": object("E2eGadget", "two")})).await;
            assert!(out.contains("E2eGadget two applied"), "{out}");
            let target = json!({"context": ctx, "kind": "e2egadgets", "namespace": NS, "name": "two"});
            let out = mcp.call("get_resource", target).await;
            assert!(out.contains("size"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        for n in names {
            let _ = crds.delete(n, &DeleteParams::default()).await;
        }
        result
    });
}

/// The read-only questions about the cluster: explain, can-i, version and wait.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
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

/// The upgrade check names a Helm release whose last manifest uses a removed API, and a budget
/// that would block a drain; the Helm one only while the agent may list the release's Secret.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_agent_checks_an_upgrade() {
    use k8s_openapi::api::core::v1::{Pod, Secret};
    use k8s_openapi::api::policy::v1::PodDisruptionBudget;

    run(async {
        let ctx = context();
        let client = k8s::connect_for_agents(ctx.clone()).await?;
        let secrets: Api<Secret> = Api::namespaced(client.clone(), NS);
        let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
        let pdbs: Api<PodDisruptionBudget> = Api::namespaced(client, NS);
        let (release, secret) = ("e2e-upgrade", "sh.helm.release.v1.e2e-upgrade.v1");
        let _ = secrets.delete(secret, &DeleteParams::default()).await;
        let _ = pdbs.delete(release, &DeleteParams::default()).await;
        let mut payload = crate::helm::tests::payload(release, NS, 1);
        payload["manifest"] = json!(
            "---\n# Source: c/templates/ingress.yaml\napiVersion: extensions/v1beta1\nkind: Ingress\nmetadata:\n  name: e2e-old-web\n"
        );
        secrets
            .create(&PostParams::default(), &crate::helm::tests::helm_secret((secret, NS, release), &payload, true))
            .await?;
        busybox_pod(&pods, release, "sleep 3600").await?;
        pods.patch(release, &PatchParams::default(), &Patch::Merge(json!({"metadata": {"labels": {"app": release}}})))
            .await?;
        let pdb = serde_json::from_value(json!({
            "metadata": {"name": release, "namespace": NS},
            "spec": {"minAvailable": 1, "selector": {"matchLabels": {"app": release}}},
        }))?;
        pdbs.create(&PostParams::default(), &pdb).await?;
        // Once the disruption controller has counted the pod.
        for _ in 0..60 {
            if pdbs.get(release).await?.status.is_some_and(|s| s.expected_pods == 1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let checked = async {
            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let out = mcp.call("upgrade_check", json!({"context": ctx})).await;
            assert!(out.contains("Ingress extensions/v1beta1: removed in 1.22 already"), "{out}");
            assert!(out.contains(&format!("e2e-old-web: in Helm release {NS}/{release} (revision 1)")), "{out}");
            assert!(out.contains("guards a single pod"), "{out}");
            assert!(out.contains(&format!("poddisruptionbudgets.policy {NS}/{release}: minAvailable 1")), "{out}");
            let out = mcp.call("upgrade_check", json!({"context": ctx, "target": "next"})).await;
            assert!(out.contains("not a Kubernetes 1.x version"), "{out}");

            let deny = rules_file::parse_rule(Mode::Deny, "* secrets")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(deny))?;
            let out = mcp.call("upgrade_check", json!({"context": ctx})).await;
            assert!(!out.contains("e2e-old-web") && !out.contains("Ingress extensions/v1beta1"), "{out}");
            assert!(out.contains("about objects you may not see"), "{out}");
            assert!(out.contains("guards a single pod"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        let _ = pdbs.delete(release, &DeleteParams::default()).await;
        let _ = pods.delete(release, &DeleteParams::default().grace_period(0)).await;
        secrets.delete(secret, &DeleteParams::default()).await?;
        result
    });
}

/// A pod waiting on a ConfigMap that doesn't exist shows in `find_problems` with the
/// ConfigMap it misses; an optional one it also names isn't reported.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_agent_finds_a_pod_s_missing_config_map() {
    use k8s_openapi::api::core::v1::Pod;

    run(async {
        let ctx = context();
        let pods: Api<Pod> = Api::namespaced(k8s::connect_for_agents(ctx.clone()).await?, NS);
        let name = "e2e-dangling";
        let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        for _ in 0..60 {
            if pods.get_opt(name).await?.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let pod = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"containers": [{"name": "main", "image": "busybox:1.37", "command": ["sleep", "3600"],
                "envFrom": [
                    {"configMapRef": {"name": "e2e-missing-config"}},
                    {"configMapRef": {"name": "e2e-optional-config", "optional": true}},
                ]}]},
        });
        pods.create(&PostParams::default(), &serde_json::from_value(pod)?).await?;
        let checked = async {
            let (hub, _agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let mut out = String::new();
            for _ in 0..90 {
                // A new scan each time: the pod's state changes meanwhile.
                let args = json!({"context": ctx, "namespace": NS, "max_age_seconds": 0});
                out = mcp.call("find_problems", args).await;
                if out.lines().any(|l| l.contains(name) && l.contains("missing ConfigMap e2e-missing-config")) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            let line = out.lines().find(|l| l.contains(name)).with_context(|| out.clone())?;
            assert!(line.contains("missing ConfigMap e2e-missing-config"), "{out}");
            assert!(!line.contains("e2e-optional-config"), "{out}");
            // Asked again by default a few seconds later: the scan just made answers, and says
            // how old it is. A new scan would say it started 0s ago.
            let pause = 3;
            tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
            let again = mcp.call("find_problems", json!({"context": ctx, "namespace": NS})).await;
            assert!(again.lines().any(|l| l.contains(name)), "{again}");
            let age = again
                .split_once("From a scan started ")
                .and_then(|(_, rest)| rest.split_once(" ago"))
                .and_then(|(age, _)| k8s::age_seconds(age))
                .with_context(|| again.clone())?;
            assert!(age >= i64::try_from(pause)?, "the scan made with max_age_seconds: 0 answers: {again}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        result
    });
}

/// The reachability check reads the real cluster through the agent's rules: a Service that
/// selects no pods breaks at its selector, an Ingress sending to a port its Service lacks
/// breaks at that backend, and the TLS Secret the rules hide is not read.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_agent_checks_why_a_service_does_not_answer() {
    use k8s_openapi::api::core::v1::Service;
    use k8s_openapi::api::networking::v1::Ingress;

    run(async {
        let ctx = context();
        let client = k8s::connect_for_agents(ctx.clone()).await?;
        let services: Api<Service> = Api::namespaced(client.clone(), NS);
        let ingresses: Api<Ingress> = Api::namespaced(client, NS);
        let name = "e2e-reach";
        let _ = services.delete(name, &DeleteParams::default()).await;
        let _ = ingresses.delete(name, &DeleteParams::default()).await;
        let svc = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"selector": {"app": "e2e-reach-none"}, "ports": [{"name": "http", "port": 80, "targetPort": "http"}]},
        });
        let ing = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {
                "tls": [{"hosts": ["e2e.example"], "secretName": "e2e-reach-tls"}],
                "rules": [{"host": "e2e.example", "http": {"paths": [{"path": "/", "pathType": "Prefix",
                    "backend": {"service": {"name": name, "port": {"number": 81}}}}]}}],
            },
        });
        services.create(&PostParams::default(), &serde_json::from_value(svc)?).await?;
        ingresses.create(&PostParams::default(), &serde_json::from_value(ing)?).await?;
        let checked = async {
            let (hub, _agent, token, _server) = agent(&["get,list *"], &["* secrets"]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let args = |kind: &str| json!({"context": ctx, "kind": kind, "namespace": NS, "name": name});
            let out = mcp.call("check_reachability", args("services")).await;
            assert!(out.contains("[BROKEN] Selects no pods"), "{out}");
            let out = mcp.call("check_reachability", args("ingresses")).await;
            assert!(out.contains("has no port 81"), "{out}");
            assert!(out.contains(crate::reachability::NOT_ALLOWED), "{out}");
            let out = mcp.call("check_reachability", args("configmaps")).await;
            assert!(out.contains("takes a Service, an Ingress or an HTTPRoute"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        services.delete(name, &DeleteParams::default()).await?;
        ingresses.delete(name, &DeleteParams::default()).await?;
        result
    });
}

/// The Problems scan reads every kind of a real cluster, the objects it lists whole (Services,
/// Ingresses...) parsed lean from what the API server sends: none is skipped. The pods of a
/// label selector, read lean from the API server's cache, are those the selector picks.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn the_problems_scan_reads_a_real_cluster() {
    use k8s_openapi::api::core::v1::{Pod, Service};
    use k8s_openapi::api::networking::v1::Ingress;

    run(async {
        let client = k8s::connect_for_agents(context()).await?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
        let services: Api<Service> = Api::namespaced(client.clone(), NS);
        let ingresses: Api<Ingress> = Api::namespaced(client.clone(), NS);
        let name = "e2e-scan";
        let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        let _ = services.delete(name, &DeleteParams::default()).await;
        let _ = ingresses.delete(name, &DeleteParams::default()).await;
        let pod = json!({
            "metadata": {"name": name, "namespace": NS, "labels": {"app": name, "tier": "a"}},
            "spec": {"containers": [{"name": "main", "image": "busybox:1.37", "command": ["sleep", "3600"]}]},
        });
        let svc = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"selector": {"app": name}, "ports": [{"name": "http", "port": 80, "targetPort": "http"}]},
        });
        let ing = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {
                "defaultBackend": {"service": {"name": name, "port": {"name": "http"}}},
                "tls": [{"hosts": ["e2e.example"], "secretName": "e2e-scan-tls"}],
                "rules": [{"host": "e2e.example", "http": {"paths": [{"path": "/", "pathType": "Prefix",
                    "backend": {"service": {"name": name, "port": {"number": 80}}}}]}}],
            },
        });
        pods.create(&PostParams::default(), &serde_json::from_value(pod)?).await?;
        services.create(&PostParams::default(), &serde_json::from_value(svc)?).await?;
        ingresses.create(&PostParams::default(), &serde_json::from_value(ing)?).await?;
        let checked = async {
            // Escaped into the request, with the operators a Service's selector can't have.
            let selector = format!("app in ({name},e2e-other),tier=a,!e2e-absent");
            let mut found = Vec::new();
            // The cache catches up with the create in a moment.
            for _ in 0..20 {
                let ar = k8s::ar_of::<Pod>();
                let read = Some(selector.clone());
                found = k8s::lists::list_lean_cached_by::<serde_json::Value>(client.clone(), ar, Some(NS.into()), read)
                    .await?;
                if !found.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let names: Vec<&str> = found.iter().filter_map(|p| p.pointer("/metadata/name")?.as_str()).collect();
            assert_eq!(names, [name]);
            let kinds = k8s::discover(client.clone()).await?;
            let scan = crate::problems::scan_partial(client.clone(), &kinds, None, None).await;
            assert!(scan.skipped.is_empty(), "{:?}", scan.skipped_text());
            Ok(())
        };
        let result: Result<()> = checked.await;
        let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        let _ = services.delete(name, &DeleteParams::default()).await;
        let _ = ingresses.delete(name, &DeleteParams::default()).await;
        result
    });
}

/// The Files tab reads a container through exec, so write mode gates it like a shell: off,
/// the UI's client refuses the listing; on, `/etc` lists, and names with spaces, newlines
/// and non-UTF-8 bytes come back whole, a file previews and a search finds it, with
/// busybox's tools and with GNU's. An agent's `browse_files` needs exec on the pod.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn the_files_tab_reads_a_container() {
    use k8s_openapi::api::core::v1::Pod;

    use crate::k8s::files::{FileKind, FileSource, display_name};

    /// Odd names, made with the container's own shell.
    const MAKE: &str = r#"mkdir -p '/data/a dir' && echo hello > '/data/a dir/file one.txt' && touch "/data/$(printf 'new\nline')" "/data/$(printf 'latin1-\351')" && ln -s '/data/a dir' /data/link && printf '\177ELF\002\001' > /data/bin"#;

    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let pods: Api<Pod> = Api::namespaced(truth.clone(), NS);
        let ui = k8s::connect(ctx.clone()).await?;
        let names = ["e2e-files-busybox", "e2e-files-debian"];
        let checked = async {
            running_pod(&pods, names[0], "busybox:1.37", "while true; do sleep 1; done").await?;
            running_pod(&pods, names[1], "debian:bookworm-slim", "while true; do sleep 1; done").await?;
            for name in names {
                let make = vec!["sh".to_string(), "-c".into(), MAKE.into()];
                let out = k8s::exec_capture(truth.clone(), NS.into(), name.into(), None, make, 30, 4096).await?;
                assert!(out.starts_with("[exit 0]"), "{name}: {out}");
                let source = FileSource::exec(ui.clone(), NS.into(), name.into(), None);

                gate::set_ui_writes(false);
                let refused = source.list(b"/etc").await.err().map_or_default(|e| format!("{e:#}"));
                assert!(refused.contains(gate::WRITES_OFF), "{name}: {refused}");
                gate::set_ui_writes(true);

                let etc = source.list(b"/etc").await?;
                assert!(etc.iter().any(|e| e.name == b"passwd" && e.kind == FileKind::File), "{name}: no passwd");
                let data = source.list(b"/data").await?;
                let odd: [&[u8]; 5] = [b"a dir", b"new\nline", b"latin1-\xe9", b"link", b"bin"];
                for want in odd {
                    assert!(data.iter().any(|e| e.name == want), "{name}: {} is not listed", display_name(want));
                }
                let link = data.iter().find(|e| e.name == b"link").context("link")?;
                assert!(link.opens() && link.target.as_deref() == Some(&b"/data/a dir"[..]), "{name}: {link:?}");
                let text = source.read(b"/data/a dir/file one.txt", 1024).await?;
                assert_eq!(text.text(), Some("hello\n"), "{name}");
                assert_eq!(source.read(b"/data/bin", 1024).await?.kind(), "ELF executable or library", "{name}");
                assert!(source.read(b"/data/latin1-\xe9", 1024).await?.bytes.is_empty(), "{name}");
                let found = source.find(b"/data", "ONE").await?;
                assert_eq!(found.paths, [b"/data/a dir/file one.txt".to_vec()], "{name}");
            }
            gate::set_ui_writes(false);

            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let browse = json!({"context": ctx, "namespace": NS, "pod": names[0], "path": "/data/a dir"});
            let out = mcp.call("browse_files", browse.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            let allow = rules_file::parse_rule(Mode::Allow, "exec pods/e2e-* -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let out = mcp.call("browse_files", browse).await;
            assert!(out.contains("file one.txt"), "{out}");
            let read = json!({"context": ctx, "namespace": NS, "pod": names[0], "path": "/data/a dir/file one.txt"});
            let out = mcp.call("browse_files", read).await;
            assert!(out.contains("hello"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        for name in names {
            let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        }
        result
    });
}

/// The timeline records what a list's watch sees change, from the metadata alone: a label
/// set by someone (and who), then the deletion. The agents' `get_changes` returns the record
/// with its source, and `recent_changes` among the namespace's changes, with its events read
/// from the cluster; rules that keep the agent from reading ConfigMaps leave it out, counted.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn the_timeline_records_what_a_watch_sees_change() {
    run(async {
        use futures::StreamExt as _;
        let ctx = context();
        let client = k8s::connect_for_agents(ctx.clone()).await?;
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), NS);
        let name = "e2e-timeline";
        let _ = cms.delete(name, &DeleteParams::default()).await;
        let created = cms.create(&PostParams::default(), &config_map(name)).await?;
        let uid = created.metadata.uid.clone().unwrap_or_default();
        let checked = async {
            let kinds = k8s::discover(client.clone()).await?;
            let kind = k8s::find_kind(&kinds, "configmaps").cloned().context("no configmaps")?;
            let tap = crate::timeline::tap(&ctx, &kind, Some(NS.into())).context("configmaps are recorded")?;
            // Shown until it has listed (a list held back says so only once shown), then its
            // rows held back, as the app's list is while another page shows.
            let pause = k8s::Pause::default();
            let (_watch, mut rx) = k8s::watch_table_paused(
                client.clone(),
                kind,
                Some(NS.into()),
                Arc::default(),
                pause.clone(),
                false,
                Some(tap),
            );
            while let Some(msg) = rx.next().await {
                if matches!(msg, k8s::TableMsg::Listed) {
                    break;
                }
            }
            pause.set(true);
            let _drain = k8s::spawn_guarded(async move { while rx.next().await.is_some() {} });
            let recorded = || crate::timeline::changes_for(&ctx, "configmaps", Some(NS), name, &uid);
            assert!(recorded().is_empty(), "the first list only notes what is there");

            let by = PatchParams { field_manager: Some("e2e-editor".into()), ..Default::default() };
            cms.patch(name, &by, &Patch::Merge(json!({"metadata": {"labels": {"tier": "front"}}}))).await?;
            let mut found = Vec::new();
            for _ in 0..60 {
                found = recorded();
                if !found.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let summaries: Vec<&str> = found.iter().map(|c| c.summary.as_str()).collect();
            assert_eq!(summaries, ["labels changed: tier by e2e-editor"]);
            let diff = found[0].diff.as_ref().map_or_default(|d| d.short(5));
            assert!(diff.contains("tier: front"), "{diff}");

            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let target = json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": name});
            let out = mcp.call("get_changes", target).await;
            assert!(out.contains("Recorded  ConfigMap e2e-timeline: labels changed: tier by e2e-editor"), "{out}");

            let recent = json!({"context": ctx, "namespace": NS});
            let out = mcp.call("recent_changes", recent.clone()).await;
            let line = "Recorded  ConfigMap default/e2e-timeline: labels changed: tier by e2e-editor";
            assert!(out.contains(line) && !out.contains("Could not read events"), "{out}");
            let deny = rules_file::parse_rule(Mode::Deny, "get configmaps")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(deny))?;
            let out = mcp.call("recent_changes", recent).await;
            assert!(!out.contains(name) && out.contains("Left out, not allowed by your rules: "), "{out}");

            cms.delete(name, &DeleteParams::default()).await?;
            for _ in 0..60 {
                if recorded().iter().any(|c| c.summary == "deleted") {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            let summaries: Vec<String> = recorded().into_iter().map(|c| c.summary).collect();
            bail!("the deletion was not recorded: {summaries:?}")
        };
        let result: Result<()> = checked.await;
        let _ = cms.delete(name, &DeleteParams::default()).await;
        result
    });
}

/// The objects a follow sends, once `done` says they are what the test waits for (a minute at
/// most).
async fn followed_until(
    rx: &mut futures::channel::mpsc::UnboundedReceiver<k8s::Followed>,
    done: impl Fn(&[serde_json::Value]) -> bool,
) -> Result<Vec<serde_json::Value>> {
    use futures::StreamExt as _;
    let wait = async {
        while let Some(msg) = rx.next().await {
            if let k8s::Followed::Objects(objects) = msg
                && done(objects.as_slice())
            {
                return Ok(objects.to_vec());
            }
        }
        bail!("the follow ended")
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), wait).await.context("never sent")?
}

/// The namespaces a connect listed follow the cluster: one created afterwards shows in the
/// context's namespace watch, and goes once deleted, for every holder of that one watch.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn the_namespace_list_follows_namespaces_created_and_deleted() {
    use k8s_openapi::api::core::v1::Namespace;
    const NAME: &str = "recouvrance-ns-test";

    async fn until(
        feed: &mut k8s::namespace_feed::NamespaceHold,
        what: &str,
        holds: impl Fn(&[String]) -> bool,
    ) -> Result<()> {
        for _ in 0..120 {
            if holds(&feed.now().names) {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        bail!("{what}: {:?}", feed.now())
    }

    /// Once `NAME` is deleted: it is terminating until what it holds is gone.
    async fn gone(namespaces: &Api<Namespace>) -> Result<()> {
        for _ in 0..180 {
            if namespaces.get_opt(NAME).await?.is_none() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let left = namespaces.get_opt(NAME).await?.and_then(|ns| ns.status);
        bail!("{NAME} still terminating after 3 minutes: {left:?}")
    }

    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let namespaces: Api<Namespace> = Api::all(truth.clone());
        let _ = namespaces.delete(NAME, &DeleteParams::default()).await;
        gone(&namespaces).await?;

        let ui = k8s::connect(ctx.clone()).await?;
        let mut feed = k8s::namespace_feed::hold(&ctx, &ui);
        let first = feed.answer().await;
        let listed = first.result().map_err(anyhow::Error::msg)?;
        assert!(listed.iter().any(|n| n == "default"), "{listed:?}");
        assert!(!listed.iter().any(|n| n == NAME), "{listed:?}");
        // A second window shares the watch: answered at once, from the same list.
        let mut other = k8s::namespace_feed::hold(&ctx, &ui);
        assert_eq!(other.answer().await.names, first.names);

        let ns = serde_json::from_value(json!({"metadata": {"name": NAME}}))?;
        namespaces.create(&PostParams::default(), &ns).await?;
        until(&mut feed, "created", |n| n.iter().any(|n| n == NAME)).await?;
        until(&mut other, "created, in the other window", |n| n.iter().any(|n| n == NAME)).await?;
        assert!(k8s::namespace_feed::listed(&ctx).is_some_and(|n| n.iter().any(|n| n == NAME)));

        namespaces.delete(NAME, &DeleteParams::default()).await?;
        // Terminating until its contents are gone: listed until then, as the API lists it.
        gone(&namespaces).await?;
        until(&mut feed, "deleted", |n| !n.iter().any(|n| n == NAME)).await?;
        drop((feed, other));
        assert!(k8s::namespace_feed::listed(&ctx).is_none(), "the last hold stops the watch");
        Ok(())
    });
}

/// A detail page follows its object with a watch on its name, and its pods with a watch on
/// their labels, as a real API server serves them (field and label selectors, bookmarks,
/// deletions): a change shows with no read asked, a deletion says so, and the object created
/// again under its name is followed.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn a_detail_page_s_watches_follow_its_object_and_pods() {
    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let cms: Api<ConfigMap> = Api::namespaced(truth.clone(), NS);
        let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(truth, NS);
        let (name, pod) = ("e2e-follow", "e2e-follow-pod");
        let _ = cms.delete(name, &DeleteParams::default()).await;
        let _ = pods.delete(pod, &DeleteParams::default().grace_period(0)).await;
        let checked = async {
            // Reads only, as the window's client: write mode stays off.
            let ui = k8s::connect(ctx.clone()).await?;
            cms.create(&PostParams::default(), &config_map(name)).await?;
            let what = k8s::Follow::object(
                k8s::ar_of::<ConfigMap>(),
                Some(NS.into()),
                name,
                std::time::Duration::from_secs(5),
            );
            let (_object, mut rx) = k8s::follow(ui.clone(), what, k8s::Pause::default());
            let value =
                |objects: &[serde_json::Value]| objects.first().and_then(|o| o["data"]["a"].as_str()).map(String::from);
            let first = followed_until(&mut rx, |o| value(o).as_deref() == Some("1")).await?;
            // As the page shows it: typed, with its managed fields.
            assert_eq!(first[0]["kind"], "ConfigMap");
            assert!(first[0].pointer("/metadata/managedFields").is_some(), "{:#}", first[0]);
            let uid = first[0]["metadata"]["uid"].as_str().unwrap_or_default().to_string();
            cms.patch(name, &PatchParams::default(), &Patch::Merge(json!({"data": {"a": "2"}}))).await?;
            followed_until(&mut rx, |o| value(o).as_deref() == Some("2")).await?;
            cms.delete(name, &DeleteParams::default()).await?;
            followed_until(&mut rx, <[serde_json::Value]>::is_empty).await?;
            cms.create(&PostParams::default(), &config_map(name)).await?;
            let again = followed_until(&mut rx, |o| !o.is_empty()).await?;
            assert_ne!(again[0]["metadata"]["uid"].as_str(), Some(uid.as_str()), "the same object came back");

            let follow = k8s::Follow {
                ar: k8s::ar_of::<k8s_openapi::api::core::v1::Pod>(),
                namespace: Some(NS.into()),
                labels: Some("e2e=follow".into()),
                fields: None,
                keep: None,
                managed: false,
                cached: true,
                every: std::time::Duration::ZERO,
                poll: std::time::Duration::from_secs(15),
            };
            let (_pods, mut rx) = k8s::follow(ui, follow, k8s::Pause::default());
            followed_until(&mut rx, <[serde_json::Value]>::is_empty).await?;
            let labelled = serde_json::from_value(json!({
                "metadata": {"name": pod, "namespace": NS, "labels": {"e2e": "follow"}},
                "spec": {"containers": [{"name": "main", "image": "busybox:1.37", "command": ["sleep", "3600"]}]},
            }))?;
            pods.create(&PostParams::default(), &labelled).await?;
            let shown = followed_until(&mut rx, |o| o.iter().any(|p| p["metadata"]["name"] == pod)).await?;
            assert!(shown[0].pointer("/metadata/managedFields").is_none(), "a page's pods keep no managed fields");
            pods.delete(pod, &DeleteParams::default().grace_period(0)).await?;
            followed_until(&mut rx, <[serde_json::Value]>::is_empty).await?;
            Ok(())
        };
        let result: Result<()> = checked.await;
        let _ = cms.delete(name, &DeleteParams::default()).await;
        let _ = pods.delete(pod, &DeleteParams::default().grace_period(0)).await;
        result
    });
}

/// The usage bars' requests and limits (and the cluster overview's pod totals) come from one
/// list of the pods, then a watch: a pod created shows with its own, a resize in place changes
/// them, and a deleted pod goes, without another list.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn usage_bar_requests_follow_pods_by_a_watch() {
    use k8s_openapi::api::core::v1::Pod;

    /// Wait until the requests and limits read for pod `name` are `want`: None once it is gone.
    async fn read_until(rx: &mut k8s::PodRequestsRx, name: &str, want: Option<k8s::PodResources>) -> Result<()> {
        let seen = |read: &Option<Result<k8s::PodRequests, String>>| {
            let pod = read.as_ref().and_then(|r| r.as_ref().ok()).map(|r| r.pods.get(NS, name).map(|p| p.resources));
            pod == Some(want)
        };
        let waited = tokio::time::timeout(std::time::Duration::from_secs(90), rx.wait_for(seen)).await;
        if waited.is_ok_and(|read| read.is_ok()) {
            return Ok(());
        }
        let last = rx.borrow().clone().map(|read| read.map(|r| r.pods.get(NS, name).map(|p| p.resources)));
        bail!("{name}: wanted {want:?}, last read {last:?}")
    }

    run(async {
        const NAME: &str = "e2e-requests";
        let client = k8s::connect_for_agents(context()).await?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
        let _ = pods.delete(NAME, &DeleteParams::default().grace_period(0)).await;
        pod_gone(&pods, NAME).await?;
        let (_watch, mut rx) =
            k8s::watch_pod_resources(client.clone(), Some(NS.into()), k8s::Pause::default(), k8s::Pause::default());
        let checked = async {
            read_until(&mut rx, NAME, None).await?;
            let pod = serde_json::from_value(json!({
                "metadata": {"name": NAME, "namespace": NS},
                "spec": {"terminationGracePeriodSeconds": 0, "containers": [{
                    "name": "main", "image": "busybox:1.37", "command": ["sleep", "3600"],
                    "resources": {"requests": {"cpu": "50m", "memory": "32Mi"},
                        "limits": {"cpu": "100m", "memory": "64Mi"}},
                }]},
            }))?;
            pods.create(&PostParams::default(), &pod).await?;
            let mi = 1024.0 * 1024.0;
            let mut want =
                k8s::PodResources { cpu: (Some(50.0), Some(100.0)), mem: (Some(32.0 * mi), Some(64.0 * mi)) };
            read_until(&mut rx, NAME, Some(want)).await?;

            // Resized in place, through the `resize` subresource of Kubernetes 1.33 and later.
            let resize = json!({"spec": {"containers": [
                {"name": "main", "resources": {"requests": {"cpu": "60m"}, "limits": {"cpu": "120m"}}},
            ]}});
            match pods.patch_subresource("resize", NAME, &PatchParams::default(), &Patch::Strategic(resize)).await {
                Ok(_) => {
                    want.cpu = (Some(60.0), Some(120.0));
                    read_until(&mut rx, NAME, Some(want)).await?;
                }
                Err(kube::Error::Api(status)) if status.code == 404 => eprintln!("no in-place resize on this cluster"),
                Err(e) => return Err(e.into()),
            }

            pods.delete(NAME, &DeleteParams::default().grace_period(0)).await?;
            read_until(&mut rx, NAME, None).await
        };
        let result: Result<()> = checked.await;
        let _ = pods.delete(NAME, &DeleteParams::default().grace_period(0)).await;
        result
    });
}

/// A container without a shell (`pause`, a scratch image like distroless) is read through a
/// debug container: exec finds no shell, the debug container added shares its processes, and
/// its files read through `/proc/<pid>/root`, a mounted volume and absolute symlinks included.
/// Browsing again reuses it. The same in a namespace enforcing Pod Security `restricted`, as
/// the container's user. An agent's `browse_files` does it with `debug` and the exec rule.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn a_container_without_a_shell_is_read_through_a_debug_container() {
    use k8s_openapi::api::core::v1::{Namespace, Pod};

    use crate::k8s::files::debug::{self, Look};
    use crate::k8s::files::{FileSource, Origin};

    /// What the init container leaves in the shared volume: a file and a folder, and absolute
    /// symlinks to them, which only resolve from the container's own root.
    const MAKE: &str = "mkdir /data/dir && echo inner > /data/dir/inner && echo hello > /data/file && ln -s /data/file /data/abs-file && ln -s /data/dir /data/abs-dir";
    const RESTRICTED: &str = "e2e-files-restricted";
    const NAME: &str = "e2e-files-shellless";
    const USER: i64 = 65532;

    /// A pod whose container has no shell, with the volume made; in `restricted`, as `USER`.
    async fn shellless_pod(pods: &Api<Pod>, ns: &str, restricted: bool) -> Result<()> {
        let locked = json!({"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}});
        let mut spec = json!({
            "volumes": [{"name": "data", "emptyDir": {}}],
            "initContainers": [{"name": "make", "image": "busybox:1.37", "command": ["sh", "-c", MAKE],
                "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
            "containers": [{"name": "main", "image": "registry.k8s.io/pause:3.10",
                "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
        });
        if restricted {
            spec["securityContext"] = json!({"runAsNonRoot": true, "runAsUser": USER, "runAsGroup": USER,
                "fsGroup": USER, "seccompProfile": {"type": "RuntimeDefault"}});
            spec["initContainers"][0]["securityContext"] = locked.clone();
            spec["containers"][0]["securityContext"] = locked;
        }
        let pod = serde_json::from_value(json!({"metadata": {"name": NAME, "namespace": ns}, "spec": spec}))?;
        pods.create(&PostParams::default(), &pod).await?;
        for _ in 0..180 {
            let phase = pods.get(NAME).await?.status.and_then(|s| s.phase);
            if phase.as_deref() == Some("Running") {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        bail!("{ns}/{NAME} did not start")
    }

    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let ui = k8s::connect(ctx.clone()).await?;
        let namespaces: Api<Namespace> = Api::all(truth.clone());
        let _ = Api::<Pod>::namespaced(truth.clone(), NS).delete(NAME, &DeleteParams::default().grace_period(0)).await;
        let _ = namespaces.delete(RESTRICTED, &DeleteParams::default()).await;
        for _ in 0..120 {
            let gone = namespaces.get_opt(RESTRICTED).await?.is_none();
            let pod_gone = Api::<Pod>::namespaced(truth.clone(), NS).get_opt(NAME).await?.is_none();
            if gone && pod_gone {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let ns =
            json!({"metadata": {"name": RESTRICTED, "labels": {"pod-security.kubernetes.io/enforce": "restricted"}}});
        namespaces.create(&PostParams::default(), &serde_json::from_value(ns)?).await?;
        let checked = async {
            for (ns, restricted) in [(NS, false), (RESTRICTED, true)] {
                let pods: Api<Pod> = Api::namespaced(truth.clone(), ns);
                shellless_pod(&pods, ns, restricted).await?;
                gate::set_ui_writes(true);
                let exec = FileSource::exec(ui.clone(), ns.into(), NAME.into(), Some("main".into()));
                let refused = exec.list(b"/").await.err().context("pause has no shell")?;
                assert!(debug::needed(&refused), "{ns}: {refused:#}");

                let look = || debug::look(ui.clone(), ns.into(), NAME.into(), "main".into(), "busybox:1.37".into());
                let Look::Add(plan) = look().await? else { bail!("{ns}: a debug container to reuse already") };
                if restricted {
                    assert_eq!((plan.user, plan.group), (Some(USER), Some(USER)), "{ns}");
                }
                let source = debug::add(ui.clone(), ns.into(), NAME.into(), plan).await?;
                assert_eq!(source.origin(), Origin::Debug);
                let root = source.list(b"/").await?;
                assert!(root.iter().any(|e| e.name == b"pause"), "{ns}: no /pause in {root:?}");
                let data = source.list(b"/data").await?;
                let dir = data.iter().find(|e| e.name == b"abs-dir").context("abs-dir")?;
                assert!(dir.opens(), "{ns}: {dir:?}");
                let file = data.iter().find(|e| e.name == b"abs-file").context("abs-file")?;
                assert!(!file.opens(), "{ns}: {file:?}");
                assert_eq!(source.read(b"/data/abs-file", 64).await?.text(), Some("hello\n"), "{ns}");
                assert_eq!(source.read(b"/data/abs-dir/inner", 64).await?.text(), Some("inner\n"), "{ns}");
                assert!(source.list(b"/data/abs-dir").await?.iter().any(|e| e.name == b"inner"), "{ns}");
                let found = source.find(b"/data", "INNER").await?;
                assert_eq!(found.paths, [b"/data/dir/inner".to_vec()], "{ns}");
                let hostname = source.read(b"/etc/hostname", 64).await?;
                assert_eq!(hostname.text().map(str::trim), Some(NAME), "{ns}");

                let Look::Reuse(again) = look().await? else { bail!("{ns}: the debug container is not reused") };
                assert_eq!(again.container(), source.container(), "{ns}");
                gate::set_ui_writes(false);
            }

            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let read = json!({"context": ctx, "namespace": NS, "pod": NAME, "path": "/data/abs-file", "debug": true});
            let out = mcp.call("browse_files", read.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            let allow = rules_file::parse_rule(Mode::Allow, "exec pods/e2e-* -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let plain = json!({"context": ctx, "namespace": NS, "pod": NAME, "path": "/data"});
            let out = mcp.call("browse_files", plain).await;
            assert!(out.contains("debug: true"), "{out}");
            let out = mcp.call("browse_files", read).await;
            assert!(out.contains("hello"), "{out}");
            let folder = json!({"context": ctx, "namespace": NS, "pod": NAME, "path": "/data", "debug": true});
            let out = mcp.call("browse_files", folder).await;
            assert!(out.contains("abs-dir -> /data/dir") && out.contains("through a debug container"), "{out}");
            // Reused: still the one debug container the app added.
            let pod = Api::<Pod>::namespaced(truth.clone(), NS).get(NAME).await?;
            let added = pod.spec.and_then(|s| s.ephemeral_containers).unwrap_or_default();
            assert_eq!(added.len(), 1, "{:?}", added.iter().map(|c| &c.name).collect::<Vec<_>>());
            Ok(())
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        let _ = Api::<Pod>::namespaced(truth, NS).delete(NAME, &DeleteParams::default().grace_period(0)).await;
        let _ = namespaces.delete(RESTRICTED, &DeleteParams::default()).await;
        result
    });
}

/// The copies of pods in the test namespace.
async fn pod_copies(pods: &Api<k8s_openapi::api::core::v1::Pod>) -> Result<Vec<k8s_openapi::api::core::v1::Pod>> {
    let lp = kube::api::ListParams::default().labels(&format!("{}=true", k8s::pod_copy::LABEL));
    Ok(pods.list(&lp).await?.items)
}

/// Wait until pod `name` is gone.
async fn pod_gone(pods: &Api<k8s_openapi::api::core::v1::Pod>, name: &str) -> Result<()> {
    for _ in 0..60 {
        if pods.get_opt(name).await?.is_none() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    bail!("{name} is still there")
}

/// A container that isn't running is read from a copy of its pod: a crash-looping
/// distroless pod (no shell, no `sleep`) is copied with write mode on only, the copy runs the
/// busybox it carries, the pod's ConfigMap is mounted in it, and nothing selects it. The copy
/// is deleted even with write mode off, and one left behind (the app killed) is deleted by
/// the sweep run when the app connects again.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn a_stopped_container_is_read_from_a_copy_of_its_pod() {
    use k8s_openapi::api::core::v1::Pod;

    use crate::k8s::files::{FileSource, Origin};
    use crate::k8s::pod_copy;

    const NAME: &str = "e2e-files-crash";
    const CONFIG: &str = "e2e-files-crash-config";
    const BUSYBOX: &str = pod_copy::DEFAULT_IMAGE;

    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let pods: Api<Pod> = Api::namespaced(truth.clone(), NS);
        let maps: Api<ConfigMap> = Api::namespaced(truth.clone(), NS);
        let ui = k8s::connect(ctx.clone()).await?;
        let checked = async {
            let _ = pods.delete(NAME, &DeleteParams::default().grace_period(0)).await;
            let _ = maps.delete(CONFIG, &DeleteParams::default()).await;
            pod_gone(&pods, NAME).await?;
            let map = json!({"metadata": {"name": CONFIG, "namespace": NS}, "data": {"greeting": "hello"}});
            maps.create(&PostParams::default(), &serde_json::from_value(map)?).await?;
            // Distroless: nothing to exec in, and a command that isn't there, so it never runs.
            let pod = json!({
                "metadata": {"name": NAME, "namespace": NS, "labels": {"app": NAME}},
                "spec": {
                    "securityContext": {"runAsNonRoot": true, "runAsUser": 65532},
                    "containers": [{
                        "name": "main", "image": "gcr.io/distroless/static-debian12:nonroot", "command": ["/app"],
                        "livenessProbe": {"exec": {"command": ["/app", "health"]}},
                        "volumeMounts": [{"name": "config", "mountPath": "/etc/app"}],
                    }],
                    "volumes": [{"name": "config", "configMap": {"name": CONFIG}}],
                },
            });
            pods.create(&PostParams::default(), &serde_json::from_value(pod)?).await?;
            let mut read = None;
            for _ in 0..120 {
                let value = serde_json::to_value(pods.get(NAME).await?)?;
                let stopped = pod_copy::not_running(&value, "main");
                if stopped.as_deref().is_some_and(|s| s != "not started" && s != "ContainerCreating") {
                    read = Some(value);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            let read = read.context("the pod's container never stopped")?;
            let node = read.pointer("/spec/nodeName").cloned();

            // Write mode off: the UI's client refuses to create the copy.
            gate::set_ui_writes(false);
            let refused = pod_copy::create(ui.clone(), read.clone(), pod_copy::new_name(NAME), BUSYBOX.into()).await;
            let refused = refused.err().map_or_default(|e| format!("{e:#}"));
            assert!(refused.contains(gate::WRITES_OFF), "{refused}");
            assert!(pod_copies(&pods).await?.is_empty(), "a copy was created with write mode off");

            gate::set_ui_writes(true);
            let copy = pod_copy::create(ui.clone(), read.clone(), pod_copy::new_name(NAME), BUSYBOX.into()).await?;
            let made = pods.get(&copy.name).await?;
            assert_eq!(made.metadata.uid.as_deref(), Some(copy.uid.as_str()));
            let made = serde_json::to_value(made)?;
            // On the same node, selected by nothing, with no probes.
            assert_eq!(made.pointer("/spec/nodeName").cloned(), node);
            assert!(made.pointer("/metadata/labels/app").is_none(), "{made}");
            assert!(made.pointer("/metadata/ownerReferences").is_none(), "{made}");
            assert!(made.pointer("/spec/containers/0/livenessProbe").is_none(), "{made}");

            // The busybox's applets are installed (or the copy's state, later tries and kubectl
            // say why not).
            let tools =
                vec![format!("{}/busybox", pod_copy::TOOLS), "ls".into(), "-l".into(), pod_copy::TOOLS_BIN.into()];
            let exec = || {
                k8s::exec_capture(
                    truth.clone(),
                    NS.into(),
                    copy.name.clone(),
                    Some("main".into()),
                    tools.clone(),
                    30,
                    1 << 16,
                )
            };
            match exec().await {
                Ok(out) => assert!(out.starts_with("[exit 0]") && out.contains(" sh -> "), "{out}"),
                Err(e) => {
                    let mut later = Vec::new();
                    for _ in 0..3 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        later.push(exec().await.unwrap_or_else(|e| format!("{e:#}")));
                    }
                    let kubectl = std::process::Command::new("kubectl")
                        .args(["--context", ctx.as_str(), "-n", NS, "exec", copy.name.as_str(), "-c", "main", "--"])
                        .args(&tools)
                        .output()
                        .map(|o| {
                            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
                        });
                    let status = serde_json::to_value(pods.get(&copy.name).await?.status)?;
                    bail!("exec in the copy: {e:#}\nlater: {later:#?}\nkubectl: {kubectl:?}\n{status:#}");
                }
            }

            let source = FileSource::copy(ui.clone(), NS.into(), copy.name.clone(), Some("main".into()));
            assert_eq!(source.origin(), Origin::Copy);
            let config = source.list(b"/etc/app").await?;
            assert!(config.iter().any(|e| e.name == b"greeting"), "the ConfigMap is not mounted: {config:?}");
            assert_eq!(source.read(b"/etc/app/greeting", 1024).await?.text(), Some("hello"));
            let etc = source.list(b"/etc").await?;
            assert!(etc.iter().any(|e| e.name == b"passwd"), "the image's own files are not there: {etc:?}");
            let found = source.find(b"/etc/app", "GREET").await?;
            // The ConfigMap's link, and the file it leads to in its timestamped folder.
            assert!(found.paths.contains(&b"/etc/app/greeting".to_vec()), "{:?}", found.paths);

            // Deleted even with write mode off (the page closed after it was turned off).
            gate::set_ui_writes(false);
            pod_copy::delete(copy.clone());
            pod_gone(&pods, &copy.name).await?;
            assert!(pods.get_opt(NAME).await?.is_some(), "the original pod went too");

            // The app killed with a copy: the sweep of its next connection deletes it.
            gate::set_ui_writes(true);
            let left = pod_copy::create(ui.clone(), read, pod_copy::new_name(NAME), BUSYBOX.into()).await?;
            gate::set_ui_writes(false);
            let swept = pod_copy::sweep(ui, Vec::new(), jiff::SignedDuration::ZERO).await?;
            assert_eq!(swept, 1);
            pod_gone(&pods, &left.name).await?;
            Ok(())
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        for copy in pod_copies(&pods).await.unwrap_or_default() {
            let name = copy.metadata.name.unwrap_or_default();
            let _ = pods.delete(&name, &DeleteParams::default().grace_period(0)).await;
        }
        let _ = pods.delete(NAME, &DeleteParams::default().grace_period(0)).await;
        let _ = maps.delete(CONFIG, &DeleteParams::default()).await;
        result
    });
}

/// The rows of a watch of events until `ok` holds for them (30 s at most).
async fn rows_until(
    rx: &mut futures::channel::mpsc::UnboundedReceiver<k8s::EventsMsg>,
    ok: impl Fn(&k8s::EventRows) -> bool,
) -> Result<()> {
    use futures::StreamExt as _;
    let rows = async {
        while let Some(msg) = rx.next().await {
            match msg {
                k8s::EventsMsg::Rows(rows) if ok(&rows) => return Ok(()),
                k8s::EventsMsg::Rows(_) => {}
                k8s::EventsMsg::Error(e) => bail!("events: {e}"),
            }
        }
        bail!("the watch of events stopped")
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), rows).await.context("the rows never came")?
}

/// The Timeline's events on a real API server, which keeps events without a watch cache:
/// listed once, then a new Warning and a deleted one come through the watch from the list's
/// resourceVersion. Nothing is written through the app: the test makes the events itself.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn the_timeline_s_events_come_through_their_watch() {
    run(async {
        use k8s_openapi::api::core::v1::Event;
        let truth: Api<Event> = Api::namespaced(k8s::connect_for_agents(context()).await?, NS);
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let object = format!("e2e-timeline-{tag}");
        let warning = |reason: &str| -> Result<Event> {
            let now = jiff::Timestamp::now().to_string();
            Ok(serde_json::from_value(json!({
                "metadata": {"name": format!("{object}.{}", reason.to_lowercase()), "namespace": NS},
                "involvedObject": {"kind": "ConfigMap", "namespace": NS, "name": object},
                "type": "Warning", "reason": reason, "message": "from the e2e test", "count": 1,
                "firstTimestamp": now, "lastTimestamp": now, "source": {"component": "e2e"},
            }))?)
        };
        let has = |rows: &k8s::EventRows, reason: &str| {
            rows.rows.iter().any(|e| e.object_name == object && e.line.reason == reason)
        };
        truth.create(&PostParams::default(), &warning("E2eFirst")?).await?;
        let checked = async {
            let ui = k8s::connect(context()).await?;
            let (_watch, mut rx) = k8s::watch_events(
                ui,
                Some(NS.into()),
                false,
                k8s::EVENTS_KEPT,
                || std::time::Duration::ZERO,
                k8s::Pause::default(),
            );
            rows_until(&mut rx, |r| has(r, "E2eFirst")).await.context("listed")?;
            truth.create(&PostParams::default(), &warning("E2eSecond")?).await?;
            rows_until(&mut rx, |r| has(r, "E2eFirst") && has(r, "E2eSecond")).await.context("added")?;
            truth.delete(&format!("{object}.e2efirst"), &DeleteParams::default()).await?;
            rows_until(&mut rx, |r| !has(r, "E2eFirst") && has(r, "E2eSecond")).await.context("deleted")?;
            Ok(())
        };
        let result: Result<()> = checked.await;
        for reason in ["e2efirst", "e2esecond"] {
            let _ = truth.delete(&format!("{object}.{reason}"), &DeleteParams::default()).await;
        }
        result
    });
}

/// A fake Argo CD API server set in Settings → Argo CD, with a token, serving `apps`: the
/// server's address and name, and its task. kind runs no Argo CD; the agents' Argo CD tools
/// read and write only through its API.
async fn fake_argo(
    apps: Vec<serde_json::Value>,
) -> Result<(Arc<k8s::argo::api::fake::Fake>, String, String, k8s::TaskGuard)> {
    use k8s::argo::api;
    let (fake, base, task) = api::fake::Fake::start(apps).await?;
    crate::settings::update(|s| s.argocd_api_servers.push(base.clone()));
    api::save_token(&base, "e2e-token")?;
    api::reload();
    let name = base.trim_start_matches("http://").to_string();
    Ok((fake, base, name, task))
}

/// The server URL of the kind context, as an Application's destination.
fn kind_server(ctx: &str) -> Result<String> {
    let targets = k8s::argo::targets();
    targets.iter().find(|t| t.name == ctx).and_then(|t| t.server.clone()).context("the kind context has no server")
}

/// An agent syncs an Argo CD Application with options, terminates the operation and rolls
/// back through Argo CD's API, under its rules judged where the Application deploys (the
/// kind context and its destination namespace), never on the Argo CD server; what each asks
/// Argo CD is what argocd-server takes (the test plays Argo CD's controller). Nothing starts
/// while an operation runs, a rollback is refused while auto-sync is on, a server set
/// read-only refuses before the rules decide, and the window's writes need write mode.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_agent_syncs_terminates_and_rolls_back_an_argo_cd_application() {
    run(async {
        let ctx = context();
        let name = "e2e-argo";
        let source = json!({"repoURL": "https://git.example/apps", "path": "web", "targetRevision": "HEAD"});
        let (first, second) = ("1".repeat(40), "2".repeat(40));
        let app = json!({
            "metadata": {"name": name, "namespace": "argocd", "resourceVersion": "1"},
            "spec": {"project": "default", "source": source,
                "destination": {"server": kind_server(&ctx)?, "namespace": NS}},
            "status": {
                "sync": {"status": "OutOfSync"},
                "health": {"status": "Healthy"},
                "resources": [
                    {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": NS, "name": "web", "status": "OutOfSync"},
                    {"version": "v1", "kind": "ConfigMap", "namespace": NS, "name": "old", "status": "OutOfSync", "requiresPruning": true},
                    {"version": "v1", "kind": "ConfigMap", "namespace": NS, "name": "kept", "status": "Synced"},
                ],
                "history": [
                    {"id": 1, "revision": first, "deployedAt": "2025-01-01T00:00:00Z", "source": source},
                    {"id": 2, "revision": second, "deployedAt": "2025-01-02T00:00:00Z", "source": source},
                ],
            },
        });
        let (fake, base, server, _argo) = fake_argo(vec![app]).await?;
        let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
        let client = crate::assistant::Mcp::connect(hub.port(), token).await?;
        let target = json!({"server": server, "kind": "applications", "namespace": "argocd", "name": name});
        let with = |extra: serde_json::Value| {
            let mut args = target.clone();
            for (k, v) in extra.as_object().into_iter().flatten() {
                args[k] = v.clone();
            }
            args
        };
        let op = || fake.get(name).and_then(|a| a.get("operation").cloned()).filter(|o| !o.is_null());

        // Without a sync rule, nothing is sent (a sync is not a patch).
        let out = client.call("gitops", with(json!({"action": "sync"}))).await;
        assert!(out.contains("Denied"), "{out}");
        assert!(fake.writes().is_empty(), "{:?}", fake.writes());
        // A rule naming the Argo CD server opens nothing: the destination decides.
        let on_server =
            rules_file::parse_rule(Mode::Allow, &format!("sync applications.argoproj.io --context {server}"))?;
        hub.update_agent(&agent.id, |a| a.policy.rules.push(on_server))?;
        let out = client.call("gitops", with(json!({"action": "sync"}))).await;
        assert!(out.contains("Denied") && out.contains(&format!("--context {ctx}")), "{out}");
        assert!(fake.writes().is_empty());
        let allow = rules_file::parse_rule(Mode::Allow, &format!("sync applications/e2e-* -n {NS} --context {ctx}"))?;
        hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
        // The old way, a hub context, is refused with what to pass instead.
        let out = client
            .call(
                "gitops",
                json!({"context": ctx, "kind": "applications", "namespace": "argocd",
            "name": name, "action": "sync"}),
            )
            .await;
        assert!(out.contains("not on a context"), "{out}");

        // A sync of two resources with prune: it names what prune deletes.
        let picked = json!([
            {"group": "apps", "kind": "Deployment", "namespace": NS, "name": "web"},
            {"kind": "ConfigMap", "namespace": NS, "name": "old"},
        ]);
        let out = client.call("gitops", with(json!({"action": "sync", "resources": picked, "prune": true}))).await;
        assert!(out.contains("prune deletes ConfigMap default/old"), "{out}");
        let sent = op().context("no sync was sent")?;
        assert_eq!(sent["sync"]["prune"], true, "{sent}");
        assert_eq!(sent["sync"]["syncStrategy"], json!({"hook": {}}), "{sent}");
        assert_eq!(sent["sync"]["resources"], picked, "{sent}");

        // Nothing else starts while it runs; it can be terminated.
        let out = client.call("gitops", with(json!({"action": "sync"}))).await;
        assert!(out.contains("in progress"), "{out}");
        fake.set(name, &json!({"status": {"operationState": {"phase": "Running", "operation": sent}}}));
        let out = client.call("gitops", with(json!({"action": "terminate"}))).await;
        assert!(out.contains("Terminating"), "{out}");
        assert_eq!(fake.get(name).unwrap().pointer("/status/operationState/phase"), Some(&json!("Terminating")));

        // Argo CD ends it; a rollback goes to the deployment before the newest.
        fake.set(name, &json!({"operation": null, "status": {"operationState": {"phase": "Failed"}}}));
        let out = client.call("gitops", with(json!({"action": "rollback"}))).await;
        assert!(out.contains("Rollback to deployment 1"), "{out}");
        let sync = op().context("no rollback was sent")?["sync"].clone();
        assert_eq!(sync["revision"], first, "{sync}");
        assert_eq!(sync["source"], source, "{sync}");

        // With auto-sync on, Argo CD refuses a rollback: so is it here, before anything is sent.
        fake.set(name, &json!({"operation": null, "spec": {"syncPolicy": {"automated": {}}}}));
        let before = fake.writes().len();
        let out = client.call("gitops", with(json!({"action": "rollback", "deployment_id": 1}))).await;
        assert!(out.contains("auto-sync is on"), "{out}");
        assert_eq!(fake.writes().len(), before);
        fake.set(name, &json!({"spec": {"syncPolicy": null}}));

        // A dry run: its result comes back once Argo CD (here, the test) is done.
        let call = client.call("gitops", with(json!({"action": "sync", "dry_run": true})));
        let controller = async {
            for _ in 0..50 {
                if let Some(o) = op().filter(|o| o.pointer("/sync/dryRun") == Some(&json!(true))) {
                    fake.set(
                        name,
                        &json!({"operation": null, "status": {"operationState": {
                            "phase": "Succeeded", "message": "successfully synced (all tasks run)",
                            "startedAt": "2030-01-01T00:00:00Z", "operation": o,
                            "syncResult": {"resources": [{"kind": "ConfigMap", "namespace": NS, "name": "old",
                                "status": "Pruned", "message": "pruned (dry run)"}]},
                        }}}),
                    );
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            bail!("no dry run was sent")
        };
        let (out, simulated) = futures::join!(call, controller);
        simulated?;
        assert!(out.contains("Dry run Succeeded") && out.contains("Pruned: pruned (dry run)"), "{out}");

        // The window's grouped sync, out of sync only: the Application's resources out of sync
        // named, the one in sync and the one to prune (prune off) left out, no hooks.
        let endpoint =
            Arc::new(k8s::argo::api::endpoints().list.into_iter().find(|e| e.server.base == base).context("set")?);
        let checked = k8s::argo::grouped_sync::check(
            &fake.get(name).context("the Application")?,
            &k8s::argo::windows::NextSync::NoWindow,
            jiff::Timestamp::now(),
        );
        let opts =
            k8s::argo::grouped_sync::Options { hooks: false, prune: false, dry_run: false, ..Default::default() };
        let req = opts.request(&checked).expect("a resource is out of sync");
        gate::set_ui_writes(true);
        k8s::argo::api::act(endpoint.clone(), "argocd".into(), name.into(), k8s::argo::api::Action::Sync(req))
            .await
            .map_err(anyhow::Error::msg)?;
        let sync = op().context("no grouped sync was sent")?["sync"].clone();
        assert_eq!(
            sync["resources"],
            json!([{"group": "apps", "kind": "Deployment", "namespace": NS, "name": "web"}]),
            "{sync}"
        );
        assert_eq!(sync["syncStrategy"], json!({"apply": {}}), "{sync}");
        fake.set(name, &json!({"operation": null}));

        // A server set read-only refuses before the rules decide: even with a rule that allows it.
        let refresh =
            rules_file::parse_rule(Mode::Allow, &format!("refresh applications/e2e-* -n {NS} --context {ctx}"))?;
        hub.update_agent(&agent.id, |a| a.policy.rules.push(refresh))?;
        crate::settings::set_read_only(&server, true);
        let before = fake.writes().len();
        let out = client.call("gitops", with(json!({"action": "refresh"}))).await;
        assert!(out.contains("Denied") && out.contains("read-only"), "{out}");
        assert_eq!(fake.writes().len(), before);
        crate::settings::set_read_only(&server, false);
        let out = client.call("gitops", with(json!({"action": "refresh"}))).await;
        assert!(out.contains("refreshes"), "{out}");
        let before = fake.writes().len();

        // The window's writes: write mode off refuses them when they are sent.
        gate::set_ui_writes(false);
        let refused = k8s::argo::api::act(
            endpoint.clone(),
            "argocd".into(),
            name.into(),
            k8s::argo::api::Action::Refresh { hard: false },
        )
        .await;
        let err = refused.expect_err("write mode is off");
        assert!(err.contains(gate::WRITES_OFF), "{err}");
        assert_eq!(fake.writes().len(), before, "the refused refresh reached Argo CD");

        // The window's delete of an Application: refused with write mode off, sent with it
        // on, its resources kept when asked.
        let delete = k8s::argo::api::Action::Delete { cascade: false };
        let refused = k8s::argo::api::act(endpoint.clone(), "argocd".into(), name.into(), delete.clone()).await;
        assert!(refused.expect_err("write mode is off").contains(gate::WRITES_OFF));
        assert_eq!(fake.writes().len(), before, "the refused delete reached Argo CD");
        gate::set_ui_writes(true);
        k8s::argo::api::act(endpoint, "argocd".into(), name.into(), delete).await.map_err(anyhow::Error::msg)?;
        gate::set_ui_writes(false);
        let (method, path, _) = fake.writes().last().cloned().context("no delete was sent")?;
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("DELETE", "/api/v1/applications/e2e-argo?appNamespace=argocd&cascade=false")
        );
        assert!(fake.get(name).is_none(), "the Application is gone");
        Ok(())
    });
}

/// An agent follows an Argo CD Application read through Argo CD's API to its destination:
/// `argocd_apps` lists it with the context it deploys to, `argocd_diff` says what a sync
/// would change, `describe_resource` and `get_changes` on an object it deployed name it (by
/// its tracking id), and `wait_for` waits for its sync and returns at once when its sync
/// failed. Rows the rules don't allow where they deploy stay out. Only reads.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_agent_follows_an_argo_cd_application_to_what_it_deployed() {
    run(async {
        let ctx = context();
        let admin = k8s::connect_for_agents(ctx.clone()).await?;
        let maps: Api<ConfigMap> = Api::namespaced(admin.clone(), NS);
        let (name, hidden, cm) = ("e2e-argo-read", "e2e-argo-hidden", "e2e-argo-read-cm");
        let _ = maps.delete(cm, &DeleteParams::default()).await;
        let dest = kind_server(&ctx)?;
        let app = |app: &str| {
            json!({
                "metadata": {"name": app, "namespace": "argocd", "resourceVersion": "1"},
                "spec": {"project": "e2e", "destination": {"server": dest, "namespace": NS}},
                "status": {
                    "sync": {"status": "OutOfSync", "revision": "3".repeat(40)},
                    "health": {"status": "Healthy"},
                    "resources": [{"version": "v1", "kind": "ConfigMap", "namespace": NS, "name": cm, "status": "OutOfSync"}],
                    "history": [{"id": 1, "revision": "3".repeat(40), "deployedAt": "2025-01-01T00:00:00Z"}],
                },
            })
        };
        let (fake, _base, server, _argo) = fake_argo(vec![app(name), app(hidden)]).await?;
        let live = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": cm, "namespace": NS}, "data": {"a": "1"}});
        let mut want = live.clone();
        want["data"]["a"] = json!("2");
        fake.set_managed(
            name,
            json!({"items": [{"kind": "ConfigMap", "namespace": NS, "name": cm, "modified": true,
            "liveState": live.to_string(), "targetState": want.to_string()}]}),
        );
        let mut map = config_map(cm);
        map.metadata.annotations =
            Some([(k8s::argo::TRACKING_ID.to_string(), format!("argocd_{name}:/ConfigMap:{NS}/{cm}"))].into());
        maps.create(&PostParams::default(), &map).await?;
        let checked = async {
            let deny = format!("get,list applications.argoproj.io/{hidden} -n {NS}");
            let (hub, _agent, token, _server) = agent(&["get,list *"], &[deny.as_str()]).await?;
            let client = crate::assistant::Mcp::connect(hub.port(), token).await?;

            // The server's Applications, with where each deploys; the one denied there stays out.
            let out = client.call("argocd_apps", json!({"server": server})).await;
            assert!(out.contains(&format!("argocd/{name}")), "{out}");
            assert!(out.contains(&format!("{ctx} / {NS}")), "{out}");
            assert!(!out.contains(hidden), "{out}");
            assert!(out.contains("1 Application your rules don't let you list"), "{out}");
            // Out of sync with auto-sync off is not a problem.
            let out = client.call("argocd_apps", json!({"server": server, "problems_only": true})).await;
            assert!(out.contains("No Applications found."), "{out}");

            // What a sync would change.
            let out = client.call("argocd_diff", json!({"server": server, "applications": [name]})).await;
            assert!(out.contains("a sync would change 1 resource") && out.contains(cm), "{out}");

            // The object it deployed names it, with its state, and its deployments.
            let target = json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": cm});
            let out = client.call("describe_resource", target.clone()).await;
            assert!(out.contains(&format!("managed by Application argocd/{name} on Argo CD server {server}")), "{out}");
            assert!(out.contains("This object in it: OutOfSync"), "{out}");
            let out = client.call("get_changes", target).await;
            assert!(out.contains(&format!("Deployment 1 · Application {name}")), "{out}");

            // Its sync, once no operation runs; a failed sync ends the wait at once.
            let wait = |condition: &str| {
                json!({"server": server, "kind": "applications", "namespace": "argocd", "name": name,
                    "condition": condition, "timeout_seconds": 20})
            };
            fake.set(name, &json!({"status": {"sync": {"status": "Synced"}}}));
            let out = client.call("wait_for", wait("sync=Synced")).await;
            assert!(out.contains("sync=Synced") && !out.contains("timed out"), "{out}");
            fake.set(
                name,
                &json!({"status": {"sync": {"status": "OutOfSync"},
                "operationState": {"phase": "Failed", "message": "e2e: one or more objects failed to apply"}}}),
            );
            let started = std::time::Instant::now();
            let out = client.call("wait_for", wait("sync=Synced")).await;
            assert!(out.contains("ended Failed: e2e: one or more objects failed to apply"), "{out}");
            assert!(started.elapsed() < std::time::Duration::from_secs(15), "it waited for the timeout");
            let on_map = json!({"context": ctx, "kind": "configmaps", "namespace": NS, "name": cm,
                "condition": "sync=Synced"});
            let out = client.call("wait_for", on_map).await;
            assert!(out.contains("a condition of Argo CD Applications"), "{out}");
            Ok(())
        };
        let result: Result<()> = checked.await;
        let _ = maps.delete(cm, &DeleteParams::default()).await;
        result
    });
}

/// Wait until `get` finds nothing, for a minute at most.
async fn until_gone<F, Fut, T>(what: &str, get: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = kube::Result<Option<T>>>,
{
    for _ in 0..60 {
        if get().await?.is_none() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    bail!("{what} is still there")
}

/// An orphan delete keeps what the object owns: an agent's `delete_resource` with
/// `cascade: orphan` (its delete rule needed, an unknown cascade refused) deletes a Deployment
/// and leaves its ReplicaSet, now without an owner; the window's delete with the same option
/// then deletes that ReplicaSet, held by a finalizer of its own that it removes, and leaves
/// its pod.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_orphan_delete_keeps_what_the_object_owns() {
    use k8s_openapi::api::apps::v1::{Deployment, ReplicaSet};
    use k8s_openapi::api::core::v1::Pod;
    /// A finalizer no controller removes.
    const HOLD: &str = "e2e.recouvrance.dev/hold";

    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let deps: Api<Deployment> = Api::namespaced(truth.clone(), NS);
        let sets: Api<ReplicaSet> = Api::namespaced(truth.clone(), NS);
        let pods: Api<Pod> = Api::namespaced(truth.clone(), NS);
        let name = "e2e-orphan";
        let selector = kube::api::ListParams::default().labels(&format!("app={name}"));
        let clean = || async {
            let _ = deps.delete(name, &DeleteParams::background()).await;
            // A ReplicaSet a failed run left held by `HOLD`.
            for rs in sets.list(&selector).await.map_or_default(|l| l.items) {
                let free = Patch::Merge(json!({"metadata": {"finalizers": null}}));
                let _ = sets.patch(&rs.metadata.name.unwrap_or_default(), &PatchParams::default(), &free).await;
            }
            let _ = sets.delete_collection(&DeleteParams::background(), &selector).await;
            let _ = pods.delete_collection(&DeleteParams::default().grace_period(0), &selector).await;
        };
        clean().await;
        until_gone("the Deployment", || deps.get_opt(name)).await?;
        let dep = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}}, "template": {
                "metadata": {"labels": {"app": name}},
                "spec": {"terminationGracePeriodSeconds": 0, "containers": [{"name": "main", "image": "busybox:1.37", "command": ["sh", "-c", "sleep 3600"]}]},
            }},
        });
        deps.create(&PostParams::default(), &serde_json::from_value(dep)?).await?;
        let checked = async {
            // Its ReplicaSet and pod, once the controllers made them.
            let mut owned = None;
            for _ in 0..60 {
                let rs = sets.list(&selector).await?.items.into_iter().next();
                let pod = pods.list(&selector).await?.items.into_iter().next();
                if let (Some(rs), Some(pod)) = (rs, pod) {
                    owned = Some((rs, pod));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            let (rs, pod) = owned.context("the Deployment made no ReplicaSet and pod")?;
            let (rs_name, pod_name) =
                (rs.metadata.name.clone().unwrap_or_default(), pod.metadata.name.unwrap_or_default());

            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let delete =
                json!({"context": ctx, "kind": "deployments", "namespace": NS, "name": name, "cascade": "orphan"});
            let out = mcp.call("delete_resource", delete.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            let allow = rules_file::parse_rule(Mode::Allow, "delete deployments/e2e-* -n default")?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let mut unknown = delete.clone();
            unknown["cascade"] = json!("keep");
            let out = mcp.call("delete_resource", unknown).await;
            assert!(out.contains("unknown cascade `keep`"), "{out}");
            assert!(deps.get_opt(name).await?.is_some(), "a delete with an unknown cascade was sent");
            let out = mcp.call("delete_resource", delete).await;
            assert!(out.contains("deleted, what it owns kept"), "{out}");
            until_gone("the orphaned Deployment", || deps.get_opt(name)).await?;
            // The garbage collector takes the owner off its ReplicaSet, and leaves it.
            let mut freed = false;
            for _ in 0..60 {
                let rs = sets.get_opt(&rs_name).await?.context("the orphaned ReplicaSet was deleted")?;
                if rs.metadata.owner_references.unwrap_or_default().is_empty() {
                    freed = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            assert!(freed, "the ReplicaSet kept its owner");
            assert!(pods.get_opt(&pod_name).await?.is_some(), "the pod went with its Deployment");

            // The window's delete, through write mode, orphans the pod the same way.
            gate::set_ui_writes(true);
            let ui = k8s::connect(ctx.clone()).await?;
            let kind =
                k8s::Kind { ar: k8s::ar_of::<ReplicaSet>(), namespaced: true, section: "", label: "ReplicaSet".into() };
            // A finalizer of its own keeps it terminating: removing finalizers must still leave
            // the garbage collector's `orphan`, or the pod would be deleted after all.
            let hold = Patch::Merge(json!({"metadata": {"finalizers": [HOLD]}}));
            sets.patch(&rs_name, &PatchParams::default(), &hold).await?;
            let opts = k8s::DeleteOptions {
                cascade: Some(k8s::Cascade::Orphan),
                strip_finalizers: true,
                ..Default::default()
            };
            let uid = rs.metadata.uid.clone();
            let deleted = k8s::delete_with(ui, kind, Some(NS.into()), rs_name.clone(), uid, opts).await;
            gate::set_ui_writes(false);
            let deleted = deleted?;
            assert!(deleted.contains("what it owns kept, finalizers removed"), "{deleted}");
            until_gone("the orphaned ReplicaSet", || sets.get_opt(&rs_name)).await?;
            for _ in 0..30 {
                let pod = pods.get_opt(&pod_name).await?.context("the pod went with its ReplicaSet")?;
                if pod.metadata.owner_references.unwrap_or_default().is_empty() {
                    assert!(pod.metadata.deletion_timestamp.is_none(), "the orphaned pod is being deleted");
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            bail!("the pod kept its owner")
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        clean().await;
        result
    });
}

/// An eviction respects PodDisruptionBudgets: an agent's `evict_pod` (dry run or not) and the
/// window's Evict are refused while a budget allows no disruption, the refusal naming it, and
/// the pod stays. Without the budget the same eviction goes through.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_eviction_a_budget_forbids_is_refused() {
    use k8s_openapi::api::core::v1::Pod;
    use k8s_openapi::api::policy::v1::PodDisruptionBudget;

    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let pods: Api<Pod> = Api::namespaced(truth.clone(), NS);
        let pdbs: Api<PodDisruptionBudget> = Api::namespaced(truth.clone(), NS);
        let name = "e2e-evict";
        let _ = pdbs.delete(name, &DeleteParams::default()).await;
        let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        until_gone("the pod", || pods.get_opt(name)).await?;
        until_gone("the budget", || pdbs.get_opt(name)).await?;
        let pod = json!({
            "metadata": {"name": name, "namespace": NS, "labels": {"app": name}},
            "spec": {"terminationGracePeriodSeconds": 0, "containers": [{"name": "main", "image": "busybox:1.37", "command": ["sh", "-c", "sleep 3600"]}]},
        });
        pods.create(&PostParams::default(), &serde_json::from_value(pod)?).await?;
        // A pod still pending is evicted whatever its budget says: wait until it runs.
        let mut running = false;
        for _ in 0..120 {
            let phase = pods.get(name).await?.status.and_then(|s| s.phase);
            if phase.as_deref() == Some("Running") {
                running = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        anyhow::ensure!(running, "{name} did not start");
        // Its only pod must stay: no disruption allowed.
        let pdb = json!({
            "metadata": {"name": name, "namespace": NS},
            "spec": {"minAvailable": 1, "selector": {"matchLabels": {"app": name}}},
        });
        pdbs.create(&PostParams::default(), &serde_json::from_value(pdb)?).await?;
        let checked = async {
            let (hub, _agent, token, _server) =
                agent(&["get,list *", &format!("delete pods/{name} -n {NS}")], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let evict = json!({"context": ctx, "namespace": NS, "pod": name});
            let mut dry = evict.clone();
            dry["dry_run"] = json!(true);
            let refusal = format!("PodDisruptionBudget {name} refuses it");
            for args in [dry.clone(), evict.clone()] {
                let out = mcp.call("evict_pod", args).await;
                assert!(out.contains(&refusal), "{out}");
            }
            gate::set_ui_writes(true);
            let ui = k8s::connect(ctx.clone()).await?;
            let refused = k8s::evict(ui.clone(), NS.into(), name.into(), None, false).await;
            gate::set_ui_writes(false);
            let err = format!("{:#}", refused.expect_err("an eviction its budget forbids went through"));
            assert!(err.contains(&refusal), "{err}");
            let pod = pods.get_opt(name).await?.context("a refused eviction deleted the pod")?;
            assert!(pod.metadata.deletion_timestamp.is_none(), "a refused eviction deleted the pod");

            // Without the budget, the same eviction goes through; a dry run first changes nothing.
            pdbs.delete(name, &DeleteParams::default()).await?;
            until_gone("the budget", || pdbs.get_opt(name)).await?;
            let out = mcp.call("evict_pod", dry).await;
            assert!(out.contains(&format!("[dry run] Pod {name} evicted")), "{out}");
            let pod = pods.get_opt(name).await?.context("a dry run evicted the pod")?;
            assert!(pod.metadata.deletion_timestamp.is_none(), "a dry run evicted the pod");
            let out = mcp.call("evict_pod", evict).await;
            assert!(out.contains(&format!("Pod {name} evicted")) && !out.contains("dry run"), "{out}");
            until_gone("the evicted pod", || pods.get_opt(name)).await
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        let _ = pdbs.delete(name, &DeleteParams::default()).await;
        let _ = pods.delete(name, &DeleteParams::default().grace_period(0)).await;
        result
    });
}

/// An `ExternalSecret`'s refresh sets the annotation the operator watches, through write mode in
/// the window and under the `refresh` verb for an agent; a dry run sets nothing. A stand-in CRD
/// of the operator's group is enough: what the operator then does is its own.
#[test]
#[ignore = "needs a kind cluster: RECOUVRANCE_E2E_CONTEXT"]
fn an_external_secret_refresh_sets_its_force_sync() {
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind};

    run(async {
        let ctx = context();
        let truth = k8s::connect_for_agents(ctx.clone()).await?;
        let crds: Api<CustomResourceDefinition> = Api::all(truth.clone());
        let crd_name = "externalsecrets.external-secrets.io";
        // A cluster that runs the operator keeps its own CRD.
        if crds.get_opt(crd_name).await?.is_some_and(|c| {
            c.metadata.labels.unwrap_or_default().get("app.kubernetes.io/managed-by").map(String::as_str)
                != Some("recouvrance-e2e")
        }) {
            bail!("{crd_name} exists and isn't this test's: run it on a cluster without the operator");
        }
        let crd = json!({
            "metadata": {"name": crd_name, "labels": {"app.kubernetes.io/managed-by": "recouvrance-e2e"}},
            "spec": {"group": "external-secrets.io", "scope": "Namespaced",
                "names": {"kind": "ExternalSecret", "plural": "externalsecrets", "singular": "externalsecret"},
                "versions": [{"name": "v1", "served": true, "storage": true,
                    "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}}}]},
        });
        if crds.get_opt(crd_name).await?.is_none() {
            crds.create(&PostParams::default(), &serde_json::from_value(crd)?).await?;
        }
        let ar = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("external-secrets.io", "v1", "ExternalSecret"),
            "externalsecrets",
        );
        let secrets: Api<DynamicObject> = Api::namespaced_with(truth.clone(), NS, &ar);
        let name = "e2e-refresh";
        let object: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "external-secrets.io/v1", "kind": "ExternalSecret",
            "metadata": {"name": name, "namespace": NS}, "spec": {"refreshInterval": "1h"},
        }))?;
        // The new kind is served a moment after the CRD is created.
        let mut created = Err(anyhow::anyhow!("not tried"));
        for _ in 0..30 {
            let _ = secrets.delete(name, &DeleteParams::default()).await;
            created = secrets.create(&PostParams::default(), &object).await.map_err(anyhow::Error::from);
            if created.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        created?;
        let checked = async {
            let forced = || async {
                let obj = secrets.get(name).await?;
                Ok::<_, anyhow::Error>(obj.metadata.annotations.unwrap_or_default().get(k8s::FORCE_SYNC).cloned())
            };
            let (hub, agent, token, _server) = agent(&["get,list *"], &[]).await?;
            let mcp = crate::assistant::Mcp::connect(hub.port(), token).await?;
            let refresh = json!({"context": ctx, "namespace": NS, "name": name});
            let out = mcp.call("refresh_external_secret", refresh.clone()).await;
            assert!(out.contains("Denied"), "{out}");
            let allow = rules_file::parse_rule(Mode::Allow, &format!("refresh {crd_name}/{name} -n {NS}"))?;
            hub.update_agent(&agent.id, |a| a.policy.rules.push(allow))?;
            let mut dry = refresh.clone();
            dry["dry_run"] = json!(true);
            let out = mcp.call("refresh_external_secret", dry).await;
            assert!(out.contains("[dry run] ExternalSecret"), "{out}");
            assert_eq!(forced().await?, None, "a dry run set the annotation");
            let out = mcp.call("refresh_external_secret", refresh).await;
            assert!(out.contains("refresh asked"), "{out}");
            let first = forced().await?.context("the agent's refresh set no annotation")?;
            assert!(first.parse::<i64>().is_ok(), "{first}");

            // In the window: refused while write mode is off.
            let ui = k8s::connect(ctx.clone()).await?;
            let kinds = k8s::discover(ui.clone()).await?;
            let kind = k8s::find_kind(&kinds, crd_name).context("the ExternalSecret kind")?.clone();
            gate::set_ui_writes(false);
            let off =
                k8s::force_sync_external_secret(ui.clone(), kind.clone(), Some(NS.into()), name.into(), false).await;
            assert!(format!("{:#}", off.expect_err("a refresh with write mode off")).contains(gate::WRITES_OFF));
            // A second later, so the value is new and the operator sees a change.
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            gate::set_ui_writes(true);
            let on = k8s::force_sync_external_secret(ui, kind, Some(NS.into()), name.into(), false).await;
            gate::set_ui_writes(false);
            on?;
            let second = forced().await?.context("the window's refresh set no annotation")?;
            assert_ne!(first, second, "a second refresh kept the same value");
            Ok(())
        };
        let result: Result<()> = checked.await;
        gate::set_ui_writes(false);
        let _ = secrets.delete(name, &DeleteParams::default()).await;
        let _ = crds.delete(crd_name, &DeleteParams::default()).await;
        result
    });
}
