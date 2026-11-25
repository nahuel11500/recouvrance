//! Argo CD Applications that need attention, shown under the cluster they deploy to: Degraded,
//! a failed sync, an error Argo CD reports, a sync running for long, and resources Missing
//! while auto-sync is on. The Applications are read from Argo CD's API (the feeds the Argo CD
//! page holds, else one list per server a minute), or on their hub when no Argo CD server is
//! logged in.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kube::Client;
use serde::Deserialize as _;

use super::{OnHub, Problem, Scan, Severity};
use crate::k8s;
use crate::k8s::ContextTarget;
use crate::k8s::argo::{self, LeanApp, api};
use crate::status::Tone;

/// What a row says the kind is.
pub const LABEL: &str = "Argo CD Application";

/// The longest message of Argo CD's kept in a reason.
const MESSAGE_CHARS: usize = 200;

/// The check's name among a scan's checks.
pub const CHECK: &str = "Argo CD Applications";

/// What a row's reason ends with when the Application's Argo CD is another cluster, or a server.
pub fn hub_note(hub: &str) -> String {
    format!(" (Argo CD on {hub})")
}

/// Where an Application lives, for an agent to read it there: "Argo CD Application
/// argocd/web on hub", or "... on Argo CD at argocd.example.com" when read by Argo CD's API.
pub fn location(on: &OnHub, name: &str) -> String {
    let at = if on.server.is_some() { "Argo CD at " } else { "" };
    format!("{LABEL} {}/{name} on {at}{}", on.namespace, on.context)
}

/// The hub or server a skipped entry of this check names ("Argo CD Applications on hub
/// (forbidden)").
pub fn skipped_hub(entry: &str) -> Option<&str> {
    let rest = entry.strip_prefix(CHECK)?.strip_prefix(" on ")?;
    Some(rest.split_once(" (").map_or(rest, |(hub, _)| hub))
}

/// "what: message", the message on one line and cut to `MESSAGE_CHARS`.
fn with_message(what: &str, message: Option<&str>) -> String {
    let Some(message) = message.map(str::trim).filter(|m| !m.is_empty()) else { return what.to_string() };
    let line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let short = if line.chars().count() > MESSAGE_CHARS {
        format!("{}…", line.chars().take(MESSAGE_CHARS - 1).collect::<String>())
    } else {
        line
    };
    format!("{what}: {short}")
}

/// Whether an Application needs attention, and why: Degraded is critical; a failed sync, a
/// condition whose type ends in `Error` (`ComparisonError`, `SyncError`, `InvalidSpecError`...),
/// a sync running for more than 10 minutes and Missing with auto-sync on are warnings. Missing
/// with auto-sync off is how an Application synced by hand shows what it has not deployed yet,
/// and out of sync is how it waits for its next sync: neither counts then (out of sync never
/// does). One being deleted is the stuck-deletion check's. None: nothing to look at.
pub fn judge(app: &LeanApp, now: jiff::Timestamp) -> Option<(Severity, String)> {
    if app.metadata.deleted.is_some() {
        return None;
    }
    // Not looked at by Argo CD yet.
    let status = app.status.as_ref()?;
    let mut worst: Option<Severity> = None;
    let mut why: Vec<String> = Vec::new();
    let mut add = |severity: Severity, text: String| {
        worst = Some(worst.map_or(severity, |w| w.min(severity)));
        why.push(text);
    };
    let health = status.health.as_ref();
    let message = health.and_then(|h| h.message.as_deref());
    // The Argo CD page's rule, so the two can't disagree on Missing.
    if let Some(state) = health.and_then(|h| h.status.as_deref())
        && argo::listed_health_tone(state, app.automated()) == Tone::Bad
    {
        let severity = if state == "Degraded" { Severity::Critical } else { Severity::Warning };
        add(severity, with_message(state, message));
    }
    if let Some(op) = &status.operation {
        let phase = op.phase.as_deref().unwrap_or_default();
        if argo::operation_tone(phase) == Tone::Bad {
            add(Severity::Warning, with_message("last sync failed", op.message.as_deref()));
        } else if matches!(phase, "Running" | "Terminating") {
            let started = op.started.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok());
            if let Some(secs) = started.map(|t| now.as_second() - t.as_second())
                && secs >= argo::diagnosis::STUCK_AFTER
            {
                add(Severity::Warning, format!("sync running for {}", k8s::duration_text(secs)));
            }
        }
    }
    for c in status.conditions.iter().flatten() {
        if let Some(kind) = c.kind.as_deref().filter(|k| k.ends_with("Error")) {
            add(Severity::Warning, with_message(kind, c.message.as_deref()));
        }
    }
    Some((worst?, why.join("; ")))
}

