//! Alerts firing in a cluster, from its Prometheus (`/api/v1/alerts`, the endpoint found for
//! usage history) and, when one is found the same way, its Alertmanager for silences. Read
//! once per cluster every little while and shared by every page.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use k8s_openapi::api::core::v1::Service;
use kube::{Api, Client, api::ListParams};
use serde_json::Value;

use crate::k8s::{self, Kind};
use crate::problems::{Problem, Scan, Severity};

/// One alert, as Prometheus reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub labels: BTreeMap<String, String>,
    /// Firing; otherwise pending (its condition holds, not for long enough yet).
    pub firing: bool,
    /// "silenced" or "inhibited" in Alertmanager: nobody is told, so it is not a problem.
    pub muted: Option<&'static str>,
    pub summary: String,
    pub description: String,
    /// An http(s) link to what to do about it.
    pub runbook: Option<String>,
    pub since: Option<jiff::Timestamp>,
}

/// Labels naming what an alert is about, most specific first: (label, kubectl kind, kind).
/// `job_name`, not `job`: `job` is the scrape job. Workloads come before `pod`, which on
/// kube-state-metrics' alerts can name the exporter's own pod.
const OBJECT_LABELS: [(&str, &str, &str); 8] = [
    ("deployment", "deployments.apps", "Deployment"),
    ("statefulset", "statefulsets.apps", "StatefulSet"),
    ("daemonset", "daemonsets.apps", "DaemonSet"),
    ("job_name", "jobs.batch", "Job"),
    ("cronjob", "cronjobs.batch", "CronJob"),
    ("persistentvolumeclaim", "persistentvolumeclaims", "PersistentVolumeClaim"),
    ("pod", "pods", "Pod"),
    ("node", "nodes", "Node"),
];

impl Alert {
    fn label(&self, name: &str) -> Option<&str> {
        self.labels.get(name).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn name(&self) -> &str {
        self.label("alertname").unwrap_or("unnamed alert")
    }

    pub fn severity_label(&self) -> &str {
        self.label("severity").unwrap_or_default()
    }

    /// critical, warning, anything else minor.
    pub fn severity(&self) -> Severity {
        severity_of(self.severity_label())
    }

    pub fn namespace(&self) -> Option<&str> {
        self.label("namespace")
    }

    /// What it is about: (kubectl kind, kind, name), from its labels.
    pub fn object(&self) -> Option<(&'static str, &'static str, &str)> {
        OBJECT_LABELS.iter().find_map(|(label, kubectl, kind)| Some((*kubectl, *kind, self.label(label)?)))
    }

    /// Firing and not silenced: someone should look.
    pub fn is_problem(&self) -> bool {
        self.firing && self.muted.is_none()
    }
}

fn severity_of(label: &str) -> Severity {
    match label.to_ascii_lowercase().as_str() {
        "critical" => Severity::Critical,
        "warning" => Severity::Warning,
        _ => Severity::Info,
    }
}

/// The alerts of one cluster, and why they could not be read when Prometheus was found but
/// did not answer.
#[derive(Debug, Default)]
pub struct Alerts {
    pub list: Vec<Alert>,
    /// (HTTP status, message).
    error: Option<(Option<u16>, String)>,
    /// Why Alertmanager could not say which alerts are silenced, when it was found but did
    /// not answer and no recent answer was kept: silenced alerts then show as firing.
    pub silences_error: Option<String>,
}

impl Alerts {
    /// The failure as an error the problem scans can name ("forbidden", "failed: …").
    pub fn error(&self) -> Option<anyhow::Error> {
        let (code, message) = self.error.as_ref()?;
        Some(match code {
            Some(code) => kube::Error::Api(kube::core::Status::failure(message, "").with_code(*code).boxed()).into(),
            None => anyhow::anyhow!("{message}"),
        })
    }

    /// What a page says when silences are unknown.
    pub fn silences_note(&self) -> Option<String> {
        let e = self.silences_error.as_ref()?;
        Some(format!("Could not read silences from Alertmanager ({e}): silenced alerts may show as firing."))
    }
}

/// Alerts with severity `none` are heartbeats (`Watchdog`, `InfoInhibitor`): always firing on
/// purpose.
fn heartbeat(labels: &BTreeMap<String, String>) -> bool {
    labels.get("severity").is_some_and(|s| s.eq_ignore_ascii_case("none"))
}

fn labels_of(v: &Value) -> BTreeMap<String, String> {
    v.as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()
}

/// Prometheus' `/api/v1/alerts` (Thanos and vmalert answer the same); None when the answer
/// is not that.
fn parse_prometheus(text: &str) -> Option<Vec<Alert>> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v["status"] != "success" {
        return None;
    }
    let alerts = v.pointer("/data/alerts")?.as_array()?;
    Some(
        alerts
            .iter()
            .filter_map(|a| {
                let firing = match a["state"].as_str()? {
                    "firing" => true,
                    "pending" => false,
                    _ => return None,
                };
                let labels = labels_of(&a["labels"]);
                if heartbeat(&labels) {
                    return None;
                }
                let note = |keys: &[&str]| {
                    keys.iter()
                        .find_map(|k| a["annotations"][*k].as_str().filter(|s| !s.trim().is_empty()))
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                };
                let runbook = Some(note(&["runbook_url", "runbook"]))
                    .filter(|u| u.starts_with("https://") || u.starts_with("http://"));
                Some(Alert {
                    labels,
                    firing,
                    muted: None,
                    summary: note(&["summary", "message", "description"]),
                    description: note(&["description"]),
                    runbook,
                    since: a["activeAt"].as_str().and_then(|t| t.parse().ok()),
                })
            })
            .collect(),
    )
}

