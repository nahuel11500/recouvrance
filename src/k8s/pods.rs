//! Pod logs, events and containers.

#[allow(unused_imports)]
use super::*;

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
        let when = |e: &Event| {
            e.last_timestamp
                .as_ref()
                .map(|t| t.0.to_string())
                .or_else(|| e.event_time.as_ref().map(|t| t.0.to_string()))
                .unwrap_or_default()
        };
        items.sort_by_key(|e| std::cmp::Reverse(when(e)));
        Ok(items
            .iter()
            .map(|e| EventLine {
                age: when(e).parse::<jiff::Timestamp>().map(age).unwrap_or_default(),
                kind: e.type_.clone().unwrap_or_default(),
                reason: e.reason.clone().unwrap_or_default(),
                message: e.message.clone().unwrap_or_default(),
                count: e.count.unwrap_or(1),
            })
            .collect())
    })
    .await
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
        None | Some(429) | Some(500..) => true,
        Some(400) => {
            let msg = short_error(e);
            ["waiting to start", "ContainerCreating", "PodInitializing", "not available", "try again"]
                .iter()
                .any(|m| msg.contains(m))
        }
        _ => false,
    }
}

/// After a followed stream ends: whether the pod still runs (its logs may go on).
async fn still_running(api: &Api<Pod>, pod: &str) -> bool {
    match api.get_opt(pod).await {
        Ok(Some(p)) => !matches!(p.status.and_then(|s| s.phase).as_deref(), Some("Succeeded" | "Failed")),
        Ok(None) => false,
        // Can't tell (network): try the stream again.
        Err(_) => true,
    }
}

/// Stream a container's log. When following, a cut stream (API server restart, a
/// connection timeout, a container restarting) reconnects with backoff from the last
/// line seen, until the pod is gone or finished.
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
        let mut last_seen: Option<jiff::Timestamp> = None;
        let mut failures = 0u32;
        loop {
            let lp = stream_params(container.clone(), previous, since_seconds, last_seen);
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
                    // After a reconnect, lines up to the last one seen come again.
                    let out = unseen(out, last_seen);
                    if let Some(t) = out.iter().rev().find_map(|l| line_time(l)) {
                        last_seen = Some(t);
                    }
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
                Ok(false) if previous || !still_running(&api, &pod).await => {
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

/// Lines newer than `seen` (all of them when nothing was seen; lines without a time stay).
fn unseen(lines: Vec<String>, seen: Option<jiff::Timestamp>) -> Vec<String> {
    match seen {
        None => lines,
        Some(seen) => lines.into_iter().filter(|l| line_time(l).is_none_or(|t| t > seen)).collect(),
    }
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
        let got = super::unseen(vec![line(1, "old"), line(2, "seen"), line(3, "new"), "no time".into()], Some(t(2)));
        assert_eq!(got, [line(3, "new"), "no time".to_string()]);
        assert_eq!(super::unseen(vec![line(1, "a")], None).len(), 1);
        // Resuming asks from the whole second of the last line, without a tail.
        let resume: jiff::Timestamp = "2024-01-01T00:00:05.75Z".parse().unwrap();
        let lp = super::stream_params(None, false, Some(60), Some(resume));
        assert_eq!(lp.since_time.map(|t| t.as_second()), Some(resume.as_second()));
        assert_eq!((lp.tail_lines, lp.since_seconds, lp.follow), (None, None, true));
        assert_eq!(super::reconnect_delay(1).as_secs(), 1);
        assert_eq!(super::reconnect_delay(3).as_secs(), 4);
        assert_eq!(super::reconnect_delay(50).as_secs(), 30);
        assert!(super::retryable(&anyhow::anyhow!("connection reset")));
    }
}
