//! A watched node's trouble taints, kept current without watching whole nodes. A node's
//! object is mostly `status.images`, and node-problem-detector and the kubelet rewrite its
//! status every few seconds: on a 50-node cluster, watching whole nodes is about 120 KB/s.
//! So the Nodes list reads whole objects once, then watches their metadata. Its
//! `managedFields` say when a field manager last wrote fields under `spec` (taints,
//! `unschedulable`); when that changes, the one node is read again (`GET`), a moment later so
//! writes within the same second are in. Rows from metadata keep the labels last read. A
//! node whose metadata has no `managedFields` at all (seen on control-plane nodes) can't say
//! when its spec changes: it is read again at most once a minute while its events come.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use kube::Client;
use serde::Deserialize;
use serde_json::value::RawValue;

use super::table::{IsNode, ServerRow, null_default};
use crate::taints::{Taint, labels_of};

/// How long after a spec write is seen the node is read: `managedFields` times are in
/// seconds, so a second write in the same second (a taint added then removed) is in by then.
/// Writes meanwhile make one read.
const SETTLE: Duration = Duration::from_millis(1100);

/// How long before a read that failed is tried again.
const RETRY: Duration = Duration::from_secs(10);

/// How often a node without `managedFields` is read again, at most.
const BLIND_EVERY: Duration = Duration::from_secs(60);

/// The parts of a node read here: its identity, when its spec was last written, and (on a
/// whole object, not on metadata) its taints. Everything else is skipped while parsing.
#[derive(Deserialize, Default)]
#[serde(default)]
struct NodeHead {
    kind: Option<IsNode>,
    metadata: HeadMeta,
    spec: Option<HeadSpec>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct HeadMeta {
    uid: Option<String>,
    name: Option<String>,
    #[serde(deserialize_with = "null_default")]
    managed_fields: Vec<Managed>,
}

/// A `managedFields` entry: when its manager last wrote, and which fields it owns (kept raw:
/// only whether it owns any under `spec` is checked).
#[derive(Deserialize)]
struct Managed {
    #[serde(default)]
    time: Option<String>,
    #[serde(default, rename = "fieldsV1")]
    fields: Option<Box<RawValue>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HeadSpec {
    #[serde(deserialize_with = "null_default")]
    taints: Vec<Taint>,
}

impl HeadMeta {
    /// The last time a manager wrote fields under `spec`. Status writes (the kubelet's,
    /// node-problem-detector's heartbeats) own none there, so they don't change it. Times
    /// are RFC 3339 in UTC to the second, which sort as text.
    fn spec_written(&self) -> Option<String> {
        self.managed_fields
            .iter()
            .filter(|m| m.fields.as_ref().is_some_and(|f| f.get().contains("\"f:spec\"")))
            .filter_map(|m| m.time.clone())
            .max()
    }
}

/// When a node's spec was last written, as its object says (None: not known).
type Written = Option<String>;

/// What is known of one watched node.
#[derive(Debug, Default)]
struct Known {
    name: String,
    /// The latest spec write its events showed.
    written: Written,
    /// The spec write its labels were read at; None until read.
    read: Option<Written>,
    labels: Vec<String>,
    /// When to read it.
    due: Option<Instant>,
    /// A read is in flight (one at a time).
    reading: bool,
    /// When its labels were last read.
    read_at: Option<Instant>,
}

impl Known {
    fn stale(&self) -> bool {
        self.read.as_ref().is_none_or(|r| self.written > *r)
    }

    /// Read it in a moment, unless that is planned already.
    fn plan(&mut self, now: Instant) {
        if self.stale() && !self.reading && self.due.is_none() {
            self.due = Some(now + SETTLE);
        }
    }

    /// Read a node that can't say when its spec changes, a minute after the last read.
    fn plan_blind(&mut self, now: Instant) {
        if !self.reading && self.due.is_none() {
            self.due = Some(self.read_at.map_or(now + SETTLE, |at| (at + BLIND_EVERY).max(now)));
        }
    }
}

/// A node read again: its uid, when its spec was last written, and its taints' labels.
#[derive(Debug, PartialEq)]
pub(super) struct NodeRead {
    uid: String,
    written: Written,
    labels: Vec<String>,
}

/// The nodes of one watch, by uid.
#[derive(Default, Debug)]
pub(super) struct NodeSpecs {
    nodes: HashMap<String, Known>,
}

impl NodeSpecs {
    /// A whole node, listed or in an event: its labels are known as of its spec.
    pub(super) fn whole(&mut self, raw: &str) {
        let Ok(head) = serde_json::from_str::<NodeHead>(raw) else { return };
        self.whole_head(head);
    }

