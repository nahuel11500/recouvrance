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
use crate::problems::{Problem, Severity};

/// One alert, as Prometheus reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
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
/// `job_name`, not `job`: `job` is the scrape job. Workloads, HPAs and Ingresses come before
/// `pod`, which on kube-state-metrics' alerts can name the exporter's own pod. `service` comes
/// last: every series scraped through a Service carries its name (kube-state-metrics' own on a
/// crash-looping pod's alert), so it names the object only when nothing else does
/// (`TargetDown`), and only with the scrape's `job` (see `Alert::object`).
const OBJECT_LABELS: [(&str, &str, &str); 11] = [
    ("deployment", "deployments.apps", "Deployment"),
    ("statefulset", "statefulsets.apps", "StatefulSet"),
    ("daemonset", "daemonsets.apps", "DaemonSet"),
    ("job_name", "jobs.batch", "Job"),
    ("cronjob", "cronjobs.batch", "CronJob"),
    ("horizontalpodautoscaler", "horizontalpodautoscalers.autoscaling", "HorizontalPodAutoscaler"),
    ("ingress", "ingresses.networking.k8s.io", "Ingress"),
    ("persistentvolumeclaim", "persistentvolumeclaims", "PersistentVolumeClaim"),
    ("pod", "pods", "Pod"),
    ("node", "nodes", "Node"),
    ("service", "services", "Service"),
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
        OBJECT_LABELS.iter().find_map(|(label, kubectl, kind)| {
            // Many rules set a fixed `service` label to route their alerts (a pager service, a
            // team): it names a Service only on a scrape target's series, which carry `job`.
            if *label == "service" && self.label("job").is_none() {
                return None;
            }
            Some((*kubectl, *kind, self.label(label)?))
        })
    }

    /// The namespace of what it is about. ingress-nginx's series carry the controller's
    /// `namespace`, and the Ingress's as `exported_namespace`.
    pub fn object_namespace(&self) -> Option<&str> {
        match self.object() {
            Some((_, "Ingress", _)) => self.label("exported_namespace").or_else(|| self.namespace()),
            _ => self.namespace(),
        }
    }

    /// The host of its `instance` label, without the port: a node's name or address for
    /// exporters on the host network (node-exporter), whose alerts are about that node.
    fn instance_host(&self) -> Option<&str> {
        let instance = self.label("instance")?;
        let host = match instance.strip_prefix('[') {
            // [IPv6]:port
            Some(rest) => rest.split_once(']').map_or(rest, |(h, _)| h),
            None => match instance.rsplit_once(':') {
                // A bare IPv6 address has several colons and no port.
                Some((h, port)) if !h.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => h,
                _ => instance,
            },
        };
        Some(host).filter(|h| !h.is_empty())
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
pub(crate) fn unsupported(e: &anyhow::Error) -> bool {
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
                tokio::time::timeout(LIMIT, crate::prom::get_text(client, &target, path))
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
    // Refused (401/403) stays refused: asking again soon would not help.
    let refused = matches!(alerts.error, Some((Some(401 | 403), _)));
    if !refused && (alerts.error.is_some() || alerts.silences_error.is_some()) { RETRY } else { FRESH }
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
        let ns = a.object_namespace();
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
                on_argo: None,
            }
        })
        .collect()
}

/// What a Problems scan notes it could not read of the alerts, like a kind: the alerts
/// themselves, or which of them are silenced.
pub fn skipped(alerts: &Alerts) -> Vec<(String, anyhow::Error)> {
    let mut out = Vec::new();
    if let Some(e) = alerts.error() {
        out.push(("Alerts".into(), e));
    }
    if let Some(e) = &alerts.silences_error {
        out.push(("Alertmanager silences".into(), anyhow::anyhow!("{e}")));
    }
    out
}

