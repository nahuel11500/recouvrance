//! Taints that keep pods off a node because something is wrong with it (its network agent
//! isn't ready, the cloud hasn't set it up, the autoscaler is removing it), named the way
//! kubectl names a node's state (`CiliumAgentNotReady`), for its Status column, page and the
//! Problems page. The taints people set on purpose to dedicate a node (`production=true`,
//! `NodeGpu`, `node-role.kubernetes.io/control-plane`) are not trouble and never show.

use serde::Deserialize;

/// A node's taint, as much of it as the label needs (`value` is skipped while parsing).
#[derive(Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Taint {
    pub key: String,
    pub effect: String,
}

/// Taints with a fixed name: the cloud provider's, Cilium's, the autoscalers'. The other
/// `node.kubernetes.io/*` ones are named from their key (`out-of-service` -> `OutOfService`).
const KNOWN: [(&str, &str); 8] = [
    // As the node condition is spelled.
    ("node.kubernetes.io/pid-pressure", "PIDPressure"),
    ("node.cilium.io/agent-not-ready", "CiliumAgentNotReady"),
    ("node.cloudprovider.kubernetes.io/uninitialized", "Uninitialized"),
    ("node.cloudprovider.kubernetes.io/shutdown", "Shutdown"),
    ("ToBeDeletedByClusterAutoscaler", "ToBeDeleted"),
    ("DeletionCandidateOfClusterAutoscaler", "DeletionCandidate"),
    ("karpenter.sh/disrupted", "Disrupted"),
    ("karpenter.sh/unregistered", "Unregistered"),
];

/// Already in kubectl's Status: `NotReady`/`Unknown` from the Ready condition, and
/// `SchedulingDisabled` for a cordoned node.
const SHOWN_BY_KUBECTL: [&str; 3] =
    ["node.kubernetes.io/not-ready", "node.kubernetes.io/unreachable", "node.kubernetes.io/unschedulable"];

/// The words of kubectl's own node Status.
const KUBECTL_WORDS: [&str; 4] = ["Ready", "NotReady", "Unknown", "SchedulingDisabled"];

/// Words in a taint key's name that say something is wrong (`ebs.csi.aws.com/agent-not-ready`),
/// matched without separators or case.
const TROUBLE_WORDS: [&str; 5] = ["notready", "unavailable", "uninitialized", "unreachable", "pressure"];

/// The label of a taint that keeps pods off the node because something is wrong, or None
/// for a taint set on purpose, or one that only prefers other nodes.
pub fn trouble_label(key: &str, effect: &str) -> Option<String> {
    let keeps_off = matches!(effect, "NoSchedule" | "NoExecute");
    // The autoscaler only steers pods away from a node it may remove.
    let prefers = effect == "PreferNoSchedule" && key == "DeletionCandidateOfClusterAutoscaler";
    if !(keeps_off || prefers) || SHOWN_BY_KUBECTL.contains(&key) || key.starts_with("node-role.") {
        return None;
    }
    if let Some((_, label)) = KNOWN.iter().find(|(k, _)| *k == key) {
        return Some((*label).to_string());
    }
    let (domain, name) = key.rsplit_once('/').unwrap_or(("", key));
    let flat: String = name.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    if domain != "node.kubernetes.io" && !TROUBLE_WORDS.iter().any(|w| flat.contains(w)) {
        return None;
    }
    let label = camel(name);
    if label.is_empty() {
        return None;
    }
    // A word kubectl's Status means something else by (`example.com/not-ready`): with
    // its domain's first part, or "Taint".
    Some(if KUBECTL_WORDS.contains(&label.as_str()) {
        let prefix = camel(domain.split('.').next().unwrap_or_default());
        format!("{}{label}", if prefix.is_empty() { "Taint" } else { prefix.as_str() })
    } else {
        label
    })
}

/// The labels of a node's trouble taints, each once, in the taints' order.
pub fn trouble_labels<'a>(taints: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for (key, effect) in taints {
        if let Some(label) = trouble_label(key, effect).filter(|l| !labels.contains(l)) {
            labels.push(label);
        }
    }
    labels
}

/// `trouble_labels` of parsed taints.
pub fn labels_of(taints: &[Taint]) -> Vec<String> {
    trouble_labels(taints.iter().map(|t| (t.key.as_str(), t.effect.as_str())))
}

/// `labels` after a node's kubectl Status, comma-separated as kubectl joins its words
/// (`Ready,SchedulingDisabled,CiliumAgentNotReady`).
pub fn append(status: &mut String, labels: &[String]) {
    for label in labels {
        if !status.is_empty() {
            status.push(',');
        }
        status.push_str(label);
    }
}

/// A node's Status with its labels replaced: kubectl's words, then `labels`.
pub fn relabel(status: &str, labels: &[String]) -> String {
    let mut out = status.split(',').filter(|w| KUBECTL_WORDS.contains(w)).collect::<Vec<_>>().join(",");
    append(&mut out, labels);
    out
}