/// Where a scan reads Applications: a hub context, or an Argo CD API server.
#[derive(Clone, Debug)]
struct Home {
    /// What a row and a skipped entry name it by: the hub context, or the server's name.
    name: String,
    /// The cluster it runs on (`in-cluster`).
    target: ContextTarget,
    /// A server's Argo CD (`Endpoint::instance`); None for a hub.
    server: Option<String>,
}

/// The Applications that need attention among those `home` holds that deploy to the cluster
/// of `here`, in `namespaces` (all when empty), each under the namespace it deploys into.
fn found_on(
    apps: &[LeanApp],
    home: &Home,
    here: &ContextTarget,
    namespaces: &[String],
    now: jiff::Timestamp,
) -> Vec<Problem> {
    apps.iter()
        .filter(|a| a.deploys_to(&home.target, here))
        .filter(|a| a.dest_namespace().is_none_or(|ns| namespaces.is_empty() || namespaces.iter().any(|n| n == ns)))
        .filter_map(|a| {
            let (severity, mut reason) = judge(a, now)?;
            // The row is under the cluster it deploys to: say where Argo CD is when elsewhere.
            if home.server.is_some() || home.name != here.name {
                reason.push_str(&hub_note(&home.name));
            }
            Some(Problem {
                severity,
                kind: argo::application_kind().kubectl_name(),
                label: LABEL.into(),
                namespace: a.dest_namespace().map(String::from),
                name: a.metadata.name.clone(),
                reason,
                age: a.metadata.created.as_deref().and_then(|t| t.parse().ok()),
                on_hub: Some(OnHub {
                    context: home.name.clone(),
                    namespace: a.metadata.namespace.clone(),
                    server: home.server.clone(),
                }),
            })
        })
        .collect()
}

/// How long a scan waits for a server's list of Applications.
const LIST_DEADLINE: Duration = Duration::from_secs(20);

/// A server's Applications are listed for the Problems scans at most this often when no
/// feed runs, however many clusters are scanned.
const LIST_TTL: Duration = Duration::from_secs(60);

type Kept = HashMap<String, (Instant, Result<Arc<Vec<LeanApp>>, String>)>;

/// Each server's Applications as the scans last listed them, or why they could not be, by
/// feed key (`api::watch_key`), with when.
static LISTS: LazyLock<Mutex<Kept>> = LazyLock::new(Default::default);

/// The Applications as the scans read them.
fn leaned<'a>(apps: impl Iterator<Item = &'a Arc<api::ApiApp>>) -> Vec<LeanApp> {
    apps.filter_map(|a| LeanApp::deserialize(&a.value).ok()).collect()
}