/// What a detail page shows alerts for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    /// Every alert labeled with this namespace.
    Namespace(String),
    /// Alerts about these objects: (kubectl kind, name), in `namespace` (None: nodes). On a
    /// node's page, `hosts` are its name and addresses: alerts whose `instance` is one of them
    /// are about the node too (node-exporter's, which name the exporter's pod).
    Objects { namespace: Option<String>, objects: Vec<(&'static str, String)>, hosts: Vec<String> },
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
    let mut hosts = Vec::new();
    if kind.is("", "Node") {
        hosts.push(name.clone());
        let addresses = obj.pointer("/status/addresses").and_then(Value::as_array).into_iter().flatten();
        hosts.extend(addresses.filter_map(|a| a["address"].as_str()).map(String::from));
        hosts.sort();
        hosts.dedup();
    }
    let mut objects = vec![(kubectl, name)];
    if kind.is("", "Pod") {
        objects.extend(pod_owners(obj));
    } else if kind.ar.group == "apps" || kind.is("batch", "Job") {
        let names = pods.iter().filter_map(|p| p.pointer("/metadata/name").and_then(Value::as_str));
        objects.extend(names.map(|n| ("pods", n.to_string())));
    }
    Some(Focus::Objects { namespace, objects, hosts })
}

/// The alerts a page shows, worst first: firing before pending, silenced last.
pub fn about(alerts: &[Alert], focus: &Focus) -> Vec<Alert> {
    let mut shown: Vec<Alert> = alerts
        .iter()
        .filter(|a| match focus {
            Focus::Namespace(ns) => a.namespace() == Some(ns.as_str()),
            Focus::Objects { namespace, objects, hosts } => {
                a.object().is_some_and(|(kubectl, _, name)| {
                    (kubectl == "nodes" || a.object_namespace() == namespace.as_deref())
                        && objects.iter().any(|(k, n)| *k == kubectl && n == name)
                }) || a.instance_host().is_some_and(|h| hosts.iter().any(|n| n == h))
            }
        })
        .cloned()
        .collect();
    let rank = |a: &Alert| (a.muted.is_some(), !a.firing, a.severity());
    shown.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name().cmp(b.name())));
    shown
}

/// An alerting rule, as Prometheus' `/api/v1/rules` gives it (vmalert answers the same).
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub name: String,
    /// Its `PromQL` expression.
    pub query: String,
    /// How long the expression must hold before the alert fires, in seconds.
    pub for_secs: u64,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    /// Why its last evaluation failed, when it did: the alert may be stale.
    pub error: Option<String>,
}

/// The alerting rules, without their alerts (Prometheus 2.49+; older ones send them too).
const RULES_PATH: &str = "/api/v1/rules?type=alert&exclude_alerts=true";

/// The alerting rules of a `/api/v1/rules` answer; None when the answer is not that.
fn parse_rules(text: &str) -> Option<Vec<Rule>> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v["status"] != "success" {
        return None;
    }
    let groups = v.pointer("/data/groups")?.as_array()?;
    let rules = groups.iter().flat_map(|g| g["rules"].as_array().into_iter().flatten());
    Some(
        rules
            .filter(|r| r["type"].as_str().is_none_or(|t| t == "alerting"))
            .filter_map(|r| {
                let error = r["lastError"].as_str().map(str::trim).filter(|e| !e.is_empty()).map(String::from);
                Some(Rule {
                    name: r["name"].as_str()?.to_string(),
                    query: r["query"].as_str()?.trim().to_string(),
                    // Seconds: a float in Prometheus, an integer in vmalert.
                    for_secs: r["duration"].as_f64().filter(|d| d.is_finite() && *d > 0.0).map_or(0, |d| d as u64),
                    labels: labels_of(&r["labels"]),
                    annotations: labels_of(&r["annotations"]),
                    error,
                })
            })
            .collect(),
    )
}

/// The rule an alert comes from: its name, and the rule's labels on the alert (a rule's
/// labels are added to every alert it fires). Several rules can share a name (one per
/// severity): the one with the most labels matching. Templated values (`{{ $value }}`) can't
/// be compared and are left out.
pub fn rule_for<'a>(rules: &'a [Rule], alert: &Alert) -> Option<&'a Rule> {
    rules
        .iter()
        .filter(|r| r.name == alert.name())
        .filter_map(|r| {
            let fixed = r.labels.iter().filter(|(_, v)| !v.contains("{{"));
            let mut matched = 0;
            for (k, v) in fixed {
                if alert.labels.get(k) != Some(v) {
                    return None;
                }
                matched += 1;
            }
            Some((matched, r))
        })
        // `max_by_key` keeps the last of the best: reversed, that is the first.
        .rev()
        .max_by_key(|(matched, _)| *matched)
        .map(|(_, r)| r)
}