/// An alert Alertmanager holds back: its labels, and "silenced" or "inhibited".
type Held = (BTreeMap<String, String>, &'static str);

/// Alertmanager's alerts, only those it holds back (silenced or inhibited): a big cluster's
/// full list is several MB and takes seconds, the held ones are a fraction of it.
const HELD_PATH: &str = "/api/v2/alerts?active=false&unprocessed=false&silenced=true&inhibited=true";

/// The alerts Alertmanager's `/api/v2/alerts` holds back: (labels, "silenced" or "inhibited").
/// None when the answer is not that list.
fn parse_alertmanager(text: &str) -> Option<Vec<Held>> {
    let Ok(Value::Array(alerts)) = serde_json::from_str::<Value>(text) else { return None };
    let some = |v: &Value| v.as_array().is_some_and(|a| !a.is_empty());
    let held = alerts
        .iter()
        .filter(|a| a.pointer("/status/state").and_then(Value::as_str) == Some("suppressed"))
        .map(|a| {
            let why = if some(&a["status"]["silencedBy"]) || !some(&a["status"]["inhibitedBy"]) {
                "silenced"
            } else {
                "inhibited"
            };
            (labels_of(&a["labels"]), why)
        })
        .collect();
    Some(held)
}

/// Mark the alerts Alertmanager holds back. Alertmanager's copy may carry more labels
/// (Prometheus' external labels), so it matches when it has all of the alert's.
fn mute(alerts: &mut [Alert], held: &[Held]) {
    let mut by_name: HashMap<&str, Vec<&Held>> = HashMap::new();
    for h in held {
        by_name.entry(h.0.get("alertname").map_or("", String::as_str)).or_default().push(h);
    }
    for a in alerts {
        let Some(candidates) = by_name.get(a.labels.get("alertname").map_or("", String::as_str)) else { continue };
        a.muted = candidates
            .iter()
            .find(|(labels, _)| a.labels.iter().all(|(k, v)| labels.get(k) == Some(v)))
            .map(|(_, why)| *why);
    }
}

/// The alert endpoint is not served there (a VictoriaMetrics without vmalert, something
/// else on the port): no alerts to show rather than a failure to report.
fn unsupported(e: &anyhow::Error) -> bool {
    if matches!(k8s::api_status(e), Some(400 | 404 | 405 | 501)) {
        return true;
    }
    let text = format!("{e:#}");
    ["HTTP 400", "HTTP 404", "HTTP 405", "HTTP 501"].iter().any(|s| text.contains(s))
}

/// A request the reader stopped waiting for, by (context, target, path): when it started,
/// and its task, which goes on until `LIMIT`.
type InFlight = HashMap<(String, String, String), (Instant, tokio::task::JoinHandle<Result<String>>)>;

static IN_FLIGHT: LazyLock<Mutex<InFlight>> = LazyLock::new(Default::default);

/// GET `path` from a `namespace/service:port[/prefix]` target through the API server, or
/// from an http(s) URL. Waits `WAIT` at most, but the request goes on until `LIMIT`: a slow
/// answer (a big cluster, an API server busy with the first lists) is then taken by the
/// next read instead of a new request.
async fn get(context: &str, client: Client, target: &str, path: &str) -> Result<String> {
    let key = (context.to_string(), target.to_string(), path.to_string());
    k8s::run(async move {
        let running = IN_FLIGHT.lock().unwrap().remove(&key);
        let (started, mut task) = running.filter(|(started, _)| reuse(started.elapsed())).unwrap_or_else(|| {
            let (_, target, path) = key.clone();
            let task = tokio::spawn(async move {
                tokio::time::timeout(LIMIT, get_now(client, target, path))
                    .await
                    .map_err(|_| anyhow::anyhow!("timed out"))?
            });
            (Instant::now(), task)
        });
        if let Ok(done) = tokio::time::timeout(WAIT, &mut task).await {
            done.map_err(|e| anyhow::anyhow!("task failed: {e}"))?
        } else {
            IN_FLIGHT.lock().unwrap().insert(key, (started, task));
            Err(anyhow::anyhow!("timed out"))
        }
    })
    .await
}

/// Whether a request sent this long ago may still answer a read: it is running (it stops
/// at `LIMIT`) or its answer is recent enough to show.
fn reuse(age: Duration) -> bool {
    age < LIMIT + FRESH
}

async fn get_now(client: Client, target: String, path: String) -> Result<String> {
    match crate::prom::proxy_path(&target)? {
        Some(base) => {
            let req = http::Request::get(format!("{base}{path}")).body(Vec::new())?;
            Ok(client.request_text(req).await?)
        }
        None => crate::assistant::get_url(format!("{}{path}", target.trim_end_matches('/'))).await,
    }
}

/// How long a read waits for an answer: the Problems page waits for it.
const WAIT: Duration = Duration::from_secs(5);

/// How long a request may run, for the next read when the first stopped waiting.
const LIMIT: Duration = Duration::from_secs(30);

/// Alertmanager found per cluster (keyed by context), like `prom::target`.
static ALERTMANAGER: LazyLock<Mutex<HashMap<String, crate::prom::Lookup>>> = LazyLock::new(Default::default);

/// The cluster's Alertmanager, if any (cached), preferring one next to Prometheus.
async fn alertmanager(context: &str, client: Client, prometheus: &str) -> Option<String> {
    let hit = crate::prom::cached(ALERTMANAGER.lock().unwrap().get(context), Instant::now());
    if let Some(found) = hit {
        return found;
    }
    let near = prometheus.split_once('/').map(|(ns, _)| ns.to_string()).filter(|_| !prometheus.contains("://"));
    // A cluster where services can't be listed has none to use, until asked again later.
    let found = k8s::run(find_alertmanager(client, near)).await.ok().flatten();
    ALERTMANAGER.lock().unwrap().insert(context.to_string(), (found.clone(), Instant::now()));
    found
}

/// How likely a service is the Alertmanager API, lower is better; None when it is not one.
fn alertmanager_rank(name: &str, ns: &str, near: Option<&str>) -> Option<(bool, u8)> {
    if !name.contains("alertmanager") {
        return None;
    }
    let rank = if name.ends_with("alertmanager") || name.ends_with("alertmanager-main") {
        0
    } else if name == "alertmanager-operated" {
        1
    } else {
        2
    };
    Some((near != Some(ns), rank))
}

async fn find_alertmanager(client: Client, near: Option<String>) -> Result<Option<String>> {
    let list = Api::<Service>::all(client.clone()).list_metadata(&ListParams::default()).await?;
    let best = list
        .items
        .iter()
        .filter_map(|s| {
            let (name, ns) = (s.metadata.name.as_deref()?, s.metadata.namespace.as_deref()?);
            Some((alertmanager_rank(name, ns, near.as_deref())?, ns, name))
        })
        .min();
    let Some((_, ns, name)) = best else { return Ok(None) };
    let svc = Api::<Service>::namespaced(client, ns).get(name).await?;
    let ports = svc.spec.and_then(|s| s.ports).unwrap_or_default();
    let port = ports
        .iter()
        .find(|p| p.port == 9093)
        .or_else(|| ports.iter().find(|p| p.name.as_deref().is_some_and(|n| n.contains("web"))))
        .or(ports.first())
        .map_or(9093, |p| p.port);
    Ok(Some(format!("{ns}/{name}:{port}")))
}

/// What Alertmanager said about silences.
enum Silences {
    /// No Alertmanager, or not one that answers this API: nothing is held back.
    None,
    Held(Vec<Held>),
    /// It was found but did not answer.
    Failed(String),
}

/// The last silences each cluster's Alertmanager gave (keyed by context), and when.
type LastHeld = HashMap<String, (Instant, Arc<Vec<Held>>)>;

static LAST_HELD: LazyLock<Mutex<LastHeld>> = LazyLock::new(Default::default);

/// How long a cluster's last silences stand in when Alertmanager does not answer: silences
/// last hours, and a few slow reads in a row should not flip silenced alerts to firing.
const HELD_KEPT: Duration = Duration::from_secs(5 * 60);

/// The silences to apply: this answer's, or, when it failed, the last ones read if recent
/// (`last`: how long ago, and what). Why they are unknown otherwise.
fn silences_to_use(read: Silences, last: Option<(Duration, Arc<Vec<Held>>)>) -> Result<Arc<Vec<Held>>, String> {
    match read {
        Silences::None => Ok(Arc::default()),
        Silences::Held(held) => Ok(Arc::new(held)),
        Silences::Failed(e) => match last {
            Some((age, held)) if age < HELD_KEPT => Ok(held),
            _ => Err(e),
        },
    }
}

async fn read_silences(context: &str, client: Client, prometheus: &str) -> Silences {
    let Some(am) = alertmanager(context, client.clone(), prometheus).await else { return Silences::None };
    match get(context, client, &am, HELD_PATH).await {
        // Something else on that service (a webhook, a sign-in page): no silences to read.
        Err(e) if unsupported(&e) => Silences::None,
        Err(e) => Silences::Failed(k8s::short_error(&e)),
        Ok(text) => parse_alertmanager(&text).map_or(Silences::None, Silences::Held),
    }
}

async fn fetch(context: &str, client: Client) -> Alerts {
    // No Prometheus (or metrics off, or services can't be listed): nothing to say.
    let Ok(Some(target)) = crate::prom::target(context, client.clone()).await else { return Alerts::default() };
    let (alerts, silences) = futures::join!(
        get(context, client.clone(), &target, "/api/v1/alerts"),
        read_silences(context, client.clone(), &target)
    );
    let text = match alerts {
        Ok(text) => text,
        Err(e) if unsupported(&e) => return Alerts::default(),
        Err(e) => {
            return Alerts {
                list: Vec::new(),
                error: Some((k8s::api_status(&e), k8s::short_error(&e))),
                silences_error: None,
            };
        }
    };
    let mut list = parse_prometheus(&text).unwrap_or_default();
    let fresh = matches!(silences, Silences::Held(_));
    let last = LAST_HELD.lock().unwrap().get(context).map(|(at, held)| (at.elapsed(), held.clone()));
    let silences_error = match silences_to_use(silences, last) {
        Ok(held) => {
            if fresh {
                LAST_HELD.lock().unwrap().insert(context.to_string(), (Instant::now(), held.clone()));
            }
            mute(&mut list, &held);
            None
        }
        // Shown unmuted, and the page says why.
        Err(e) => Some(e),
    };
    Alerts { list, error: None, silences_error }
}

/// Alerts are read again after this long; pages asking meanwhile share the last answer.
const FRESH: Duration = Duration::from_secs(30);

/// After a failed or incomplete read, the next one tries again this soon, and so can take
/// the answer of a request that was still running.
const RETRY: Duration = Duration::from_secs(5);

/// How long an answer is shared before it is read again.
fn kept_for(alerts: &Alerts) -> Duration {
    if alerts.error.is_some() || alerts.silences_error.is_some() { RETRY } else { FRESH }
}

type Slot = Arc<tokio::sync::Mutex<Option<(Instant, Arc<Alerts>)>>>;

static CACHE: LazyLock<Mutex<HashMap<String, Slot>>> = LazyLock::new(Default::default);

/// The cluster's alerts, firing or pending: one request per cluster every `FRESH` at most,
/// however many pages ask (one waits for another's request instead of sending its own).
pub async fn active(context: &str, client: Client) -> Arc<Alerts> {
    if !crate::settings::metrics_on(context) {
        return Arc::default();
    }
    let slot = CACHE.lock().unwrap().entry(context.to_string()).or_default().clone();
    let mut held = slot.lock().await;
    if let Some((at, alerts)) = held.as_ref()
        && at.elapsed() < kept_for(alerts)
    {
        return alerts.clone();
    }
    let alerts = Arc::new(fetch(context, client).await);
    *held = Some((Instant::now(), alerts.clone()));
    alerts
}

/// Firing alerts nobody silenced, as problems: one per alert name, namespace and object.
/// Only those in `namespaces`, or every one (cluster-wide ones too) when it is empty.
pub fn problems(alerts: &[Alert], namespaces: &[String]) -> Vec<Problem> {
    type Key<'a> = (&'a str, Option<&'a str>, Option<(&'static str, &'static str, &'a str)>);
    // (worst severity, how many, summary, firing since)
    let mut groups: BTreeMap<Key, (Severity, usize, &str, Option<jiff::Timestamp>)> = BTreeMap::new();
    for a in alerts.iter().filter(|a| a.is_problem()) {
        let ns = a.namespace();
        if !namespaces.is_empty() && !ns.is_some_and(|ns| namespaces.iter().any(|n| n == ns)) {
            continue;
        }
        let g = groups.entry((a.name(), ns, a.object())).or_insert((Severity::Info, 0, "", None));
        g.0 = g.0.min(a.severity());
        g.1 += 1;
        if g.2.is_empty() {
            g.2 = &a.summary;
        }
        g.3 = match (g.3, a.since) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        };
    }
    groups
        .into_iter()
        .map(|((alert, ns, object), (severity, count, summary, since))| {
            let (kind, label, name) = match (object, ns) {
                (Some((kubectl, kind, name)), _) => (kubectl, format!("{kind} alert"), name),
                (None, Some(ns)) => ("namespaces", "Namespace alert".into(), ns),
                // Nothing to open: the alert's name stands for it.
                (None, None) => ("", "Alert".into(), alert),
            };
            let mut reason = match (summary.is_empty(), kind.is_empty()) {
                (true, true) => "firing".to_string(),
                (true, false) => alert.to_string(),
                (false, true) => summary.to_string(),
                (false, false) => format!("{alert}: {summary}"),
            };
            if count > 1 {
                reason = format!("{reason} ({count} firing)");
            }
            Problem {
                severity,
                kind: kind.to_string(),
                label,
                namespace: ns.map(String::from),
                name: name.to_string(),
                reason,
                age: since,
            }
        })
        .collect()
}