    fn whole_head(&mut self, head: NodeHead) {
        let (Some(spec), Some(uid)) = (head.spec, head.metadata.uid.clone()) else { return };
        let written = head.metadata.spec_written();
        let known = self.nodes.entry(uid).or_default();
        known.name = head.metadata.name.unwrap_or_default();
        known.written = known.written.clone().max(written.clone());
        known.read = Some(written);
        known.labels = labels_of(&spec.taints);
        known.due = None;
        known.read_at = Some(Instant::now());
    }

    /// A node's row from a watch event, before it becomes a row. With its metadata only, the
    /// labels last read go after its Status, and a newer spec write (or a node not seen yet)
    /// plans a read. A whole node (custom columns, a timeline recording whole objects) is
    /// read from itself.
    pub(super) fn event(&mut self, row: &mut ServerRow, now: Instant) {
        let Some(raw) = row.object.as_ref() else { return };
        let Ok(head) = serde_json::from_str::<NodeHead>(raw.get()) else { return };
        if head.kind.is_some_and(|k| k.0) {
            self.whole_head(head);
            return;
        }
        let Some(uid) = head.metadata.uid.clone() else { return };
        let blind = head.metadata.managed_fields.is_empty();
        let written = head.metadata.spec_written();
        let known = self.nodes.entry(uid).or_default();
        if let Some(name) = head.metadata.name {
            known.name = name;
        }
        known.written = known.written.clone().max(written);
        if blind {
            known.plan_blind(now);
        } else {
            known.plan(now);
        }
        if let Some(serde_json::Value::String(status)) = row.cells.get_mut(1) {
            crate::taints::append(status, &known.labels);
        }
    }

    pub(super) fn deleted(&mut self, uid: &str) {
        self.nodes.remove(uid);
    }

    /// When the next read is due.
    pub(super) fn next_due(&self) -> Option<Instant> {
        self.nodes.values().filter(|k| !k.reading).filter_map(|k| k.due).min()
    }

    /// The nodes to read now, as (uid, name): each is then in flight until `read`.
    pub(super) fn take_due(&mut self, now: Instant) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (uid, known) in &mut self.nodes {
            if !known.reading && known.due.is_some_and(|d| d <= now) {
                known.due = None;
                known.reading = true;
                out.push((uid.clone(), known.name.clone()));
            }
        }
        out
    }

    /// A read of node `uid` came back. Its new labels when they changed. A spec write seen
    /// since plans another read; a failed one is tried again later.
    pub(super) fn read(&mut self, uid: &str, read: Result<NodeRead>, now: Instant) -> Option<Vec<String>> {
        let known = self.nodes.get_mut(uid)?;
        known.reading = false;
        known.read_at = Some(now);
        let read = match read {
            // Re-created under its name: the events of the new one will say.
            Ok(read) if read.uid == uid => read,
            Ok(_) => return None,
            Err(_) => {
                known.due = Some(now + RETRY);
                return None;
            }
        };
        known.written = known.written.clone().max(read.written.clone());
        known.read = Some(read.written);
        known.plan(now);
        (known.labels != read.labels).then(|| {
            known.labels = read.labels;
            known.labels.clone()
        })
    }
}

/// Read one node again, leanly (its spec and when it was written, nothing kept).
pub(super) async fn read_node(client: Client, uid: String, name: String) -> (String, Result<NodeRead>) {
    let read = async {
        let req = http::Request::get(format!("/api/v1/nodes/{name}")).body(Vec::new())?;
        let head: NodeHead = super::lists::request_idle(&client, req).await?;
        parse_read(head)
    };
    (uid, read.await)
}

fn parse_read(head: NodeHead) -> Result<NodeRead> {
    let spec = head.spec.ok_or_else(|| anyhow!("no spec"))?;
    Ok(NodeRead {
        written: head.metadata.spec_written(),
        uid: head.metadata.uid.ok_or_else(|| anyhow!("no uid"))?,
        labels: labels_of(&spec.taints),
    })
}

#[cfg(test)]
mod tests {
    use super::{BLIND_EVERY, NodeHead, NodeRead, NodeSpecs, RETRY, SETTLE, ServerRow, parse_read};
    use std::time::{Duration, Instant};

    /// `managedFields` with the kubelet's and node-problem-detector's status writes at
    /// `status`, and the Cilium operator's write of the taints at `spec`.
    fn managed(status: &str, spec: &str) -> serde_json::Value {
        serde_json::json!([
            {"manager": "kubelet", "operation": "Update", "subresource": "status", "time": status,
                "fieldsV1": {"f:status": {"f:conditions": {}}}},
            {"manager": "node-problem-detector", "operation": "Update", "subresource": "status", "time": status,
                "fieldsV1": {"f:status": {"f:conditions": {}}}},
            {"manager": "cilium-operator-generic", "operation": "Update", "time": spec,
                "fieldsV1": {"f:spec": {"f:taints": {}}}},
        ])
    }