/// The rules read per (context, Prometheus): they change with a deploy, not every minute.
type RulesCache = HashMap<(String, String), (Instant, Arc<Vec<Rule>>)>;

static RULES: LazyLock<Mutex<RulesCache>> = LazyLock::new(Default::default);

/// How long rules read are used before they are read again.
const RULES_FRESH: Duration = Duration::from_secs(300);

/// The cluster's alerting rules, read at most every `RULES_FRESH`, or why they could not be.
async fn rules(context: &str, client: Client, target: &str) -> Result<Arc<Vec<Rule>>, String> {
    let key = (context.to_string(), target.to_string());
    let kept = RULES.lock().unwrap().get(&key).filter(|(at, _)| at.elapsed() < RULES_FRESH).map(|(_, r)| r.clone());
    if let Some(rules) = kept {
        return Ok(rules);
    }
    let text = match get(context, client, target, RULES_PATH).await {
        Ok(text) => text,
        Err(e) if unsupported(&e) => return Err("this Prometheus doesn't serve its rules".into()),
        Err(e) => return Err(k8s::short_error(&e)),
    };
    let rules = Arc::new(parse_rules(&text).ok_or("the rules API gave something else")?);
    RULES.lock().unwrap().insert(key, (Instant::now(), rules.clone()));
    Ok(rules)
}

/// What a click on an alert shows: its rule and a chart of it.
#[derive(Clone, Debug)]
pub struct Detail {
    /// None: no rule of that name and labels (deleted since, or another Prometheus's).
    pub rule: Option<Rule>,
    pub chart: Option<Chart>,
}

/// The rule's expression over the last `CHART_MINUTES`, for this alert's series.
#[derive(Clone, Debug)]
pub struct Chart {
    /// What is drawn: the left side of the comparison, or the whole expression.
    pub expr: String,
    /// How `expr` was cut from the rule's expression.
    pub cut: Cut,
    /// The comparison and its number ("> 0.9"), drawn as a line.
    pub threshold: Option<(String, f64)>,
    /// The alert's labels the series are kept to; empty when every series is drawn.
    pub matched_on: Vec<String>,
    /// The points, or why they could not be read.
    pub series: Result<crate::prom::Series, String>,
}

/// How far back the chart goes.
pub const CHART_MINUTES: i64 = 180;

/// An alert's rule and its chart: one read of the rules (kept a few minutes), then one range
/// query, both only on a click.
pub async fn detail(context: String, client: Client, alert: Alert) -> Result<Detail, String> {
    let target = crate::prom::target(&context, client.clone())
        .await
        .map_err(|e| k8s::short_error(&e))?
        .ok_or("no Prometheus found")?;
    let rules = rules(&context, client.clone(), &target).await?;
    let Some(rule) = rule_for(&rules, &alert).cloned() else { return Ok(Detail { rule: None, chart: None }) };
    let (expr, cut) = chart_expr(&rule.query);
    let query = series_query(&expr, &alert, &rule);
    let matched_on = own_labels(&alert, &rule).into_iter().map(|(k, _)| k.clone()).collect();
    let step = (CHART_MINUTES * 60 / 120).max(15);
    let series = crate::prom::range(client, target, query, CHART_MINUTES, step).await.map_err(|e| k8s::short_error(&e));
    let threshold = cut.threshold().map(|(op, n)| (op.to_string(), n));
    let chart = Chart { expr, cut, threshold, matched_on, series };
    Ok(Detail { rule: Some(rule), chart: Some(chart) })
}

/// Set operators: they bind looser than comparisons.
const SET_OPS: [&str; 3] = ["and", "or", "unless"];

/// Comparison operators, longest first so `>=` is not read as `>`.
const COMPARISONS: [&str; 6] = ["==", "!=", ">=", "<=", ">", "<"];

/// The byte ranges of an expression's top level: what is outside parentheses, brackets,
/// braces and strings (comments are taken out before). Calls `f(at, rest)` at each such
/// position; `f` returns how many bytes it took (0 for one character), or None to stop.
fn top_level(expr: &str, mut f: impl FnMut(usize, &str) -> Option<usize>) {
    let bytes = expr.as_bytes();
    let (mut i, mut depth, mut quote): (usize, i32, Option<u8>) = (0, 0, None);
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' && q != b'`' {
                i += 1;
            } else if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' | b'`' => quote = Some(b),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0 => match f(i, &expr[i..]) {
                None => return,
                Some(0) => {}
                Some(n) => {
                    i += n;
                    continue;
                }
            },
            _ => {}
        }
        // Multi-byte characters only appear inside strings and comments, or as one whole
        // character here: step over all of it.
        i += expr[i..].chars().next().map_or(1, char::len_utf8);
    }
}

