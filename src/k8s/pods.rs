//! Pod logs, events and containers.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use futures::channel::mpsc;
use futures::{AsyncBufReadExt as _, StreamExt as _};
use k8s_openapi::api::core::v1::{Event, Pod};
use kube::Client;
use kube::api::{Api, ListParams, LogParams};

use super::{ObjectRef, TaskGuard, age, api_status, names, run, short_error, spawn_guarded};

pub async fn logs_text(
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    tail_lines: i64,
    since_seconds: Option<i64>,
    previous: bool,
) -> Result<String> {
    run(async move {
        let api: Api<Pod> = Api::namespaced(client, &ns);
        let lp = LogParams {
            container,
            tail_lines: Some(tail_lines),
            since_seconds,
            previous,
            // A few lines of a chatty container can still be huge: never read more than this.
            limit_bytes: Some(LOGS_TEXT_LIMIT),
            ..Default::default()
        };
        Ok(api.logs(&pod, &lp).await?)
    })
    .await
}

/// The most one `logs_text` call reads (8 MiB).
const LOGS_TEXT_LIMIT: i64 = 8 << 20;

/// The longest log line kept; the rest of a longer one is cut (64 KiB).
const MAX_LINE_BYTES: usize = 64 << 10;

/// Marks a line that was cut at `MAX_LINE_BYTES`.
const CUT_MARK: &str = " …[line cut]";

pub struct EventLine {
    /// The Event object's own namespace and name.
    pub namespace: Option<String>,
    pub name: String,
    pub age: String,
    pub kind: String,
    pub reason: String,
    pub message: String,
    pub count: i32,
}

pub async fn events_for(client: Client, obj: ObjectRef) -> Result<Vec<EventLine>> {
    let fields = events_selector(&obj.kind.ar.kind, &obj.name, &obj.uid)?;
    let ns = obj.namespace.clone();
    run(async move {
        let api: Api<Event> = match &ns {
            Some(ns) => Api::namespaced(client, ns),
            None => Api::all(client),
        };
        let lp = ListParams::default().fields(&fields);
        let mut items = api.list(&lp).await?.items;
        items.sort_by_key(|e| std::cmp::Reverse(event_last(e)));
        Ok(items
            .iter()
            .map(|e| EventLine {
                namespace: e.metadata.namespace.clone(),
                name: e.metadata.name.clone().unwrap_or_default(),
                age: event_last(e).map(age).unwrap_or_default(),
                kind: e.type_.clone().unwrap_or_default(),
                reason: e.reason.clone().unwrap_or_default(),
                message: e.message.clone().unwrap_or_default(),
                count: event_count(e),
            })
            .collect())
    })
    .await
}

/// When an event last happened. Events recorded through events.k8s.io (kube-scheduler,
/// for example) keep their repeats in `series`.
fn event_last(e: &Event) -> Option<jiff::Timestamp> {
    e.series
        .as_ref()
        .and_then(|s| s.last_observed_time.as_ref())
        .map(|t| t.0)
        .or_else(|| e.last_timestamp.as_ref().map(|t| t.0))
        .or_else(|| e.event_time.as_ref().map(|t| t.0))
        .or_else(|| e.metadata.creation_timestamp.as_ref().map(|t| t.0))
}

/// How many times an event happened, its `series` included.
fn event_count(e: &Event) -> i32 {
    e.series.as_ref().and_then(|s| s.count).unwrap_or(0).max(e.count.unwrap_or(0)).max(1)
}

/// The field selector for an object's events: by uid when it is known, else by kind
/// and name (pages opened by name have no uid, rows of a fast first load a stand-in).
fn events_selector(kind: &str, name: &str, uid: &str) -> Result<String> {
    if !uid.is_empty() && !uid.contains('/') {
        return Ok(format!("involvedObject.uid={uid}"));
    }
    names::validate_kind_name(kind).map_err(anyhow::Error::msg)?;
    names::validate_name(name).map_err(anyhow::Error::msg)?;
    Ok(format!("involvedObject.kind={kind},involvedObject.name={name}"))
}