/// A server's Applications for a scan: its feed's, with no request, when a page holds one
/// that has listed them; else the list kept under `key` within `LIST_TTL`, failed or not;
/// else `list`'s answer, kept. Scans asking together wait for one list.
async fn held_or_listed(
    key: &str,
    feed: Option<Arc<Mutex<api::FeedState>>>,
    list: impl Future<Output = Result<Vec<Arc<api::ApiApp>>, String>>,
) -> Result<Arc<Vec<LeanApp>>, String> {
    if let Some(feed) = feed {
        // Taken out first: the feed isn't held while they are read.
        let held: Option<Vec<Arc<api::ApiApp>>> = {
            let state = feed.lock().unwrap_or_else(PoisonError::into_inner);
            state.listed.then(|| state.apps.values().cloned().collect())
        };
        if let Some(apps) = held {
            return Ok(Arc::new(leaned(apps.iter())));
        }
    }
    static LISTING: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);
    let kept = || {
        let lists = LISTS.lock().unwrap_or_else(PoisonError::into_inner);
        lists.get(key).filter(|(at, _)| at.elapsed() < LIST_TTL).map(|(_, l)| l.clone())
    };
    let lock = LISTING.lock().unwrap_or_else(PoisonError::into_inner).entry(key.to_string()).or_default().clone();
    let _one = lock.lock().await;
    if let Some(answer) = kept() {
        return answer;
    }
    let answer = list.await.map(|apps| Arc::new(leaned(apps.iter())));
    let mut lists = LISTS.lock().unwrap_or_else(PoisonError::into_inner);
    // Those of an older login or config are never asked again.
    lists.retain(|_, (at, _)| at.elapsed() < LIST_TTL);
    lists.insert(key.to_string(), (Instant::now(), answer.clone()));
    answer
}

/// The Applications of the Argo CD API server `endpoint`, for a scan (see `held_or_listed`).
async fn server_apps(endpoint: &api::Endpoint) -> Result<Arc<Vec<LeanApp>>, String> {
    let list = api::list_once(endpoint.clone(), LIST_DEADLINE);
    held_or_listed(&api::watch_key(endpoint), api::running(endpoint), list).await
}

/// The Argo CD API servers a scan reads: those turned on, when one of them has a login.
/// None: no Argo CD is read by its API, and the hubs are read instead.
fn api_servers(endpoints: Vec<api::Endpoint>) -> Option<Vec<api::Endpoint>> {
    let on: Vec<api::Endpoint> = endpoints.into_iter().filter(|e| e.on).collect();
    on.iter().any(|e| e.login.is_ok()).then_some(on)
}

/// The check of a scan of `context` (`client`: its own; `serves`: it serves Applications): the
/// Applications that deploy there, read from Argo CD's API servers (the argocd CLI's logins
/// and those set in Settings → Argo CD) when one is logged in; else on every hub known (see
/// `argo::problem_hubs`). A server or hub that can't be read is named, and doesn't hide the
/// others.
pub async fn scan(context: String, client: Client, serves: bool, namespaces: Vec<String>) -> Scan {
    let now = jiff::Timestamp::now();
    let targets = argo::targets();
    let here = argo::target(&targets, &context);
    let mut scan = Scan::default();
    if let Some(endpoints) = api_servers(api::endpoints().list) {
        let servers = crate::settings::with(|s| s.argocd_servers.clone());
        let reads = futures::future::join_all(endpoints.iter().map(server_apps)).await;
        for (endpoint, read) in endpoints.iter().zip(reads) {
            let name = endpoint.name();
            match read {
                Ok(apps) => {
                    let home = Home {
                        name,
                        target: argo::server_home(&servers, &endpoint.server.base, &targets),
                        server: Some(endpoint.instance()),
                    };
                    scan.problems.extend(found_on(&apps, &home, &here, &namespaces, now));
                }
                Err(why) => scan.skipped.push((format!("{CHECK} on {name}"), anyhow::anyhow!(why))),
            }
        }
        return scan;
    }
    let hubs = argo::problem_hubs(&context, &client, serves);
    let lists = futures::future::join_all(hubs.iter().map(argo::lean_applications)).await;
    for (hub, list) in hubs.iter().zip(lists) {
        match list {
            Ok(apps) => {
                let home =
                    Home { name: hub.context.clone(), target: argo::target(&targets, &hub.context), server: None };
                scan.problems.extend(found_on(&apps, &home, &here, &namespaces, now));
            }
            // Access to some namespaces of a cluster with Argo CD's CRDs: not an Argo CD of
            // the user's, not a check that failed.
            Err(e) if hub.context == context && k8s::api_status(&e) == Some(403) => {}
            Err(e) => scan.skipped.push((format!("{CHECK} on {}", hub.context), e)),
        }
    }
    scan
}