/// The expression without parentheses around all of it.
fn strip_parens(mut expr: &str) -> &str {
    loop {
        expr = expr.trim();
        let Some(inner) = expr.strip_prefix('(').and_then(|e| e.strip_suffix(')')) else { return expr };
        // `(a) > (b)` starts and ends with them, but they are not one pair: something of it
        // is at the top level.
        let mut one_pair = true;
        top_level(expr, |_, _| {
            one_pair = false;
            None
        });
        if !one_pair {
            return expr;
        }
        expr = inner;
    }
}

/// A number, as a threshold is written (`0.9`, `1e9`, `(5)`, `bool 0`).
fn number(side: &str) -> Option<f64> {
    let side = strip_parens(side);
    let side = side.strip_prefix("bool").filter(|s| s.starts_with(char::is_whitespace)).unwrap_or(side);
    strip_parens(side).parse::<f64>().ok().filter(|n| n.is_finite())
}

/// How a rule's expression was cut to be charted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cut {
    /// The comparison taken out, and its number when the other side is one.
    pub comparison: Option<(&'static str, Option<f64>)>,
    /// The set operator (`and`, `or`, `unless`) the expression was cut at.
    pub set_op: Option<&'static str>,
}

impl Cut {
    /// The comparison and its number, when the other side is one: a line on the chart.
    pub fn threshold(self) -> Option<(&'static str, f64)> {
        let (op, n) = self.comparison?;
        Some((op, n?))
    }
}

/// What a chart of a rule draws: the left side of its comparison (the comparison keeps only
/// the points past the threshold, so the whole expression draws nothing below it), and the
/// threshold when the other side is a number. `a > 1 and b` is cut at `and` first, which
/// binds looser. The whole expression when there is no comparison (`absent(…)`).
pub fn chart_expr(query: &str) -> (String, Cut) {
    let clean = without_comments(query);
    let (expr, cut) = split_comparison(&clean);
    (expr.to_string(), cut)
}

/// The expression without its `# …` comments (outside strings), which run to the line's end.
fn without_comments(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let (mut quote, mut escaped, mut comment) = (None, false, false);
    for c in query.chars() {
        if comment {
            if c != '\n' {
                continue;
            }
            comment = false;
        } else if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' && q != '`' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if matches!(c, '"' | '\'' | '`') {
            quote = Some(c);
        } else if c == '#' {
            comment = true;
            continue;
        }
        out.push(c);
    }
    out
}

/// `chart_expr` on an expression without comments.
fn split_comparison(query: &str) -> (&str, Cut) {
    let query = strip_parens(query);
    let mut end = query.len();
    let mut comparison: Option<(usize, &'static str)> = None;
    top_level(query, |at, rest| {
        let word = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':')).unwrap_or(rest.len());
        if word > 0 {
            if SET_OPS.contains(&&rest[..word]) {
                end = at;
                return None;
            }
            return Some(word);
        }
        if comparison.is_none()
            && let Some(op) = COMPARISONS.iter().find(|op| rest.starts_with(**op))
        {
            comparison = Some((at, *op));
            return Some(op.len());
        }
        Some(0)
    });
    let set_op =
        (end < query.len()).then(|| SET_OPS.into_iter().find(|w| query[end..].starts_with(w)).unwrap_or("and"));
    let Some((at, op)) = comparison.filter(|(at, _)| *at < end) else {
        let part = strip_parens(&query[..end]);
        // `(a > 1) and b`: the comparison is inside the parentheses taken off; look again.
        if part.len() < query.len() {
            let (expr, inner) = split_comparison(part);
            return (expr, Cut { comparison: inner.comparison, set_op: set_op.or(inner.set_op) });
        }
        return (part, Cut { comparison: None, set_op });
    };
    let (left, right) = (strip_parens(&query[..at]), strip_parens(&query[at + op.len()..end]));
    let (expr, comparison) = match (number(left), number(right)) {
        // `0.9 < x`: the number on the left, so x is drawn against it, the comparison turned.
        (Some(n), None) => {
            let turned = match op {
                ">" => "<",
                "<" => ">",
                ">=" => "<=",
                "<=" => ">=",
                same => same,
            };
            (right, (turned, Some(n)))
        }
        (_, n) => (left, (op, n)),
    };
    (expr, Cut { comparison: Some(comparison), set_op })
}