/// The pod's containers, its default one (`kubectl.kubernetes.io/default-container`) first.
pub async fn containers(client: Client, ns: String, pod: String) -> Result<Vec<String>> {
    run(async move {
        let api: Api<Pod> = Api::namespaced(client, &ns);
        let pod = api.get(&pod).await?;
        let default = pod.metadata.annotations.as_ref().and_then(|a| a.get("kubectl.kubernetes.io/default-container"));
        let names =
            pod.spec.as_ref().map(|s| s.containers.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
        Ok(default_first(names, default.map(String::as_str)))
    })
    .await
}

/// A listed pod's containers, its default one first (as `containers` reads them).
pub fn pod_containers(pod: &serde_json::Value) -> Vec<String> {
    let names = pod
        .pointer("/spec/containers")
        .and_then(|c| c.as_array())
        .map(|cs| cs.iter().filter_map(|c| c["name"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    let default =
        pod.pointer("/metadata/annotations/kubectl.kubernetes.io~1default-container").and_then(|v| v.as_str());
    default_first(names, default)
}

/// `names` with `default` moved to the front, when it is one of them.
fn default_first(mut names: Vec<String>, default: Option<&str>) -> Vec<String> {
    if let Some(i) = default.and_then(|d| names.iter().position(|n| n == d)) {
        let name = names.remove(i);
        names.insert(0, name);
    }
    names
}

pub enum LogMsg {
    Lines(Vec<String>),
    /// A followed stream was cut (why); it resumes where it stopped after a pause.
    Reconnecting(String),
    End,
    Error(String),
}

/// The request for a stream: from `resume` (the last line's time, after a reconnect),
/// else the chosen window or the last lines.
fn stream_params(
    container: Option<String>,
    previous: bool,
    since_seconds: Option<i64>,
    resume: Option<jiff::Timestamp>,
) -> LogParams {
    let (tail_lines, since_seconds) = match resume {
        Some(_) => (None, None),
        // A time window replaces the tail limit.
        None => (since_seconds.is_none().then_some(crate::settings::get().log_tail_lines), since_seconds),
    };
    LogParams {
        container,
        follow: !previous,
        previous,
        tail_lines,
        since_seconds,
        // Whole seconds (the server's precision): lines of that second come again and are skipped.
        since_time: resume.and_then(|t| jiff::Timestamp::from_second(t.as_second()).ok()),
        // RFC3339 prefix on every line, for the time column and histogram.
        timestamps: true,
        ..Default::default()
    }
}

/// The time a `timestamps=true` line starts with.
fn line_time(line: &str) -> Option<jiff::Timestamp> {
    line.split_once(' ')?.0.parse().ok()
}

/// A followed stream that sends nothing for this long is asked again from its last line.
const LOG_IDLE: std::time::Duration = std::time::Duration::from_secs(120);

/// Wait before reconnecting: 1s, 2s, 4s… up to 30s.
fn reconnect_delay(failures: u32) -> std::time::Duration {
    std::time::Duration::from_secs((1u64 << failures.saturating_sub(1).min(5)).min(30))
}

/// Errors worth retrying: the network, the server, or a container not started yet.
fn retryable(e: &anyhow::Error) -> bool {
    match api_status(e) {
        None | Some(429 | 500..) => true,
        Some(400) => {
            let msg = short_error(e);
            ["waiting to start", "ContainerCreating", "PodInitializing", "not available", "try again"]
                .iter()
                .any(|m| msg.contains(m))
        }
        _ => false,
    }
}

/// After a followed stream ends: whether the container may still log (its logs may go on).
async fn still_running(api: &Api<Pod>, pod: &str, container: Option<&str>) -> bool {
    match api.get_opt(pod).await {
        Ok(Some(p)) => may_log_more(&p, container),
        Ok(None) => false,
        // Can't tell (network): try the stream again.
        Err(_) => true,
    }
}

/// Whether a container may write to its log again: its pod is not finished, and it has not
/// ended for good (an init container that succeeded, an ephemeral container, one its restart
/// policy won't start again). When that can't be told, it may.
fn may_log_more(pod: &Pod, container: Option<&str>) -> bool {
    let status = pod.status.as_ref();
    if matches!(status.and_then(|s| s.phase.as_deref()), Some("Succeeded" | "Failed")) {
        return false;
    }
    let (Some(name), Some(status)) = (container, status) else { return true };
    // 0: an app container, 1: an init container, 2: an ephemeral one.
    let lists = [&status.container_statuses, &status.init_container_statuses, &status.ephemeral_container_statuses];
    let Some((kind, ended)) = lists.iter().enumerate().find_map(|(kind, list)| {
        let found = list.as_ref()?.iter().find(|c| c.name == name)?;
        Some((kind, found.state.as_ref()?.terminated.as_ref()?))
    }) else {
        return true;
    };
    let spec = pod.spec.as_ref();
    let own = spec
        .and_then(|s| match kind {
            0 => s.containers.iter().find(|c| c.name == name),
            1 => s.init_containers.as_ref()?.iter().find(|c| c.name == name),
            _ => None,
        })
        .and_then(|c| c.restart_policy.as_deref());
    let pod_policy = spec.and_then(|s| s.restart_policy.as_deref()).unwrap_or("Always");
    match kind {
        0 => match own.unwrap_or(pod_policy) {
            "Always" => true,
            "OnFailure" => ended.exit_code != 0,
            _ => false,
        },
        // A sidecar starts again; another init container only after it failed.
        1 => own == Some("Always") || (ended.exit_code != 0 && pod_policy != "Never"),
        _ => false,
    }
}

/// Stream a container's log. When following, a cut stream (API server restart, a
/// connection timeout, a container restarting) reconnects with backoff from the last
/// line seen, until the pod is gone or finished or the container has ended for good.
pub fn stream_logs(
    client: Client,
    ns: String,
    pod: String,
    container: Option<String>,
    previous: bool,
    since_seconds: Option<i64>,
) -> (TaskGuard, mpsc::UnboundedReceiver<LogMsg>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let api: Api<Pod> = Api::namespaced(client, &ns);
        let mut shown = Shown::default();
        let mut failures = 0u32;
        loop {
            let lp = stream_params(container.clone(), previous, since_seconds, shown.newest);
            // A reconnect asks from the second of the newest line shown: those lines come again.
            let mut repeats = shown.repeats();
            // Ok(true): the stream went quiet for `LOG_IDLE`.
            let outcome: Result<bool> = async {
                let reader = tokio::time::timeout(LOG_IDLE, api.log_stream(&pod, &lp))
                    .await
                    .map_err(|_| anyhow!("no answer from the API server"))??;
                let mut lines = std::pin::pin!(lossy_lines(reader, MAX_LINE_BYTES).ready_chunks(500));
                loop {
                    let next = if previous {
                        lines.next().await
                    } else {
                        match tokio::time::timeout(LOG_IDLE, lines.next()).await {
                            Ok(next) => next,
                            Err(_) => return Ok(true),
                        }
                    };
                    let Some(chunk) = next else { break };
                    let mut out = Vec::with_capacity(chunk.len());
                    let mut cut = None;
                    for line in chunk {
                        match line {
                            Ok(l) => out.push(l),
                            Err(e) => {
                                cut = Some(e);
                                break;
                            }
                        }
                    }
                    let out = drop_repeats(out, &mut repeats);
                    shown.note(&out);
                    if !out.is_empty() {
                        failures = 0;
                        tx.unbounded_send(LogMsg::Lines(out))?;
                    }
                    if let Some(e) = cut {
                        return Err(e.into());
                    }
                }
                Ok(false)
            }
            .await;
            if tx.is_closed() {
                return;
            }
            // `previous` logs are complete; a stream that cannot come back is over.
            let why = match &outcome {
                // The container may just be quiet, or the connection died without closing
                // (sleep, VPN, a load balancer's idle timeout): ask again from the last line.
                Ok(true) => continue,
                Ok(false) if previous || !still_running(&api, &pod, container.as_deref()).await => {
                    let _ = tx.unbounded_send(LogMsg::End);
                    return;
                }
                Err(e) if previous || !retryable(e) => {
                    let _ = tx.unbounded_send(LogMsg::Error(short_error(e)));
                    return;
                }
                Ok(false) => "stream closed".to_string(),
                Err(e) => short_error(e),
            };
            failures = failures.saturating_add(1);
            if tx.unbounded_send(LogMsg::Reconnecting(why)).is_err() {
                return;
            }
            tokio::time::sleep(reconnect_delay(failures)).await;
        }
    });
    (guard, rx)
}

/// What a followed stream has shown: its newest line's time, and the lines (hashed) since
/// the start of that second, which a reconnect from that second sends again.
#[derive(Default)]
struct Shown {
    newest: Option<jiff::Timestamp>,
    lines: Vec<(jiff::Timestamp, u64)>,
}

impl Shown {
    /// Note lines just shown.
    fn note(&mut self, lines: &[String]) {
        for line in lines {
            let Some(t) = line_time(line) else { continue };
            let second = self.newest.map(jiff::Timestamp::as_second);
            if self.newest.is_none_or(|n| t > n) {
                self.newest = Some(t);
                if second != Some(t.as_second()) {
                    self.lines.retain(|(l, _)| l.as_second() >= t.as_second());
                }
            }
            if self.newest.is_some_and(|n| t.as_second() >= n.as_second()) {
                self.lines.push((t, line_hash(line)));
            }
        }
    }

    /// How many times each line shown comes again after a reconnect.
    fn repeats(&self) -> HashMap<u64, usize> {
        let mut out = HashMap::new();
        for (_, h) in &self.lines {
            *out.entry(*h).or_insert(0) += 1;
        }
        out
    }
}

/// A line's hash, to recognize it when it comes again.
fn line_hash(line: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    line.hash(&mut h);
    h.finish()
}

/// `lines` without the ones already shown before a reconnect. Lines are matched whole
/// (time included), so lines sharing a time, or out of time order (stdout and stderr),
/// are neither lost nor shown twice.
fn drop_repeats(lines: Vec<String>, repeats: &mut HashMap<u64, usize>) -> Vec<String> {
    if repeats.is_empty() {
        return lines;
    }
    lines
        .into_iter()
        .filter(|l| {
            let h = line_hash(l);
            let Some(n) = repeats.get_mut(&h) else { return true };
            *n -= 1;
            if *n == 0 {
                repeats.remove(&h);
            }
            false
        })
        .collect()
}

/// Lines split on `\n`, decoded leniently: a log line with invalid UTF-8 shows
/// replacement characters instead of ending the stream (`lines()` would error).
/// A line longer than `max` bytes is cut there (and marked), not held whole.
fn lossy_lines<R: futures::AsyncBufRead + Unpin>(
    reader: R,
    max: usize,
) -> impl futures::Stream<Item = std::io::Result<String>> {
    futures::stream::unfold(Some((reader, Vec::new())), move |state| async move {
        let (mut reader, mut buf) = state?;
        buf.clear();
        match read_line_capped(&mut reader, &mut buf, max).await {
            Ok(None) => None,
            Ok(Some(cut)) => {
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                let mut line = String::from_utf8_lossy(&buf).into_owned();
                if cut {
                    line.push_str(CUT_MARK);
                }
                Some((Ok(line), Some((reader, buf))))
            }
            // The stream ends after an error.
            Err(e) => Some((Err(e), None)),
        }
    })
}

/// Read one line into `buf` (without its `\n`), keeping at most `max` bytes of it.
/// None at the end of the stream; else whether the line was cut.
async fn read_line_capped<R: futures::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<bool>> {
    let (mut read_any, mut cut) = (false, false);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(read_any.then_some(cut));
        }
        read_any = true;
        let newline = available.iter().position(|b| *b == b'\n');
        let content = &available[..newline.unwrap_or(available.len())];
        let room = max.saturating_sub(buf.len());
        cut |= content.len() > room;
        buf.extend_from_slice(&content[..content.len().min(room)]);
        let used = newline.map_or(available.len(), |i| i + 1);
        reader.consume_unpin(used);
        if newline.is_some() {
            return Ok(Some(cut));
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;

    #[test]
    fn a_container_that_ended_for_good_logs_no_more() {
        let pod = |policy: &str, statuses: serde_json::Value| -> super::Pod {
            let mut pod = serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {"name": "web-1"},
                "spec": {
                    "restartPolicy": policy,
                    "initContainers": [{"name": "migrate"}, {"name": "proxy", "restartPolicy": "Always"}],
                    "containers": [{"name": "app"}],
                },
                "status": {"phase": "Running"},
            });
            pod["status"].as_object_mut().unwrap().extend(statuses.as_object().unwrap().clone());
            serde_json::from_value(pod).unwrap()
        };
        let ended = |name: &str, code: i32| serde_json::json!({"name": name, "image": "", "imageID": "", "ready": false, "restartCount": 0, "state": {"terminated": {"exitCode": code}}});
        let running = |name: &str| serde_json::json!({"name": name, "image": "", "imageID": "", "ready": true, "restartCount": 0, "state": {"running": {}}});
        let init = |s| serde_json::json!({"initContainerStatuses": [s]});
        let app = |s| serde_json::json!({"containerStatuses": [s]});
        // An init container that succeeded is done; one that failed runs again unless Never.
        assert!(!super::may_log_more(&pod("Always", init(ended("migrate", 0))), Some("migrate")));
        assert!(super::may_log_more(&pod("Always", init(ended("migrate", 1))), Some("migrate")));
        assert!(!super::may_log_more(&pod("Never", init(ended("migrate", 1))), Some("migrate")));
        // A sidecar starts again.
        assert!(super::may_log_more(&pod("Never", init(ended("proxy", 0))), Some("proxy")));
        // App containers follow the pod's restart policy.
        assert!(super::may_log_more(&pod("Always", app(ended("app", 0))), Some("app")));
        assert!(!super::may_log_more(&pod("OnFailure", app(ended("app", 0))), Some("app")));
        assert!(super::may_log_more(&pod("OnFailure", app(ended("app", 2))), Some("app")));
        assert!(!super::may_log_more(&pod("Never", app(ended("app", 2))), Some("app")));
        // An ephemeral container never starts again.
        let debug = serde_json::json!({"ephemeralContainerStatuses": [ended("debugger-x", 0)]});
        assert!(!super::may_log_more(&pod("Always", debug), Some("debugger-x")));
        // Running, unknown or not chosen: it may.
        assert!(super::may_log_more(&pod("Never", app(running("app"))), Some("app")));
        assert!(super::may_log_more(&pod("Never", app(running("app"))), Some("other")));
        assert!(super::may_log_more(&pod("Never", app(ended("app", 0))), None));
        // A finished pod logs no more.
        let mut done = pod("Always", app(running("app")));
        done.status.as_mut().unwrap().phase = Some("Succeeded".into());
        assert!(!super::may_log_more(&done, Some("app")));
    }

    fn lines(data: &[u8], max: usize) -> Vec<String> {
        futures::executor::block_on(super::lossy_lines(data, max).map(|l| l.unwrap()).collect::<Vec<_>>())
    }

    #[test]
    fn default_container_first() {
        let names = || vec!["linkerd-proxy".to_string(), "app".to_string()];
        assert_eq!(super::default_first(names(), Some("app")), ["app", "linkerd-proxy"]);
        assert_eq!(super::default_first(names(), None), ["linkerd-proxy", "app"]);
        assert_eq!(super::default_first(names(), Some("gone")), ["linkerd-proxy", "app"]);
        let pod = serde_json::json!({
            "metadata": {"annotations": {"kubectl.kubernetes.io/default-container": "app"}},
            "spec": {"containers": [{"name": "linkerd-proxy"}, {"name": "app"}]},
        });
        assert_eq!(super::pod_containers(&pod), ["app", "linkerd-proxy"]);
        assert!(super::pod_containers(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn event_series_count_and_time() {
        let event = |v: serde_json::Value| serde_json::from_value::<k8s_openapi::api::core::v1::Event>(v).unwrap();
        let e = event(serde_json::json!({
            "metadata": {"name": "web.1", "creationTimestamp": "2024-01-01T00:00:00Z"},
            "involvedObject": {},
            "eventTime": "2024-01-01T00:00:00.000000Z",
            "series": {"count": 300, "lastObservedTime": "2024-01-01T02:00:00.000000Z"},
        }));
        assert_eq!(super::event_count(&e), 300);
        assert_eq!(super::event_last(&e), "2024-01-01T02:00:00Z".parse().ok());
        let e = event(serde_json::json!({
            "metadata": {"name": "web.2"},
            "involvedObject": {},
            "count": 4,
            "lastTimestamp": "2024-01-01T01:00:00Z",
        }));
        assert_eq!(super::event_count(&e), 4);
        assert_eq!(super::event_last(&e), "2024-01-01T01:00:00Z".parse().ok());
        assert_eq!(super::event_count(&event(serde_json::json!({"metadata": {}, "involvedObject": {}}))), 1);
    }

    #[test]
    fn events_by_uid_else_by_name() {
        let uid = "0b6f3c1e-8f7a-4c2e-9d1b-2a3c4d5e6f70";
        assert_eq!(super::events_selector("Pod", "web-1", uid).unwrap(), format!("involvedObject.uid={uid}"));
        for stand_in in ["", "default/web-1", "/node-1"] {
            assert_eq!(
                super::events_selector("Pod", "web-1", stand_in).unwrap(),
                "involvedObject.kind=Pod,involvedObject.name=web-1"
            );
        }
        assert!(super::events_selector("Pod", "x,involvedObject.namespace=y", "").is_err());
    }

    #[test]
    fn invalid_utf8_lines_survive() {
        assert_eq!(lines(b"ok\nbad \xff byte\r\nlast", 1024), ["ok", "bad \u{fffd} byte", "last"]);
        assert_eq!(lines(b"\n\nx\n", 1024), ["", "", "x"]);
        assert!(lines(b"", 1024).is_empty());
    }

    #[test]
    fn long_lines_are_cut() {
        let cut = |s: &str| format!("{s}{}", super::CUT_MARK);
        assert_eq!(lines(b"0123456789abc\nok\n12345678\n", 8), [cut("01234567"), "ok".into(), "12345678".into()]);
        // A line longer than the reader's buffer, without a final newline.
        let long = vec![b'x'; 100_000];
        let got = lines(&long, 64 << 10);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].len(), (64 << 10) + super::CUT_MARK.len());
    }

    #[test]
    fn reconnects_resume_after_the_last_line() {
        let t = |s: i64| jiff::Timestamp::from_second(1_700_000_000 + s).unwrap();
        let line = |s: i64, text: &str| format!("{} {text}", t(s));
        // CRI-O stamps a whole read with one time; stderr can be stamped before stdout.
        let mut shown = super::Shown::default();
        shown.note(&[line(0, "older second"), line(2, "a"), line(2, "b")]);
        shown.note(&[line(2, "b"), line(3, "stdout"), line(2, "stderr")]);
        assert_eq!(shown.newest, Some(t(3)));
        // The reconnect from second 3 sends that second's lines again, then new ones.
        let mut repeats = shown.repeats();
        let got = super::drop_repeats(vec![line(3, "stdout")], &mut repeats);
        assert!(got.is_empty());
        let got = super::drop_repeats(vec![line(3, "new"), line(3, "stdout"), "no time".into()], &mut repeats);
        assert_eq!(got, [line(3, "new"), line(3, "stdout"), "no time".to_string()]);
        assert_eq!(super::drop_repeats(vec![line(1, "a")], &mut Default::default()).len(), 1);
        // Resuming asks from the whole second of the last line, without a tail.
        let resume: jiff::Timestamp = "2024-01-01T00:00:05.75Z".parse().unwrap();
        let lp = super::stream_params(None, false, Some(60), Some(resume));
        assert_eq!(lp.since_time.map(jiff::Timestamp::as_second), Some(resume.as_second()));
        assert_eq!((lp.tail_lines, lp.since_seconds, lp.follow), (None, None, true));
        assert_eq!(super::reconnect_delay(1).as_secs(), 1);
        assert_eq!(super::reconnect_delay(3).as_secs(), 4);
        assert_eq!(super::reconnect_delay(50).as_secs(), 30);
        assert!(super::retryable(&anyhow::anyhow!("connection reset")));
    }
}
