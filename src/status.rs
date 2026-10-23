//! What a status word means, for pods and for any CRD's status column ("SecretSynced",
//! "Valid", "Healthy", "Progressing"…): one classifier for tables, overviews and the dashboard.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Tone {
    Good,
    Done,
    Busy,
    Bad,
}

/// Words that mean something is wrong wherever they appear ("SecretSyncedError").
const BAD: [&str; 23] = [
    "notready",
    "unhealthy",
    "invalid",
    "unavailable",
    "notfound",
    "error",
    "errimagepull",
    "fail",
    "crash",
    "backoff",
    "oomkilled",
    "evicted",
    "denied",
    "forbidden",
    "degraded",
    "missing",
    "stalled",
    "rejected",
    "conflict",
    "outofsync",
    "lost",
    "broken",
    // A container whose state was lost (often an evicted pod's): it will not come back.
    "containerstatusunknown",
];

/// Finished: neither good nor bad any more.
const DONE: [&str; 5] = ["completed", "succeeded", "done", "finished", "terminated"];

const BUSY: [&str; 14] = [
    "pending",
    "progressing",
    "creating",
    "initializing",
    "terminating",
    "waiting",
    "reconciling",
    "syncing",
    "updating",
    "provisioning",
    "starting",
    "deleting",
    "suspended",
    "unknown",
];

/// Good words found anywhere ("NewReplicaSetAvailable"), unless negated ("Unready").
const GOOD: [&str; 23] = [
    "running",
    "ready",
    "active",
    "bound",
    "complete",
    "available",
    "synced",
    "valid",
    "healthy",
    "succeeded",
    "success",
    "reconciled",
    "applied",
    "issued",
    "deployed",
    "established",
    "provisioned",
    "programmed",
    "accepted",
    "uptodate",
    "insync",
    "online",
    "started",
];

/// Good words too short to look for inside others ("Broken" holds "ok"): whole words only.
const GOOD_WORDS: [&str; 3] = ["ok", "up", "true"];

/// Prefixes that turn a good word into a bad one: "NotSynced", "Unbound", "Inactive".
const NEGATIONS: [&str; 3] = ["not", "un", "in"];

/// The tone of a status; None for words we do not know (no color then).
pub(crate) fn tone(status: &str) -> Option<Tone> {
    let lower = status.trim().to_lowercase();
    let s: String = lower.chars().filter(|c| !matches!(c, ' ' | '-' | '_')).collect();
    if s.is_empty() {
        return None;
    }
    let words = words(status);
    // "Installed" holds "stalled".
    let bad_text = s.replace("install", "");
    if BAD.iter().any(|w| bad_text.contains(w)) || words.iter().any(|w| w == "false") || negated(&words) {
        return Some(Tone::Bad);
    }
    if DONE.contains(&s.as_str()) {
        return Some(Tone::Done);
    }
    if BUSY.iter().any(|w| s.contains(w)) || s.starts_with("init:") {
        return Some(Tone::Busy);
    }
    if GOOD.iter().any(|w| s.contains(w)) || words.iter().any(|w| GOOD_WORDS.contains(&w.as_str())) {
        return Some(Tone::Good);
    }
    None
}

/// Does a good word start `rest` ("readyyet")? Short words must be all of it.
fn starts_good(rest: &str) -> bool {
    GOOD.iter().any(|g| rest.starts_with(g)) || GOOD_WORDS.contains(&rest)
}

/// A good word negated: "Unready", "Inactive", "NotSynced", "not ready".
fn negated(words: &[String]) -> bool {
    words.iter().enumerate().any(|(i, w)| {
        let prefixed =
            NEGATIONS.iter().any(|n| w.strip_prefix(n).is_some_and(|rest| !rest.is_empty() && starts_good(rest)));
        let before = w == "not" && words.get(i + 1).is_some_and(|next| starts_good(next));
        prefixed || before
    })
}