/// A `PromQL` string literal holding `value`.
fn string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A label name `PromQL` takes as it is.
fn plain_label(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Labels the alert source adds to every alert, never on the expression's series: vmalert's
/// `alertgroup` (unless it runs with `-disableAlertgroupLabel`).
const SOURCE_LABELS: [&str; 2] = ["alertname", "alertgroup"];

/// The alert's labels that come from its series: without the rule's (added to the alert,
/// not on the series) and the alert source's.
fn own_labels<'a>(alert: &'a Alert, rule: &Rule) -> Vec<(&'a String, &'a String)> {
    alert
        .labels
        .iter()
        .filter(|(k, _)| !SOURCE_LABELS.contains(&k.as_str()) && !rule.labels.contains_key(*k) && plain_label(k))
        .collect()
}

/// `expr` kept to the alert's own series: its `own_labels`, matched with `and on(…)` against
/// a one-sample vector that carries them. Without such labels, `expr` itself.
fn series_query(expr: &str, alert: &Alert, rule: &Rule) -> String {
    let own = own_labels(alert, rule);
    if own.is_empty() {
        return expr.to_string();
    }
    // `label_replace` reads `$1` in its replacement: `$$` is a plain `$`.
    let one = own.iter().fold("vector(1)".to_string(), |v, (k, val)| {
        format!(r#"label_replace({v}, "{k}", {}, "", "")"#, string_literal(&val.replace('$', "$$")))
    });
    let names = own.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ");
    format!("({expr}) and on({names}) {one}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problems::Scan;
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
        let scan = Scan { problems: problems(&alerts.list, &[]), skipped: skipped(&alerts) };
        // Still counted (as before), but the page says the silences are unknown.
        assert_eq!(scan.problems.len(), 1);
        assert_eq!(scan.skipped_text().unwrap(), "Alertmanager silences (failed: timed out)");
        assert!(alerts.silences_note().unwrap().contains("timed out"));
        assert_eq!(kept_for(&alerts), RETRY);
        assert_eq!(kept_for(&Alerts::default()), FRESH);
        let forbidden = Alerts { list: Vec::new(), error: Some((Some(403), "forbidden".into())), silences_error: None };
        assert_eq!(kept_for(&forbidden), FRESH);
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
        Kind::fake(group, k)
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
                .map(|a| (a.name().to_string(), a.object().map_or_default(|o| o.2.to_string())))
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

    #[test]
    fn more_labels_open_their_object() {
        // kube-state-metrics' alerts carry its own pod (and Service): the HPA is what it is about.
        let hpa = alert(&[("alertname", "KubeHpaMaxedOut"), ("horizontalpodautoscaler", "web"), ("pod", "ksm-0")]);
        assert_eq!(hpa.object(), Some(("horizontalpodautoscalers.autoscaling", "HorizontalPodAutoscaler", "web")));
        let ing = alert(&[("ingress", "shop"), ("pod", "ksm-0"), ("service", "ksm")]);
        assert_eq!(ing.object(), Some(("ingresses.networking.k8s.io", "Ingress", "shop")));
        // A crash-looping pod's alert names kube-state-metrics' Service too: the pod wins.
        let crash = alert(&[("alertname", "KubePodCrashLooping"), ("pod", "web-1"), ("service", "ksm")]);
        assert_eq!(crash.object(), Some(("pods", "Pod", "web-1")));
        // Nothing else named: the Service whose targets are down.
        let down = alert(&[("alertname", "TargetDown"), ("job", "api"), ("namespace", "shop"), ("service", "api")]);
        assert_eq!(down.object(), Some(("services", "Service", "api")));
        // A Service's page shows it.
        let svc = named("api", Some("shop"), &[], Value::Null);
        let page = focus(&kind("", "Service"), &svc, &[]).unwrap();
        assert_eq!(about(std::slice::from_ref(&down), &page).len(), 1);
        let ing_page = focus(&kind("networking.k8s.io", "Ingress"), &svc, &[]).unwrap();
        assert!(about(&[down], &ing_page).is_empty());
    }

    #[test]
    fn a_rules_service_label_is_not_a_service() {
        // A fixed `service` label set by the rule to route the alert, on a namespace's alert.
        let routed = alert(&[("alertname", "QuotaAlmostFull"), ("namespace", "shop"), ("service", "payments")]);
        assert_eq!(routed.object(), None);
        let rows = problems(&[routed], &[]);
        assert_eq!((rows[0].kind.as_str(), rows[0].name.as_str()), ("namespaces", "shop"));
    }

    #[test]
    fn ingress_nginx_alerts_open_the_ingress_in_its_namespace() {
        // ingress-nginx's series: the controller's namespace, the Ingress's as `exported_namespace`.
        let a = alert(&[
            ("alertname", "IngressErrors"),
            ("ingress", "shop"),
            ("namespace", "ingress-nginx"),
            ("exported_namespace", "web"),
        ]);
        let rows = problems(std::slice::from_ref(&a), &[]);
        assert_eq!((rows[0].namespace.as_deref(), rows[0].name.as_str()), (Some("web"), "shop"));
        let ing = named("shop", Some("web"), &[], Value::Null);
        let page = focus(&kind("networking.k8s.io", "Ingress"), &ing, &[]).unwrap();
        assert_eq!(about(&[a], &page).len(), 1);
        // kube-state-metrics' series carry the Ingress's own namespace.
        let ksm = alert(&[("ingress", "shop"), ("namespace", "web")]);
        assert_eq!(ksm.object_namespace(), Some("web"));
    }

    #[test]
    fn a_nodes_page_shows_the_alerts_of_its_exporters() {
        let node = serde_json::json!({"metadata": {"name": "worker-1"}, "status": {"addresses": [
            {"type": "InternalIP", "address": "10.0.0.7"}, {"type": "Hostname", "address": "worker-1"}
        ]}});
        let page = focus(&kind("", "Node"), &node, &[]).unwrap();
        // node-exporter runs on the host network: its instance is the node's address, its pod
        // label the exporter's.
        let by_ip =
            alert(&[("alertname", "NodeFilesystemAlmostOutOfSpace"), ("instance", "10.0.0.7:9100"), ("pod", "ne-x")]);
        let by_name = alert(&[("alertname", "NodeClockNotSynchronising"), ("instance", "worker-1:9100")]);
        let other = alert(&[("alertname", "NodeFilesystemAlmostOutOfSpace"), ("instance", "10.0.0.70:9100")]);
        let pod_ip = alert(&[("alertname", "KubePodCrashLooping"), ("instance", "10.0.0.71:8080"), ("pod", "web-1")]);
        let shown = about(&[by_ip, by_name, other, pod_ip], &page);
        let names: Vec<&str> = shown.iter().map(Alert::name).collect();
        assert_eq!(names, ["NodeClockNotSynchronising", "NodeFilesystemAlmostOutOfSpace"]);
        // Other pages don't match by instance.
        let pod = named("ne-y", Some("monitoring"), &[], Value::Null);
        let page = focus(&kind("", "Pod"), &pod, &[]).unwrap();
        assert!(about(&[alert(&[("instance", "10.0.0.7:9100")])], &page).is_empty());
    }

    #[test]
    fn instance_hosts() {
        let host = |i: &str| alert(&[("instance", i)]).instance_host().map(String::from);
        assert_eq!(host("10.0.0.7:9100").as_deref(), Some("10.0.0.7"));
        assert_eq!(host("worker-1").as_deref(), Some("worker-1"));
        assert_eq!(host("[fd00::7]:9100").as_deref(), Some("fd00::7"));
        assert_eq!(host("fd00::7").as_deref(), Some("fd00::7"));
        assert_eq!(host(":9100"), None);
        assert_eq!(alert(&[]).instance_host(), None);
    }

    #[test]
    fn reads_alerting_rules() {
        let text = r#"{"status":"success","data":{"groups":[{"name":"k8s","rules":[
            {"type":"alerting","name":"KubePodCrashLooping","query":"max_over_time(x[5m]) >= 1","duration":900,
             "labels":{"severity":"warning"},"annotations":{"summary":"Pod is crash looping."},"health":"ok","lastError":""},
            {"type":"recording","name":"job:x:sum","query":"sum(x)"},
            {"type":"alerting","name":"Broken","query":"y > 1","duration":0,"health":"err","lastError":"many-to-many matching"}
        ]},{"name":"vm","rules":[
            {"type":"alerting","name":"KubePodCrashLooping","query":"z > 0","duration":60.5,"labels":{"severity":"critical"}}
        ]}]}}"#;
        let rules = parse_rules(text).unwrap();
        assert_eq!(rules.len(), 3, "recording rules are left out");
        assert_eq!((rules[0].for_secs, rules[0].error.as_deref()), (900, None));
        assert_eq!(rules[0].annotations["summary"], "Pod is crash looping.");
        assert_eq!((rules[1].for_secs, rules[1].error.as_deref()), (0, Some("many-to-many matching")));
        assert_eq!(rules[2].for_secs, 60);
        assert!(parse_rules(r#"{"status":"error","error":"x"}"#).is_none());
        assert!(parse_rules("<html>").is_none());
        // The rule of the same name whose labels the alert carries.
        let critical = alert(&[("alertname", "KubePodCrashLooping"), ("severity", "critical"), ("pod", "p")]);
        assert_eq!(rule_for(&rules, &critical).map(|r| r.query.as_str()), Some("z > 0"));
        let warning = alert(&[("alertname", "KubePodCrashLooping"), ("severity", "warning")]);
        assert_eq!(rule_for(&rules, &warning).map(|r| r.query.as_str()), Some("max_over_time(x[5m]) >= 1"));
        assert!(rule_for(&rules, &alert(&[("alertname", "KubePodCrashLooping"), ("severity", "info")])).is_none());
        assert!(rule_for(&rules, &alert(&[("alertname", "Other")])).is_none());
        // A templated label can't be compared: it doesn't keep the rule from matching.
        let mut templated = rules[1].clone();
        templated.labels.insert("severity".into(), "{{ $labels.level }}".into());
        assert!(rule_for(&[templated], &alert(&[("alertname", "Broken"), ("severity", "warning")])).is_some());
    }

    #[test]
    fn charts_the_left_side_of_the_comparison() {
        // What is drawn and the threshold.
        fn split_comparison(q: &str) -> (&str, Option<(&'static str, f64)>) {
            let (expr, cut) = super::split_comparison(q);
            (expr, cut.threshold())
        }
        fn chart_expr(q: &str) -> (String, Option<(&'static str, f64)>) {
            let (expr, cut) = super::chart_expr(q);
            (expr, cut.threshold())
        }
        assert_eq!(split_comparison("max_over_time(x[5m]) >= 1"), ("max_over_time(x[5m])", Some((">=", 1.0))));
        // Matchers, strings and subqueries inside are not comparisons.
        assert_eq!(
            split_comparison(r#"rate(x{code=~"5..",path!="/a>b"}[5m:1m]) / rate(x[5m]) > 0.05"#),
            (r#"rate(x{code=~"5..",path!="/a>b"}[5m:1m]) / rate(x[5m])"#, Some((">", 0.05)))
        );
        // `and` binds looser than the comparison; the other side isn't a number.
        assert_eq!(
            split_comparison("(a - b) / c > on(pod) group_left d\nand\nchanges(e[1h]) > 0"),
            ("(a - b) / c", None)
        );
        assert_eq!(split_comparison("kube_band > bool (5)"), ("kube_band", Some((">", 5.0))));
        // The number on the left: the comparison is turned.
        assert_eq!(split_comparison("0.9 < free / size"), ("free / size", Some((">", 0.9))));
        // Parentheses around everything, comments.
        assert_eq!(chart_expr("(up == 0) # down, (x > 1)\n"), ("up".to_string(), Some(("==", 0.0))));
        assert_eq!(chart_expr("x{a=\"#\"} > 1 # y"), ("x{a=\"#\"}".to_string(), Some((">", 1.0))));
        assert_eq!(split_comparison("(a) > (b)"), ("a", None));
        assert_eq!(split_comparison("absent(up{job=\"x\"})"), ("absent(up{job=\"x\"})", None));
        assert_eq!(split_comparison("a unless b"), ("a", None));
        // A comparison inside the parentheses of the part kept before `and`/`unless`.
        assert_eq!(split_comparison("(a > 1) and b"), ("a", Some((">", 1.0))));
        assert_eq!(split_comparison("(rate(x[5m]) > 0.1) and on(pod) y"), ("rate(x[5m])", Some((">", 0.1))));
        assert_eq!(split_comparison("(a / b < 0.1) unless z"), ("a / b", Some(("<", 0.1))));
    }

    #[test]
    fn says_how_the_expression_was_cut() {
        let cut = |q| chart_expr(q).1;
        assert_eq!(cut("a > 1"), Cut { comparison: Some((">", Some(1.0))), set_op: None });
        assert_eq!(cut("a > b"), Cut { comparison: Some((">", None)), set_op: None });
        // Only comments and parentheses taken off: nothing was cut.
        assert_eq!(cut("(absent(x)) # note"), Cut::default());
        assert_eq!(cut("a unless b"), Cut { comparison: None, set_op: Some("unless") });
        assert_eq!(cut("a or b > 1"), Cut { comparison: None, set_op: Some("or") });
        assert_eq!(cut("(a > 1) and b"), Cut { comparison: Some((">", Some(1.0))), set_op: Some("and") });
    }

    #[test]
    fn the_chart_keeps_the_alerts_own_series() {
        let rule = Rule {
            name: "A".into(),
            query: "x > 1".into(),
            for_secs: 0,
            labels: BTreeMap::from([("severity".into(), "warning".into())]),
            annotations: BTreeMap::new(),
            error: None,
        };
        let a = alert(&[("alertname", "A"), ("severity", "warning"), ("pod", r#"we"b$1"#), ("namespace", "shop")]);
        assert_eq!(
            series_query("x", &a, &rule),
            r#"(x) and on(namespace, pod) label_replace(label_replace(vector(1), "namespace", "shop", "", ""), "pod", "we\"b$$1", "", "")"#
        );
        // Only the rule's labels: the expression as it is.
        assert_eq!(series_query("x", &alert(&[("alertname", "A"), ("severity", "warning")]), &rule), "x");
        // vmalert adds `alertgroup` to every alert: no series carries it.
        let vm = alert(&[("alertname", "A"), ("alertgroup", "k8s"), ("severity", "warning"), ("pod", "p")]);
        assert_eq!(series_query("x", &vm, &rule), r#"(x) and on(pod) label_replace(vector(1), "pod", "p", "", "")"#);
        assert_eq!(own_labels(&vm, &rule).len(), 1);
    }

    /// What a `PromQL` double-quoted string literal holds, or None when it is not one.
    fn read_literal(text: &str) -> Option<String> {
        let inner = text.strip_prefix('"')?.strip_suffix('"')?;
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return None,
                '\\' => out.push(match chars.next()? {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    c @ ('\\' | '"') => c,
                    _ => return None,
                }),
                c => out.push(c),
            }
        }
        Some(out)
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

        /// Any label value goes into the chart's query as a string literal holding exactly it.
        #[test]
        fn label_values_are_quoted(value in any::<String>()) {
            prop_assert_eq!(read_literal(&string_literal(&value)), Some(value));
        }

        /// The chart draws the comparison's left side, whatever its matchers, strings,
        /// parentheses and comments hold, against the number on the right.
        #[test]
        fn the_left_side_of_any_comparison(
            name in "m_[a-z_]{0,6}",
            value in r#"[a-z0-9 <>=!()#\[\]{}'`]{0,8}"#,
            shape in 0..4usize,
            op in prop::sample::select(&COMPARISONS[..]),
            n in -1e6..1e6f64,
            comment in prop::option::of("[a-z <>=()]{0,8}"),
        ) {
            let sel = format!("{{l={}}}", string_literal(&value));
            let left = match shape {
                0 => format!("{name}{sel}"),
                1 => format!("rate({name}{sel}[5m])"),
                2 => format!("sum by (l) ({name}{sel}) / {name}"),
                _ => format!("({name}{sel} - {name}) * 100"),
            };
            let comment = comment.map_or_default(|c| format!(" # {c}\n"));
            let (expr, cut) = chart_expr(&format!("{left} {op} {n}{comment}"));
            prop_assert_eq!(expr, left);
            prop_assert_eq!(cut.threshold(), Some((op, n)));
        }
    }
}