/// A cluster's problems (see `problems::scan_namespaces`) with its firing alerts in the same
/// namespaces; alerts that could not be read are noted like a kind.
pub async fn scan(context: &str, client: Client, kinds: &[Kind], namespaces: &[String]) -> Scan {
    let (mut scan, alerts) =
        futures::join!(crate::problems::scan_namespaces(client.clone(), kinds, namespaces), active(context, client));
    add(&mut scan, &alerts, namespaces);
    scan
}

/// Add the alerts in `namespaces` (all when empty) to a scan, keeping it worst first.
pub fn add(scan: &mut Scan, alerts: &Alerts, namespaces: &[String]) {
    scan.problems.extend(problems(&alerts.list, namespaces));
    if let Some(e) = alerts.error() {
        scan.skipped.push(("Alerts".into(), e));
    }
    if let Some(e) = &alerts.silences_error {
        scan.skipped.push(("Alertmanager silences".into(), anyhow::anyhow!("{e}")));
    }
    scan.problems
        .sort_by(|a, b| a.severity.cmp(&b.severity).then(a.namespace.cmp(&b.namespace)).then(a.name.cmp(&b.name)));
}

/// What a detail page shows alerts for.
#[derive(Clone, Debug, PartialEq)]
pub enum Focus {
    /// Every alert labeled with this namespace.
    Namespace(String),
    /// Alerts about these objects: (kubectl kind, name), in `namespace` (None: nodes).
    Objects { namespace: Option<String>, objects: Vec<(&'static str, String)> },
}

fn kubectl_kind(kind: &str) -> Option<&'static str> {
    OBJECT_LABELS.iter().find(|(_, _, k)| *k == kind).map(|(_, kubectl, _)| *kubectl)
}

/// The workloads a pod belongs to, from its owners: a Deployment through its ReplicaSet
/// (whose name is the Deployment's and the pod template's hash).
fn pod_owners(pod: &Value) -> Vec<(&'static str, String)> {
    let hash = pod.pointer("/metadata/labels/pod-template-hash").and_then(Value::as_str);
    let mut owners = Vec::new();
    for o in pod.pointer("/metadata/ownerReferences").and_then(Value::as_array).into_iter().flatten() {
        let (Some(kind), Some(name)) = (o["kind"].as_str(), o["name"].as_str()) else { continue };
        if kind == "ReplicaSet" {
            if let Some(deploy) = hash.and_then(|h| name.strip_suffix(&format!("-{h}"))) {
                owners.push(("deployments.apps", deploy.to_string()));
            }
        } else if let Some(kubectl) = kubectl_kind(kind) {
            owners.push((kubectl, name.to_string()));
        }
    }
    owners
}

/// The alerts a page about this object shows: a namespace's, or those about the object,
/// its pods (a workload) or its workloads (a pod). None for other kinds.
pub fn focus(kind: &Kind, obj: &Value, pods: &[Value]) -> Option<Focus> {
    let name = obj.pointer("/metadata/name").and_then(Value::as_str)?.to_string();
    if kind.is("", "Namespace") {
        return Some(Focus::Namespace(name));
    }
    let kubectl = OBJECT_LABELS
        .iter()
        .find(|(_, kubectl, k)| kind.ar.kind == *k && kubectl.split_once('.').map_or("", |(_, g)| g) == kind.ar.group)
        .map(|(_, kubectl, _)| *kubectl)?;
    let namespace = obj.pointer("/metadata/namespace").and_then(Value::as_str).map(String::from);
    let mut objects = vec![(kubectl, name)];
    if kind.is("", "Pod") {
        objects.extend(pod_owners(obj));
    } else if kind.ar.group == "apps" || kind.is("batch", "Job") {
        let names = pods.iter().filter_map(|p| p.pointer("/metadata/name").and_then(Value::as_str));
        objects.extend(names.map(|n| ("pods", n.to_string())));
    }
    Some(Focus::Objects { namespace, objects })
}

/// The alerts a page shows, worst first: firing before pending, silenced last.
pub fn about(alerts: &[Alert], focus: &Focus) -> Vec<Alert> {
    let mut shown: Vec<Alert> = alerts
        .iter()
        .filter(|a| match focus {
            Focus::Namespace(ns) => a.namespace() == Some(ns.as_str()),
            Focus::Objects { namespace, objects } => a.object().is_some_and(|(kubectl, _, name)| {
                (kubectl == "nodes" || a.namespace() == namespace.as_deref())
                    && objects.iter().any(|(k, n)| *k == kubectl && n == name)
            }),
        })
        .cloned()
        .collect();
    let rank = |a: &Alert| (a.muted.is_some(), !a.firing, a.severity());
    shown.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name().cmp(b.name())));
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn alert(labels: &[(&str, &str)]) -> Alert {
        Alert {
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            firing: true,
            muted: None,
            summary: String::new(),
            description: String::new(),
            runbook: None,
            since: None,
        }
    }

    #[test]
    fn reads_prometheus_alerts() {
        let text = r#"{"status":"success","data":{"alerts":[
            {"labels":{"alertname":"KubePodCrashLooping","severity":"warning","namespace":"shop","pod":"web-1","container":"app"},
             "annotations":{"summary":"Pod is crash looping.","runbook_url":"https://runbooks.example/crash"},
             "state":"firing","activeAt":"2026-10-28T10:00:00.123Z","value":"1e+00"},
            {"labels":{"alertname":"Watchdog","severity":"none"},"annotations":{},"state":"firing"},
            {"labels":{"alertname":"Slow","severity":"info"},"annotations":{"message":"old style","runbook_url":"javascript:x"},"state":"pending"}
        ]}}"#;
        let alerts = parse_prometheus(text).unwrap();
        assert_eq!(alerts.len(), 2, "the Watchdog heartbeat is left out");
        let crash = &alerts[0];
        assert_eq!((crash.name(), crash.severity(), crash.firing), ("KubePodCrashLooping", Severity::Warning, true));
        assert_eq!(crash.summary, "Pod is crash looping.");
        assert_eq!(crash.runbook.as_deref(), Some("https://runbooks.example/crash"));
        assert_eq!(crash.since.map(jiff::Timestamp::as_second), Some(1_793_181_600));
        assert_eq!(crash.object(), Some(("pods", "Pod", "web-1")));
        let slow = &alerts[1];
        assert_eq!((slow.firing, slow.severity(), slow.summary.as_str()), (false, Severity::Info, "old style"));
        assert_eq!(slow.runbook, None, "only http(s) links open");
        assert!(parse_prometheus(r#"{"status":"error","error":"missing -vmalert.proxyURL"}"#).is_none());
        assert!(parse_prometheus("<html>sign in</html>").is_none());
    }

    #[test]
    fn severities() {
        assert_eq!(severity_of("critical"), Severity::Critical);
        assert_eq!(severity_of("Critical"), Severity::Critical);
        assert_eq!(severity_of("warning"), Severity::Warning);
        assert_eq!(severity_of("info"), Severity::Info);
        assert_eq!(severity_of(""), Severity::Info);
    }

    #[test]
    fn workloads_before_the_exporters_pod() {
        // kube-state-metrics' alerts can carry its own pod's label next to the workload's.
        let a = alert(&[("alertname", "KubeDeploymentReplicasMismatch"), ("deployment", "web"), ("pod", "ksm-0")]);
        assert_eq!(a.object(), Some(("deployments.apps", "Deployment", "web")));
        // `job` is the scrape job, not a Job.
        let a = alert(&[("alertname", "TargetDown"), ("job", "node-exporter")]);
        assert_eq!(a.object(), None);
        assert_eq!(alert(&[("job_name", "backup-1")]).object(), Some(("jobs.batch", "Job", "backup-1")));
    }

    #[test]
    fn alertmanager_silences() {
        let text = r#"[
            {"labels":{"alertname":"A","pod":"p","prometheus":"monitoring/k8s"},"status":{"state":"suppressed","silencedBy":["s1"],"inhibitedBy":[]}},
            {"labels":{"alertname":"B","pod":"p"},"status":{"state":"suppressed","silencedBy":[],"inhibitedBy":["x"]}},
            {"labels":{"alertname":"C","pod":"p"},"status":{"state":"active","silencedBy":[],"inhibitedBy":[]}}
        ]"#;
        let held = parse_alertmanager(text).unwrap();
        assert_eq!(held.len(), 2);
        let mut alerts = vec![
            alert(&[("alertname", "A"), ("pod", "p")]),
            alert(&[("alertname", "A"), ("pod", "q")]),
            alert(&[("alertname", "B"), ("pod", "p")]),
            alert(&[("alertname", "C"), ("pod", "p")]),
        ];
        mute(&mut alerts, &held);
        let muted: Vec<Option<&str>> = alerts.iter().map(|a| a.muted).collect();
        // Alertmanager's copy has Prometheus' external labels too.
        assert_eq!(muted, [Some("silenced"), None, Some("inhibited"), None]);
        assert!(parse_alertmanager("not json").is_none());
        assert_eq!(parse_alertmanager("[]"), Some(Vec::new()));
    }

    #[test]
    fn missing_endpoints_are_not_failures() {
        let status = |code: u16| -> anyhow::Error {
            kube::Error::Api(kube::core::Status::failure("x", "").with_code(code).boxed()).into()
        };
        assert!(unsupported(&status(404)));
        assert!(unsupported(&status(400)));
        assert!(unsupported(&anyhow::anyhow!("HTTP 404 Not Found")));
        assert!(!unsupported(&status(403)));
        assert!(!unsupported(&status(503)));
        let alerts = Alerts { list: Vec::new(), error: Some((Some(403), "forbidden".into())), silences_error: None };
        assert_eq!(k8s::lists::load_error(&alerts.error().unwrap()), "forbidden");
    }

    #[test]
    fn asks_alertmanager_only_for_held_alerts() {
        // Alertmanager's filters default to true: active and unprocessed are turned off.
        assert!(HELD_PATH.starts_with("/api/v2/alerts?"));
        for param in ["active=false", "unprocessed=false", "silenced=true", "inhibited=true"] {
            assert!(HELD_PATH.contains(param), "{param}");
        }
    }

    #[test]
    fn a_failed_silence_read_keeps_recent_silences_or_says_so() {
        let held: Arc<Vec<Held>> = Arc::new(vec![(BTreeMap::from([("alertname".into(), "A".into())]), "silenced")]);
        let failed = || Silences::Failed("timed out".into());
        let recent = Some((Duration::from_secs(60), held.clone()));
        let old = Some((HELD_KEPT, held.clone()));
        // A new answer wins over the last one, even when it holds nothing back.
        assert_eq!(*silences_to_use(Silences::Held(Vec::new()), recent.clone()).unwrap(), Vec::new());
        assert_eq!(silences_to_use(Silences::None, recent.clone()).unwrap().len(), 0);
        // A failure: the recent silences stand in, too old ones don't, nothing known is unknown.
        assert_eq!(silences_to_use(failed(), recent).unwrap(), held);
        assert_eq!(silences_to_use(failed(), old).unwrap_err(), "timed out");
        assert_eq!(silences_to_use(failed(), None).unwrap_err(), "timed out");
    }

    #[test]
    fn unknown_silences_are_named_and_retried_soon() {
        let list = vec![alert(&[("alertname", "A"), ("namespace", "shop"), ("severity", "warning")])];
        let alerts = Alerts { list, error: None, silences_error: Some("timed out".into()) };
        let mut scan = Scan::default();
        add(&mut scan, &alerts, &[]);
        // Still counted (as before), but the page says the silences are unknown.
        assert_eq!(scan.problems.len(), 1);
        assert_eq!(scan.skipped_text().unwrap(), "Alertmanager silences (failed: timed out)");
        assert!(alerts.silences_note().unwrap().contains("timed out"));
        assert_eq!(kept_for(&alerts), RETRY);
        assert_eq!(kept_for(&Alerts::default()), FRESH);
        assert!(Alerts::default().silences_note().is_none());
    }

    #[test]
    fn late_answers_are_taken_while_recent() {
        assert!(reuse(Duration::ZERO));
        assert!(reuse(LIMIT));
        assert!(!reuse(LIMIT + FRESH));
    }

    #[test]
    fn finds_alertmanager() {
        let rank = |name: &str, ns: &str| alertmanager_rank(name, ns, Some("monitoring"));
        assert_eq!(rank("prometheus-operated", "monitoring"), None);
        assert!(rank("kube-prometheus-stack-alertmanager", "monitoring") < rank("alertmanager-operated", "monitoring"));
        assert!(rank("alertmanager-operated", "monitoring") < rank("alertmanager-operated", "other"));
        assert!(rank("alertmanager-main", "monitoring") < rank("alertmanager-webhook", "monitoring"));
    }

    #[test]
    fn grouped_by_name_namespace_and_object() {
        let mut a = alert(&[("alertname", "KubePodCrashLooping"), ("severity", "warning"), ("namespace", "a")]);
        a.labels.insert("pod".into(), "web-1".into());
        a.summary = "crash looping".into();
        let mut b = a.clone();
        b.labels.insert("container".into(), "sidecar".into());
        b.labels.insert("severity".into(), "critical".into());
        let ns_only = alert(&[("alertname", "Quota"), ("namespace", "b")]);
        let cluster = alert(&[("alertname", "Etcd"), ("severity", "critical")]);
        let mut pending = alert(&[("alertname", "P"), ("namespace", "a")]);
        pending.firing = false;
        let mut silenced = alert(&[("alertname", "S"), ("namespace", "a")]);
        silenced.muted = Some("silenced");
        let all = [a, b, ns_only, cluster, pending, silenced];
        let rows = |ns: &[&str]| -> Vec<(String, String, Option<String>, String, Severity)> {
            let ns: Vec<String> = ns.iter().map(ToString::to_string).collect();
            problems(&all, &ns).into_iter().map(|p| (p.kind, p.name, p.namespace, p.reason, p.severity)).collect()
        };
        assert_eq!(
            rows(&[]),
            [
                (String::new(), "Etcd".into(), None, "firing".into(), Severity::Critical),
                (
                    "pods".into(),
                    "web-1".into(),
                    Some("a".into()),
                    "KubePodCrashLooping: crash looping (2 firing)".into(),
                    Severity::Critical
                ),
                ("namespaces".into(), "b".into(), Some("b".into()), "Quota".into(), Severity::Info),
            ]
        );
        // Namespaces chosen: theirs only, nothing cluster-wide.
        assert_eq!(rows(&["b"]).len(), 1);
        assert_eq!(rows(&["c"]).len(), 0);
    }

    fn named(name: &str, ns: Option<&str>, labels: &[(&str, &str)], owners: Value) -> Value {
        let label_map: serde_json::Map<String, Value> =
            labels.iter().map(|(k, v)| (k.to_string(), Value::from(*v))).collect();
        serde_json::json!({"metadata": {"name": name, "namespace": ns, "labels": label_map, "ownerReferences": owners}})
    }

    fn kind(group: &str, k: &str) -> Kind {
        Kind {
            ar: kube::discovery::ApiResource {
                group: group.into(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: k.into(),
                plural: format!("{}s", k.to_lowercase()),
            },
            namespaced: true,
            section: "",
            label: k.into(),
        }
    }

    #[test]
    fn pages_show_their_objects_alerts() {
        let pod = named(
            "web-7d9f-x2",
            Some("shop"),
            &[("pod-template-hash", "7d9f")],
            serde_json::json!([{"kind": "ReplicaSet", "name": "web-7d9f"}]),
        );
        let page = focus(&kind("", "Pod"), &pod, &[]).unwrap();
        let crash = alert(&[("alertname", "Crash"), ("namespace", "shop"), ("pod", "web-7d9f-x2")]);
        let mismatch = alert(&[("alertname", "Mismatch"), ("namespace", "shop"), ("deployment", "web")]);
        let elsewhere = alert(&[("alertname", "Mismatch"), ("namespace", "other"), ("deployment", "web")]);
        let other_pod = alert(&[("alertname", "Crash"), ("namespace", "shop"), ("pod", "db-0")]);
        let all = [crash, mismatch, elsewhere, other_pod];
        let names = |page: &Focus| -> Vec<(String, String)> {
            about(&all, page)
                .iter()
                .map(|a| (a.name().to_string(), a.object().map(|o| o.2.to_string()).unwrap_or_default()))
                .collect()
        };
        let own = [("Crash".to_string(), "web-7d9f-x2".to_string()), ("Mismatch".to_string(), "web".to_string())];
        assert_eq!(names(&page), own);
        // A Deployment's page: its own alerts and its pods'.
        let deploy = named("web", Some("shop"), &[], Value::Null);
        let page = focus(&kind("apps", "Deployment"), &deploy, std::slice::from_ref(&pod)).unwrap();
        assert_eq!(names(&page), own);
        // A namespace's page: everything labeled with it.
        let ns = named("shop", None, &[], Value::Null);
        let page = focus(&kind("", "Namespace"), &ns, &[]).unwrap();
        assert_eq!(names(&page).len(), 3);
        // Kinds alerts don't name have none.
        assert!(focus(&kind("", "ConfigMap"), &deploy, &[]).is_none());
        // A Deployment of another API group is not the apps one.
        assert!(focus(&kind("example.com", "Deployment"), &deploy, &[]).is_none());
    }

    #[test]
    fn worst_first_silenced_last() {
        let mut silenced = alert(&[("alertname", "A"), ("severity", "critical"), ("namespace", "n")]);
        silenced.muted = Some("silenced");
        let mut pending = alert(&[("alertname", "B"), ("severity", "critical"), ("namespace", "n")]);
        pending.firing = false;
        let warning = alert(&[("alertname", "C"), ("severity", "warning"), ("namespace", "n")]);
        let critical = alert(&[("alertname", "D"), ("severity", "critical"), ("namespace", "n")]);
        let shown = about(&[silenced, pending, warning, critical], &Focus::Namespace("n".into()));
        let names: Vec<&str> = shown.iter().map(Alert::name).collect();
        assert_eq!(names, ["D", "C", "B", "A"]);
    }

    fn any_alert() -> impl Strategy<Value = Alert> {
        let opt = |values: &'static [&'static str]| prop::option::of(prop::sample::select(values));
        (
            prop::sample::select(&["A", "B"][..]),
            opt(&["critical", "warning", "info"]),
            opt(&["a", "b", "c"]),
            opt(&["p1", "p2"]),
            opt(&["d1"]),
            any::<bool>(),
            prop::option::of(prop::sample::select(&["silenced", "inhibited"][..])),
        )
            .prop_map(|(name, severity, ns, pod, deploy, firing, muted)| {
                let mut labels = vec![("alertname", name)];
                labels.extend(severity.map(|s| ("severity", s)));
                labels.extend(ns.map(|n| ("namespace", n)));
                labels.extend(pod.map(|p| ("pod", p)));
                labels.extend(deploy.map(|d| ("deployment", d)));
                let mut a = alert(&labels);
                a.firing = firing;
                a.muted = muted;
                a
            })
    }

    proptest! {
        /// Every row is a firing, unsilenced alert in the namespaces scanned, one per alert
        /// name, namespace and object, as bad as the worst of them.
        #[test]
        fn problems_follow_the_scope(
            alerts in prop::collection::vec(any_alert(), 0..30),
            scope in prop::collection::vec(prop::sample::select(&["a", "b", "c"][..]), 0..3),
        ) {
            let scope: Vec<String> = scope.into_iter().map(String::from).collect();
            let rows = problems(&alerts, &scope);
            let mut keys = std::collections::HashSet::new();
            for p in &rows {
                prop_assert!(keys.insert((p.kind.clone(), p.namespace.clone(), p.name.clone(), p.reason.split(':').next().map(String::from))));
                if !scope.is_empty() {
                    prop_assert!(p.namespace.as_ref().is_some_and(|n| scope.contains(n)));
                }
                let counted: Vec<&Alert> = alerts
                    .iter()
                    .filter(|a| a.is_problem() && a.namespace().map(String::from) == p.namespace)
                    .collect();
                prop_assert!(!counted.is_empty());
                prop_assert!(counted.iter().any(|a| a.severity() == p.severity));
            }
            let in_scope = alerts
                .iter()
                .filter(|a| a.is_problem())
                .filter(|a| scope.is_empty() || a.namespace().is_some_and(|n| scope.iter().any(|s| s == n)))
                .count();
            prop_assert!(rows.len() <= in_scope);
            prop_assert_eq!(rows.is_empty(), in_scope == 0);
        }
    }
}