/// Lowercase words of a status, split on separators and camel case ("OOMKilled" -> oom, killed).
fn words(status: &str) -> Vec<String> {
    let chars: Vec<char> = status.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_lower) {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Condition types where "True" is the bad state: the one list every page, table and
/// check colors and counts conditions by.
pub fn true_is_bad(ty: &str) -> bool {
    matches!(
        ty,
        "ScalingLimited"
            | "MemoryPressure"
            | "DiskPressure"
            | "PIDPressure"
            | "NetworkUnavailable"
            | "Failed"
            | "FailureTarget"
            | "ReplicaFailure"
            | "DisruptionTarget"
            | "Stalled"
            // node-problem-detector
            | "KernelDeadlock"
            | "ReadonlyFilesystem"
            | "FrequentKubeletRestart"
            | "FrequentDockerRestart"
            | "FrequentContainerdRestart"
            | "FrequentUnregisterNetDevice"
            | "CorruptDockerOverlay2"
            | "KubeletUnhealthy"
            | "ContainerRuntimeUnhealthy"
            // namespace deletion
            | "NamespaceDeletionDiscoveryFailure"
            | "NamespaceDeletionGroupVersionParsingFailure"
            | "NamespaceDeletionContentFailure"
            | "NamespaceContentRemaining"
            | "NamespaceFinalizersRemaining"
            // CRDs and VPAs
            | "NonStructuralSchema"
            | "Terminating"
            | "LowConfidence"
            | "NoPodsMatched"
            | "ConfigUnsupported"
            | "ConfigDeprecated"
    )
}

/// Whether a node condition reports a problem. On a node every condition but Ready is
/// True when something is wrong (pressure, node-problem-detector's `KubeletProblem`,
/// Azure's `VMEventScheduled`…), so one we don't know is flagged too, unless its name says
/// it is good when True ("GPUHealthy").
pub fn node_condition_bad(ty: &str, status: &str) -> bool {
    status == "True" && (true_is_bad(ty) || (!true_is_good(ty) && tone(ty) != Some(Tone::Good)))
}

/// Condition types where "True" is the good state, so "False" is bad.
pub fn true_is_good(ty: &str) -> bool {
    matches!(
        ty,
        "Ready"
            | "Available"
            | "Progressing"
            | "Complete"
            | "SuccessCriteriaMet"
            | "Initialized"
            | "ContainersReady"
            | "PodScheduled"
            | "PodReadyToStartContainers"
            | "Established"
            | "NamesAccepted"
            | "ScalingActive"
            | "AbleToScale"
            | "RecommendationProvided"
            | "Accepted"
            | "Programmed"
            | "ResolvedRefs"
            | "Synced"
            | "Healthy"
    )
}

#[cfg(test)]
mod tests {
    use super::{Tone, node_condition_bad, tone, words};

    #[test]
    fn node_conditions() {
        // Known and unknown problem conditions, as node-problem-detector and clouds add them.
        for ty in ["MemoryPressure", "KernelDeadlock", "KubeletProblem", "ContainerRuntimeProblem", "VMEventScheduled"]
        {
            assert!(node_condition_bad(ty, "True"), "{ty}");
            assert!(!node_condition_bad(ty, "False"), "{ty}");
            assert!(!node_condition_bad(ty, "Unknown"), "{ty}");
        }
        // Good when True.
        for ty in ["Ready", "GPUHealthy", "CiliumAgentReady"] {
            assert!(!node_condition_bad(ty, "True"), "{ty}");
        }
    }

    #[test]
    fn tones() {
        let cases: &[(&str, Option<Tone>)] = &[
            // Good.
            ("SecretSynced", Some(Tone::Good)),
            ("Valid", Some(Tone::Good)),
            ("Ready", Some(Tone::Good)),
            ("Running", Some(Tone::Good)),
            ("Bound", Some(Tone::Good)),
            ("Available", Some(Tone::Good)),
            ("Healthy", Some(Tone::Good)),
            ("Synced", Some(Tone::Good)),
            ("InSync", Some(Tone::Good)),
            ("True", Some(Tone::Good)),
            ("OK", Some(Tone::Good)),
            ("Up", Some(Tone::Good)),
            ("Ready,SchedulingDisabled", Some(Tone::Good)),
            ("MinimumReplicasAvailable", Some(Tone::Good)),
            ("Installed", None),
            // Done and busy.
            ("Completed", Some(Tone::Done)),
            ("Succeeded", Some(Tone::Done)),
            ("Pending", Some(Tone::Busy)),
            ("Progressing", Some(Tone::Busy)),
            ("ContainerCreating", Some(Tone::Busy)),
            ("Init:0/2", Some(Tone::Busy)),
            ("Unknown", Some(Tone::Busy)),
            // Negated good words.
            ("Unready", Some(Tone::Bad)),
            ("Inactive", Some(Tone::Bad)),
            ("Unbound", Some(Tone::Bad)),
            ("NotSynced", Some(Tone::Bad)),
            ("Unsynced", Some(Tone::Bad)),
            ("SecretNotSynced", Some(Tone::Bad)),
            ("Not Ready", Some(Tone::Bad)),
            ("not-ready", Some(Tone::Bad)),
            ("NotReady", Some(Tone::Bad)),
            ("Incomplete", Some(Tone::Bad)),
            ("Untrue", Some(Tone::Bad)),
            // Bad words.
            ("Broken", Some(Tone::Bad)),
            ("False", Some(Tone::Bad)),
            ("InvalidProviderConfig", Some(Tone::Bad)),
            ("SecretSyncedError", Some(Tone::Bad)),
            ("CrashLoopBackOff", Some(Tone::Bad)),
            ("ErrImagePull", Some(Tone::Bad)),
            ("OOMKilled", Some(Tone::Bad)),
            ("Init:Error", Some(Tone::Bad)),
            ("OutOfSync", Some(Tone::Bad)),
            ("Lost", Some(Tone::Bad)),
            ("NotReady,SchedulingDisabled", Some(Tone::Bad)),
            ("ContainerStatusUnknown", Some(Tone::Bad)),
            ("Init:ContainerStatusUnknown", Some(Tone::Bad)),
            // Unknown words stay neutral.
            ("Lorem", None),
            ("Setup", None),
            ("Lookup", None),
            ("", None),
            ("  ", None),
        ];
        for (status, want) in cases {
            assert_eq!(tone(status), *want, "{status:?}");
        }
    }

    #[test]
    fn splits_words() {
        assert_eq!(words("OOMKilled"), ["oom", "killed"]);
        assert_eq!(words("SecretNotSynced"), ["secret", "not", "synced"]);
        assert_eq!(words("not-ready"), ["not", "ready"]);
        assert_eq!(words("Init:0/2"), ["init", "0", "2"]);
    }
}