/// The check as a job of a scan of `context`, whose `kinds` say whether it serves
/// Applications itself.
pub fn job(context: &str, client: &Client, kinds: &[k8s::Kind], namespaces: &[String]) -> super::Job {
    use futures::FutureExt as _;
    let serves = kinds.iter().any(|k| k.is("argoproj.io", "Application"));
    (CHECK.to_string(), scan(context.to_string(), client.clone(), serves, namespaces.to_vec()).boxed())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::*;

    fn app(v: &Value) -> LeanApp {
        serde_json::from_value(v.clone()).unwrap()
    }

    fn ago(secs: i64) -> String {
        jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() - secs).unwrap().to_string()
    }

    /// An Application with auto-sync on or off, the given health, operation and conditions.
    fn of(automated: bool, health: &str, operation: Option<Value>, conditions: &[(&str, &str)]) -> Value {
        let mut v = json!({
            "metadata": {"name": "web", "namespace": "argocd"},
            "spec": {"destination": {"server": "https://kubernetes.default.svc", "namespace": "web"}},
            "status": {
                "sync": {"status": "Synced"},
                "health": {"status": health},
                "conditions": conditions.iter().map(|(t, m)| json!({"type": t, "message": m})).collect::<Vec<_>>(),
            },
        });
        if automated {
            v["spec"]["syncPolicy"] = json!({"automated": {"prune": true}});
        }
        if let Some(op) = operation {
            v["status"]["operationState"] = op;
        }
        v
    }

    /// A rule's case: its name, the Application, and the verdict wanted.
    type Case = (&'static str, Value, Option<(Severity, &'static str)>);

    #[test]
    fn the_rules() {
        let now = jiff::Timestamp::now();
        let failed = Some(json!({"phase": "Failed", "message": "one or more objects failed to apply"}));
        let error = Some(json!({"phase": "Error", "message": "rpc error"}));
        let long = Some(json!({"phase": "Running", "startedAt": ago(25 * 60 + 30)}));
        let short = Some(json!({"phase": "Running", "startedAt": ago(60)}));
        let ok = Some(json!({"phase": "Succeeded"}));
        let cases: Vec<Case> = vec![
            ("healthy", of(true, "Healthy", ok, &[]), None),
            ("degraded", of(false, "Degraded", None, &[]), Some((Severity::Critical, "Degraded"))),
            ("missing, auto-sync on", of(true, "Missing", None, &[]), Some((Severity::Warning, "Missing"))),
            ("missing, synced by hand", of(false, "Missing", None, &[]), None),
            ("progressing", of(true, "Progressing", None, &[]), None),
            ("suspended", of(false, "Suspended", None, &[]), None),
            (
                "failed sync",
                of(false, "Healthy", failed, &[]),
                Some((Severity::Warning, "last sync failed: one or more objects failed to apply")),
            ),
            ("sync error", of(true, "Healthy", error, &[]), Some((Severity::Warning, "last sync failed: rpc error"))),
            (
                "sync running long",
                of(true, "Progressing", long, &[]),
                Some((Severity::Warning, "sync running for 25m")),
            ),
            ("sync just started", of(true, "Progressing", short, &[]), None),
            (
                "comparison error",
                of(false, "Healthy", None, &[("ComparisonError", "repo not found")]),
                Some((Severity::Warning, "ComparisonError: repo not found")),
            ),
            ("a warning condition", of(false, "Healthy", None, &[("SyncWarning", "x")]), None),
            ("an orphaned resource", of(false, "Healthy", None, &[("OrphanedResourceWarning", "x")]), None),
            (
                "degraded and an error",
                of(true, "Degraded", None, &[("InvalidSpecError", "bad\n  path")]),
                Some((Severity::Critical, "Degraded; InvalidSpecError: bad path")),
            ),
        ];
        for (name, v, want) in cases {
            let got = judge(&app(&v), now);
            assert_eq!(got.as_ref().map(|(s, r)| (*s, r.as_str())), want, "{name}");
        }
        // Auto-sync turned off with `enabled: false` is off.
        let mut v = of(true, "Missing", None, &[]);
        v["spec"]["syncPolicy"]["automated"]["enabled"] = json!(false);
        assert_eq!(judge(&app(&v), now), None);
        // Being deleted: the stuck-deletion check says it.
        let mut v = of(true, "Degraded", None, &[]);
        v["metadata"]["deletionTimestamp"] = json!(ago(60));
        assert_eq!(judge(&app(&v), now), None);
        // Not looked at yet.
        assert_eq!(judge(&app(&json!({"metadata": {"name": "new"}, "spec": {}})), now), None);
    }

    #[test]
    fn skipped_entries_name_their_hub() {
        assert_eq!(skipped_hub("Argo CD Applications on hub-1 (failed: did not answer (x))"), Some("hub-1"));
        assert_eq!(skipped_hub("Argo CD Applications on hub-1"), Some("hub-1"));
        assert_eq!(skipped_hub("Pods (forbidden)"), None);
        let on = OnHub { context: "hub".into(), namespace: "argocd".into(), server: None };
        assert_eq!(location(&on, "web"), "Argo CD Application argocd/web on hub");
        let on =
            OnHub { context: "argocd.example.com".into(), server: Some("https://argocd.example.com".into()), ..on };
        assert_eq!(location(&on, "web"), "Argo CD Application argocd/web on Argo CD at argocd.example.com");
        assert_eq!(skipped_hub("Argo CD Applications on argocd.example.com (failed: x)"), Some("argocd.example.com"));
    }

    #[test]
    fn long_messages_are_cut() {
        let long = "x".repeat(500);
        let reason = with_message("Degraded", Some(&long));
        assert_eq!(reason.chars().count(), "Degraded: ".len() + MESSAGE_CHARS);
        assert!(reason.ends_with('…'));
    }

    fn context(name: &str, server: Option<&str>) -> ContextTarget {
        ContextTarget { name: name.into(), server: server.map(String::from), ..Default::default() }
    }

    fn hub(name: &str) -> Home {
        Home { name: name.into(), target: context(name, None), server: None }
    }

    #[test]
    fn rows_go_under_the_cluster_and_namespaces_they_deploy_to() {
        let now = jiff::Timestamp::now();
        let mut elsewhere = of(true, "Degraded", None, &[]);
        elsewhere["metadata"]["name"] = json!("other");
        elsewhere["spec"]["destination"] = json!({"server": "https://other.example:6443", "namespace": "web"});
        let mut api = of(true, "Degraded", None, &[]);
        api["metadata"]["name"] = json!("api");
        api["spec"]["destination"]["namespace"] = json!("api");
        let apps = [app(&of(true, "Degraded", None, &[])), app(&elsewhere), app(&api)];
        let found = found_on(&apps, &hub("hub"), &context("hub", None), &[], now);
        let names: Vec<(&str, Option<&str>)> =
            found.iter().map(|p| (p.name.as_str(), p.namespace.as_deref())).collect();
        assert_eq!(names, [("web", Some("web")), ("api", Some("api"))]);
        assert_eq!(found[0].on_hub, Some(OnHub { context: "hub".into(), namespace: "argocd".into(), server: None }));
        assert_eq!(found[0].label, LABEL);
        assert_eq!(found[0].reason, "Degraded");
        let found = found_on(&apps, &hub("hub"), &context("hub", None), &["api".into()], now);
        assert_eq!(found.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["api"]);
    }

    /// An Application read from an Argo CD API server goes under the context its destination
    /// maps to, says the server, and is kept as on that server: the agents' rules and the
    /// page's link go by it.
    #[test]
    fn rows_read_from_a_server_go_under_their_destination() {
        let now = jiff::Timestamp::now();
        let mut prod = of(true, "Degraded", None, &[]);
        prod["spec"]["destination"] = json!({"server": "https://prod.example:6443", "namespace": "web"});
        let mut by_name = of(false, "Healthy", None, &[("ComparisonError", "repo not found")]);
        by_name["metadata"]["name"] = json!("named");
        by_name["spec"]["destination"] = json!({"name": "prod", "namespace": "web"});
        // `in-cluster`: Argo CD's own cluster, unknown here.
        let mut own = of(true, "Degraded", None, &[]);
        own["metadata"]["name"] = json!("own");
        let apps = [app(&prod), app(&by_name), app(&own)];
        let server = Home {
            name: "argocd.example.com".into(),
            target: ContextTarget::default(),
            server: Some("https://argocd.example.com".into()),
        };
        let here = context("prod", Some("https://prod.example:6443"));
        let found = found_on(&apps, &server, &here, &[], now);
        assert_eq!(found.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["web", "named"]);
        assert_eq!(found[0].reason, "Degraded (Argo CD on argocd.example.com)");
        assert_eq!(
            found[0].on_hub,
            Some(OnHub {
                context: "argocd.example.com".into(),
                namespace: "argocd".into(),
                server: Some("https://argocd.example.com".into()),
            })
        );
        assert!(found_on(&apps, &server, &context("staging", Some("https://staging.example")), &[], now).is_empty());
        // With its cluster known (its hub context's Argo CD server is this one), `in-cluster`
        // is that cluster.
        let home = Home { target: context("hub", Some("https://hub.example")), ..server };
        let found = found_on(&apps, &home, &context("hub", Some("https://hub.example")), &[], now);
        assert_eq!(found.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["own"]);
    }

    fn api_app(v: &Value) -> Arc<api::ApiApp> {
        let value = api::lean(v);
        Arc::new(api::ApiApp { summary: argo::Summary::of(&value), value })
    }

    /// What an Application read by the API keeps is what the rules read: its age, and how
    /// long its sync has run.
    #[test]
    fn the_rules_read_what_the_api_keeps() {
        let now = jiff::Timestamp::now();
        let mut v = of(true, "Progressing", Some(json!({"phase": "Running", "startedAt": ago(25 * 60 + 30)})), &[]);
        v["metadata"]["creationTimestamp"] = json!("2026-01-02T03:04:05Z");
        let apps = leaned([api_app(&v)].iter());
        assert_eq!(judge(&apps[0], now).map(|(_, r)| r).as_deref(), Some("sync running for 25m"));
        assert_eq!(apps[0].metadata.created.as_deref(), Some("2026-01-02T03:04:05Z"));
    }

    /// A feed running (the Argo CD page open) answers a scan with no request; else one list
    /// serves every scan for `LIST_TTL`, failed or not.
    #[test]
    fn a_running_feed_is_read_and_a_list_is_shared() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let asked = AtomicUsize::new(0);
        let degraded = of(true, "Degraded", None, &[]);
        let list = |answer: Result<Vec<Arc<api::ApiApp>>, String>| {
            let asked = &asked;
            async move {
                asked.fetch_add(1, Ordering::SeqCst);
                answer
            }
        };
        let mut state = api::FeedState { listed: true, ..Default::default() };
        state.apps.insert(("argocd".into(), "web".into()), api_app(&degraded));
        let feed = Arc::new(Mutex::new(state));
        let key = "a-running-feed-is-read";
        futures::executor::block_on(async {
            let read = held_or_listed(key, Some(feed.clone()), list(Ok(vec![]))).await.unwrap();
            assert_eq!(read.iter().map(|a| a.metadata.name.as_str()).collect::<Vec<_>>(), ["web"]);
            assert_eq!(asked.load(Ordering::SeqCst), 0, "a running feed is read, not listed");
            // A feed that hasn't listed yet: one list, then kept.
            feed.lock().unwrap().listed = false;
            for _ in 0..3 {
                let read = held_or_listed(key, Some(feed.clone()), list(Ok(vec![api_app(&degraded)]))).await;
                assert_eq!(read.unwrap().len(), 1);
            }
            assert_eq!(asked.load(Ordering::SeqCst), 1);
            // A failure is kept as long, so a down server isn't waited for by every scan.
            let failed = "a-failed-server-is-kept";
            for _ in 0..3 {
                let read = held_or_listed(failed, None, list(Err("did not answer".into()))).await;
                assert_eq!(read.unwrap_err(), "did not answer");
            }
            assert_eq!(asked.load(Ordering::SeqCst), 2);
            // Past the TTL: listed again.
            let old = Instant::now().checked_sub(LIST_TTL + Duration::from_secs(1)).unwrap();
            LISTS.lock().unwrap().get_mut(failed).unwrap().0 = old;
            assert!(held_or_listed(failed, None, list(Ok(vec![]))).await.unwrap().is_empty());
            assert_eq!(asked.load(Ordering::SeqCst), 3);
        });
    }

    /// The servers are read when one is logged in, off ones never; with none logged in, the
    /// hubs are.
    #[test]
    fn servers_are_read_when_one_is_logged_in() {
        let endpoint = |url: &str, on: bool| api::Endpoint {
            server: argo::server::Server::parse(url).unwrap(),
            cli_context: None,
            in_settings: true,
            insecure: false,
            login: Err("no token".into()),
            on,
            settings_token: false,
        };
        assert!(api_servers(vec![]).is_none());
        assert!(api_servers(vec![endpoint("https://a.example", true), endpoint("https://b.example", false)]).is_none());
    }

    /// How auto-sync may be off: no sync policy, none automated, or turned off.
    fn auto_sync_off() -> impl Strategy<Value = Option<Value>> {
        prop::sample::select(vec![
            None,
            Some(json!({})),
            Some(json!({"automated": null})),
            Some(json!({"automated": {"enabled": false, "prune": true}})),
            Some(json!({"syncOptions": ["CreateNamespace=true"]})),
        ])
    }

    proptest! {
        /// Out of sync never counts on an Application with auto-sync off: it is judged as if it
        /// were synced, and one otherwise fine (Missing included) is never a problem.
        #[test]
        fn out_of_sync_by_hand_never_counts(
            policy in auto_sync_off(),
            health in prop::sample::select(vec!["Healthy", "Missing", "Progressing", "Suspended", "Unknown", "Degraded"]),
            phase in prop::option::of(prop::sample::select(vec!["Succeeded", "Running", "Failed", "Error", "Terminating"])),
            started in 0..7200i64,
            condition in prop::option::of(prop::sample::select(vec!["SyncWarning", "OrphanedResourceWarning", "ComparisonError", "SharedResourceWarning"])),
        ) {
            let now = jiff::Timestamp::now();
            let operation = phase.map(|p| json!({"phase": p, "startedAt": ago(started), "message": "m"}));
            let conditions: Vec<(&str, &str)> = condition.iter().map(|c| (*c, "m")).collect();
            let mut v = of(false, health, operation, &conditions);
            if let Some(policy) = policy {
                v["spec"]["syncPolicy"] = policy;
            }
            let synced = judge(&app(&v), now);
            v["status"]["sync"] = json!({"status": "OutOfSync"});
            let out_of_sync = judge(&app(&v), now);
            prop_assert_eq!(&out_of_sync, &synced);
            let failed = matches!(phase, Some("Failed" | "Error"));
            // A second's leeway: the clock moves between `ago` and `now`.
            let stuck = matches!(phase, Some("Running" | "Terminating")) && started + 1 >= argo::diagnosis::STUCK_AFTER;
            let error = condition.is_some_and(|c| c.ends_with("Error"));
            let troubled = health == "Degraded" || failed || stuck || error;
            if !troubled {
                prop_assert_eq!(out_of_sync, None);
            }
        }
    }
}