    /// A node's row as the metadata watch sends it.
    fn meta_row(status: &str, spec: &str) -> ServerRow {
        let row = serde_json::json!({"cells": ["n1", "Ready"], "object": {"kind": "PartialObjectMetadata",
            "metadata": {"name": "n1", "uid": "u1", "managedFields": managed(status, spec)}}});
        serde_json::from_str(&row.to_string()).unwrap()
    }

    fn whole(spec: &str, taints: serde_json::Value) -> String {
        serde_json::json!({"kind": "Node", "apiVersion": "v1",
            "metadata": {"name": "n1", "uid": "u1", "managedFields": managed("2026-10-30T10:00:00Z", spec)},
            "spec": {"taints": taints}, "status": {"images": []}})
        .to_string()
    }

    fn cilium() -> serde_json::Value {
        serde_json::json!([{"key": "node.cilium.io/agent-not-ready", "effect": "NoSchedule"}])
    }

    fn status(row: &ServerRow) -> &str {
        row.cells[1].as_str().unwrap()
    }

    fn read(spec: &str, labels: &[&str]) -> anyhow::Result<NodeRead> {
        Ok(NodeRead {
            uid: "u1".into(),
            written: Some(spec.into()),
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
        })
    }

    #[test]
    fn status_writes_read_nothing_and_keep_the_labels() {
        let mut specs = NodeSpecs::default();
        specs.whole(&whole("2026-10-30T09:00:00Z", cilium()));
        let now = Instant::now();
        for second in 10..40 {
            // node-problem-detector's heartbeats, every ten seconds.
            let mut row = meta_row(&format!("2026-10-30T10:00:{second}Z"), "2026-10-30T09:00:00Z");
            specs.event(&mut row, now);
            assert_eq!(status(&row), "Ready,CiliumAgentNotReady");
        }
        assert_eq!(specs.next_due(), None);
        assert!(specs.take_due(now + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn a_spec_write_reads_the_node_once() {
        let mut specs = NodeSpecs::default();
        specs.whole(&whole("2026-10-30T09:00:00Z", cilium()));
        let now = Instant::now();
        // The operator removes the taint: its entry's time moves.
        let mut row = meta_row("2026-10-30T10:00:00Z", "2026-10-30T10:00:01Z");
        specs.event(&mut row, now);
        // Until read, the row says what was last known.
        assert_eq!(status(&row), "Ready,CiliumAgentNotReady");
        assert_eq!(specs.next_due(), Some(now + SETTLE));
        assert!(specs.take_due(now).is_empty(), "not before the writes of that second are in");
        // More events meanwhile, a second write too: still one read.
        specs.event(&mut meta_row("2026-10-30T10:00:02Z", "2026-10-30T10:00:01Z"), now);
        specs.event(&mut meta_row("2026-10-30T10:00:02Z", "2026-10-30T10:00:02Z"), now);
        let later = now + SETTLE;
        assert_eq!(specs.take_due(later), [("u1".to_string(), "n1".to_string())]);
        // In flight: no other read of it, whatever comes.
        specs.event(&mut meta_row("2026-10-30T10:00:03Z", "2026-10-30T10:00:02Z"), later);
        assert_eq!(specs.next_due(), None);
        assert!(specs.take_due(later + SETTLE).is_empty());
        assert_eq!(specs.read("u1", read("2026-10-30T10:00:02Z", &[]), later), Some(vec![]));
        assert_eq!(specs.next_due(), None);
        // From now on, the rows say what was read.
        let mut row = meta_row("2026-10-30T10:00:20Z", "2026-10-30T10:00:02Z");
        specs.event(&mut row, later);
        assert_eq!(status(&row), "Ready");
    }

    #[test]
    fn a_write_during_a_read_reads_again() {
        let mut specs = NodeSpecs::default();
        specs.whole(&whole("2026-10-30T09:00:00Z", serde_json::json!([])));
        let now = Instant::now();
        specs.event(&mut meta_row("2026-10-30T10:00:00Z", "2026-10-30T10:00:05Z"), now);
        assert_eq!(specs.take_due(now + SETTLE).len(), 1);
        // Another write lands while the read is out; the read saw the one before.
        specs.event(&mut meta_row("2026-10-30T10:00:00Z", "2026-10-30T10:00:09Z"), now + SETTLE);
        let labels = specs.read("u1", read("2026-10-30T10:00:05Z", &["CiliumAgentNotReady"]), now + SETTLE);
        assert_eq!(labels, Some(vec!["CiliumAgentNotReady".to_string()]));
        assert_eq!(specs.next_due(), Some(now + SETTLE + SETTLE));
        // The same labels again: nothing to change.
        assert_eq!(specs.take_due(now + SETTLE * 2).len(), 1);
        assert_eq!(specs.read("u1", read("2026-10-30T10:00:09Z", &["CiliumAgentNotReady"]), now), None);
        assert_eq!(specs.next_due(), None);
    }

    #[test]
    fn new_nodes_are_read_and_failures_retried() {
        let mut specs = NodeSpecs::default();
        let now = Instant::now();
        // Added after the list: its taints are not known yet.
        let mut row = meta_row("2026-10-30T10:00:00Z", "2026-10-30T10:00:00Z");
        specs.event(&mut row, now);
        assert_eq!(status(&row), "Ready");
        assert_eq!(specs.take_due(now + SETTLE).len(), 1);
        assert_eq!(specs.read("u1", Err(anyhow::anyhow!("timeout")), now), None);
        assert_eq!(specs.next_due(), Some(now + RETRY));
        assert_eq!(specs.take_due(now + RETRY).len(), 1);
        // Another node under that name now: its own events will say.
        let other = NodeRead { uid: "u2".into(), written: None, labels: vec!["X".into()] };
        assert_eq!(specs.read("u1", Ok(other), now), None);
        // Deleted: forgotten, and a late read changes nothing.
        specs.deleted("u1");
        assert_eq!(specs.read("u1", read("2026-10-30T10:00:00Z", &["X"]), now), None);
        assert_eq!(specs.next_due(), None);
    }

    #[test]
    fn nodes_without_managed_fields_are_read_once_a_minute() {
        let mut specs = NodeSpecs::default();
        specs.whole(r#"{"kind": "Node", "metadata": {"name": "n1", "uid": "u1"}, "spec": {}}"#);
        let blind = || -> ServerRow {
            serde_json::from_str(r#"{"cells": ["n1", "Ready"], "object": {"metadata": {"name": "n1", "uid": "u1"}}}"#)
                .unwrap()
        };
        let now = Instant::now();
        specs.event(&mut blind(), now);
        let due = specs.next_due().unwrap();
        assert!(due >= now + BLIND_EVERY - Duration::from_secs(5), "a minute after the list");
        for i in 0..6 {
            specs.event(&mut blind(), now + Duration::from_secs(i * 10));
        }
        assert_eq!(specs.take_due(due).len(), 1, "once, however many events");
        specs.event(&mut blind(), due);
        assert_eq!(
            specs.read("u1", read("x", &["CiliumAgentNotReady"]), due),
            Some(vec!["CiliumAgentNotReady".into()])
        );
        specs.event(&mut blind(), due + SETTLE);
        assert_eq!(specs.next_due(), Some(due + BLIND_EVERY));
    }

    #[test]
    fn whole_nodes_in_events_are_read_from_themselves() {
        let mut specs = NodeSpecs::default();
        let row = serde_json::json!({"cells": ["n1", "Ready"], "object": serde_json::from_str::<serde_json::Value>(
            &whole("2026-10-30T10:00:00Z", cilium())).unwrap()});
        let mut row: ServerRow = serde_json::from_str(&row.to_string()).unwrap();
        specs.event(&mut row, Instant::now());
        // `to_row` adds them for a whole node.
        assert_eq!(status(&row), "Ready");
        assert_eq!(specs.next_due(), None);
        let mut meta = meta_row("2026-10-30T10:00:10Z", "2026-10-30T10:00:00Z");
        specs.event(&mut meta, Instant::now());
        assert_eq!(status(&meta), "Ready,CiliumAgentNotReady");
    }

    #[test]
    fn reads_are_lean() {
        let head: NodeHead = serde_json::from_str(&whole("2026-10-30T10:00:00Z", cilium())).unwrap();
        let read = parse_read(head).unwrap();
        assert_eq!(read.labels, ["CiliumAgentNotReady"]);
        assert_eq!(read.written.as_deref(), Some("2026-10-30T10:00:00Z"));
        // A node no manager wrote the spec of.
        let bare = r#"{"kind": "Node", "metadata": {"uid": "u1"}, "spec": {}}"#;
        let read = parse_read(serde_json::from_str(bare).unwrap()).unwrap();
        assert_eq!((read.written, read.labels), (None, Vec::<String>::new()));
    }
}