/// A node's Status as kubectl prints it, from its Ready condition's status (None without
/// one) and `spec.unschedulable`, with its trouble taints' labels after it.
pub fn node_status(ready: Option<&str>, unschedulable: bool, labels: &[String]) -> String {
    let mut status = match ready {
        Some("True") => "Ready",
        Some(_) => "NotReady",
        None => "Unknown",
    }
    .to_string();
    if unschedulable {
        status.push_str(",SchedulingDisabled");
    }
    append(&mut status, labels);
    status
}

/// The labels in a node's Status beyond kubectl's own words: its trouble taints.
pub fn labels_in(status: &str) -> impl Iterator<Item = &str> {
    status.split(',').filter(|w| !w.is_empty() && !KUBECTL_WORDS.contains(w))
}

/// `agent-not-ready` -> `AgentNotReady`; letters and digits only, so a label never holds
/// the comma that separates them.
fn camel(name: &str) -> String {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map(|c| c.to_ascii_uppercase()).into_iter().chain(chars).collect::<String>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{append, camel, labels_in, node_status, relabel, trouble_label, trouble_labels};
    use proptest::prelude::*;

    #[test]
    fn labels_of_trouble_taints() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("node.cilium.io/agent-not-ready", "NoSchedule", Some("CiliumAgentNotReady")),
            ("node.kubernetes.io/memory-pressure", "NoSchedule", Some("MemoryPressure")),
            ("node.kubernetes.io/disk-pressure", "NoSchedule", Some("DiskPressure")),
            ("node.kubernetes.io/pid-pressure", "NoSchedule", Some("PIDPressure")),
            ("node.kubernetes.io/network-unavailable", "NoSchedule", Some("NetworkUnavailable")),
            ("node.kubernetes.io/out-of-service", "NoExecute", Some("OutOfService")),
            ("node.cloudprovider.kubernetes.io/uninitialized", "NoSchedule", Some("Uninitialized")),
            ("node.cloudprovider.kubernetes.io/shutdown", "NoSchedule", Some("Shutdown")),
            ("ToBeDeletedByClusterAutoscaler", "NoSchedule", Some("ToBeDeleted")),
            ("DeletionCandidateOfClusterAutoscaler", "PreferNoSchedule", Some("DeletionCandidate")),
            ("karpenter.sh/disrupted", "NoSchedule", Some("Disrupted")),
            ("karpenter.sh/unregistered", "NoExecute", Some("Unregistered")),
            // By the words in their name.
            ("ebs.csi.aws.com/agent-not-ready", "NoExecute", Some("AgentNotReady")),
            ("example.com/storage-unavailable", "NoSchedule", Some("StorageUnavailable")),
            ("example.com/gpu_driver_uninitialized", "NoSchedule", Some("GpuDriverUninitialized")),
            ("example.com/IOPressure", "NoSchedule", Some("IOPressure")),
            // kubectl's Status already says it.
            ("node.kubernetes.io/not-ready", "NoExecute", None),
            ("node.kubernetes.io/unreachable", "NoExecute", None),
            ("node.kubernetes.io/unschedulable", "NoSchedule", None),
            // Set on purpose to dedicate a node.
            ("production", "NoSchedule", None),
            ("NodeGpu", "NoSchedule", None),
            ("nvidia.com/gpu", "NoSchedule", None),
            ("CriticalAddonsOnly", "NoSchedule", None),
            ("node-role.kubernetes.io/control-plane", "NoSchedule", None),
            ("node-role.kubernetes.io/master", "NoSchedule", None),
            ("node-role.wiremind.io/egress", "NoSchedule", None),
            ("node-role.example.com/not-ready", "NoSchedule", None),
            ("kubernetes.azure.com/scalesetpriority", "NoSchedule", None),
            // Only a preference: pods still go there.
            ("node.cilium.io/agent-not-ready", "PreferNoSchedule", None),
            ("ToBeDeletedByClusterAutoscaler", "PreferNoSchedule", None),
            ("node.cilium.io/agent-not-ready", "", None),
            // Not to be read as kubectl's own words.
            ("example.com/not-ready", "NoSchedule", Some("ExampleNotReady")),
            ("not-ready", "NoSchedule", Some("TaintNotReady")),
            ("node.kubernetes.io/ready", "NoSchedule", Some("NodeReady")),
            // Nothing to name.
            ("node.kubernetes.io/", "NoSchedule", None),
            ("", "NoSchedule", None),
        ];
        for (key, effect, want) in cases {
            assert_eq!(trouble_label(key, effect).as_deref(), *want, "{key}:{effect}");
        }
    }

    #[test]
    fn labels_once_in_order() {
        let taints = [
            ("production", "NoSchedule"),
            ("node.cilium.io/agent-not-ready", "NoSchedule"),
            ("ToBeDeletedByClusterAutoscaler", "NoSchedule"),
            ("node.cilium.io/agent-not-ready", "NoExecute"),
        ];
        assert_eq!(trouble_labels(taints), ["CiliumAgentNotReady", "ToBeDeleted"]);
        assert!(trouble_labels(Vec::<(&str, &str)>::new()).is_empty());
    }

    #[test]
    fn statuses_as_kubectl_prints_them() {
        let cilium = ["CiliumAgentNotReady".to_string()];
        assert_eq!(node_status(Some("True"), false, &[]), "Ready");
        assert_eq!(node_status(Some("False"), true, &[]), "NotReady,SchedulingDisabled");
        assert_eq!(node_status(Some("Unknown"), false, &[]), "NotReady");
        assert_eq!(node_status(None, false, &[]), "Unknown");
        assert_eq!(node_status(Some("True"), true, &cilium), "Ready,SchedulingDisabled,CiliumAgentNotReady");
        let mut empty = String::new();
        append(&mut empty, &cilium);
        assert_eq!(empty, "CiliumAgentNotReady");
        assert_eq!(
            labels_in("Ready,SchedulingDisabled,CiliumAgentNotReady").collect::<Vec<_>>(),
            ["CiliumAgentNotReady"]
        );
        assert_eq!(labels_in("NotReady,SchedulingDisabled").count(), 0);
        assert_eq!(relabel("Ready,SchedulingDisabled,CiliumAgentNotReady", &[]), "Ready,SchedulingDisabled");
        assert_eq!(relabel("Ready,ToBeDeleted", &cilium), "Ready,CiliumAgentNotReady");
    }

    #[test]
    fn camel_case() {
        assert_eq!(camel("agent-not-ready"), "AgentNotReady");
        assert_eq!(camel("out-of-service"), "OutOfService");
        assert_eq!(camel("NotReady"), "NotReady");
        assert_eq!(camel("a..b__c"), "ABC");
        assert_eq!(camel(""), "");
    }

    /// Taint keys of the shapes people dedicate nodes with: a name, maybe under a domain,
    /// without the words that mean trouble.
    fn on_purpose_key() -> impl Strategy<Value = String> {
        let domain = prop_oneof![
            Just(String::new()),
            Just("node-role.kubernetes.io/".to_string()),
            Just("example.com/".to_string()),
            Just("nvidia.com/".to_string()),
            "[a-z]{1,8}(\\.[a-z]{1,8}){0,2}/",
        ];
        (domain, "[A-Za-z][A-Za-z0-9_.-]{0,20}").prop_map(|(d, n)| format!("{d}{n}")).prop_filter(
            "no trouble word",
            |k| {
                let flat: String =
                    k.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
                !super::TROUBLE_WORDS.iter().any(|w| flat.contains(w))
                    && !super::KNOWN.iter().any(|(known, _)| known == k)
                    && !k.starts_with("node.kubernetes.io/")
            },
        )
    }

    fn effect() -> impl Strategy<Value = String> {
        prop_oneof![Just("NoSchedule"), Just("NoExecute"), Just("PreferNoSchedule"), Just("")].prop_map(String::from)
    }

    proptest! {
        /// A taint that is not trouble never changes a node's status.
        #[test]
        fn other_taints_leave_the_status_alone(
            taints in prop::collection::vec((on_purpose_key(), effect()), 0..6),
            ready in prop_oneof![Just(Some("True")), Just(Some("False")), Just(None)],
            unschedulable in any::<bool>(),
        ) {
            let labels = trouble_labels(taints.iter().map(|(k, e)| (k.as_str(), e.as_str())));
            prop_assert!(labels.is_empty(), "{labels:?}");
            prop_assert_eq!(node_status(ready, unschedulable, &labels), node_status(ready, unschedulable, &[]));
        }

        /// Any role taint, whatever its name says, and any taint that only prefers other
        /// nodes (but the autoscaler's) is not trouble either.
        #[test]
        fn roles_and_preferences_are_not_trouble(name in "[A-Za-z0-9_.-]{0,30}") {
            prop_assert_eq!(trouble_label(&format!("node-role.kubernetes.io/{name}"), "NoSchedule"), None);
            prop_assert_eq!(trouble_label(&format!("example.com/{name}"), "PreferNoSchedule"), None);
        }

        /// Labels only add to kubectl's Status: it stays as it was before them, each label
        /// is a word of letters and digits that kubectl doesn't use, and they read back.
        #[test]
        fn labels_append_words(
            keys in prop::collection::vec(prop_oneof![
                "[a-zA-Z0-9./_-]{0,40}",
                "node\\.kubernetes\\.io/[a-zA-Z0-9_.-]{0,20}",
                "([a-z]{1,6}(\\.[a-z]{1,6})?/)?[a-z-]{0,6}(not-ready|unavailable|pressure|ready|NotReady|Unknown)?",
            ], 0..6),
            effect in effect(),
            unschedulable in any::<bool>(),
        ) {
            let labels = trouble_labels(keys.iter().map(|k| (k.as_str(), effect.as_str())));
            for l in &labels {
                prop_assert!(!l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric()), "{l}");
            }
            let base = node_status(Some("True"), unschedulable, &[]);
            let status = node_status(Some("True"), unschedulable, &labels);
            prop_assert!(status.starts_with(&base));
            prop_assert_eq!(labels_in(&status).collect::<Vec<_>>(), labels.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
}
