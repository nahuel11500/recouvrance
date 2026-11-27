//! What a sync would change, read by component rather than by Kubernetes object: a
//! StatefulSet, its Service, its ConfigMap and its volume claims `data-<name>-0..2` are one
//! component, named by [`component_of`]. Each component is one line in plain words:
//!
//! - an image bump is one line per image and versions, with the workloads that run it
//!   (`api v1.4 → v1.5 · web, worker`);
//! - a change made by hand in the cluster, git unchanged since the last sync, says so
//!   (`replicas 5 → 3 · changed in the cluster, not in git`);
//! - a new component says what it adds (`new · Deployment, HPA`), one removed from git what a
//!   prune deletes (`no longer in git · Deployment, ConfigMap`).
//!
//! The changes come from the smart diff when the managed resources were read (facts, noise
//! left out and counted), else from Argo CD's list of resources that differ. The lines go
//! changed first, then added, then removed.

use std::collections::BTreeSet;

use super::Summary;
use super::dashboard::DiffSummary;
use super::diagnosis::Res;
use crate::smartdiff::{Fact, ObjectKey, SmartDiff};

/// Facts said in a changed component's line, at most; the others counted.
const FACTS_SAID: usize = 2;
/// Kinds said in a line, at most; the others counted.
const KINDS_SAID: usize = 4;

/// The component a resource belongs to, from its name: an ordinal suffix (`-0`, `-12`) and a
/// `data-` or `pgdata-` prefix (a StatefulSet's volume claims) are taken off.
/// `data-search-0` → `search`, `web-0` → `web`, `api` → `api`.
pub fn component_of(name: &str) -> String {
    let mut out = name;
    for prefix in ["pgdata-", "data-"] {
        if let Some(rest) = out.strip_prefix(prefix).filter(|r| !r.is_empty()) {
            out = rest;
            break;
        }
    }
    if let Some((head, tail)) = out.rsplit_once('-')
        && !head.is_empty()
        && !tail.is_empty()
        && tail.bytes().all(|b| b.is_ascii_digit())
    {
        out = head;
    }
    out.to_string()
}

/// A kind as a line says it: the long ones by their usual short names.
pub fn kind_word(kind: &str) -> &str {
    match kind {
        "HorizontalPodAutoscaler" => "HPA",
        "PersistentVolumeClaim" => "PVC",
        "PersistentVolume" => "PV",
        "PodDisruptionBudget" => "PDB",
        "CustomResourceDefinition" => "CRD",
        other => other,
    }
}

/// The kinds of `keys` in the order first met, a count before those there are several of:
/// `StatefulSet, Service, 3 PVC`; the first few, then how many more kinds.
fn kinds_text<'a>(keys: impl IntoIterator<Item = &'a ObjectKey>) -> String {
    let mut kinds: Vec<(&str, usize)> = Vec::new();
    for k in keys {
        let word = kind_word(&k.kind);
        match kinds.iter_mut().find(|(w, _)| *w == word) {
            Some(e) => e.1 += 1,
            None => kinds.push((word, 1)),
        }
    }
    let mut out: Vec<String> =
        kinds.iter().take(KINDS_SAID).map(|(w, n)| if *n == 1 { w.to_string() } else { format!("{n} {w}") }).collect();
    if kinds.len() > KINDS_SAID {
        out.push(format!("+{} more", kinds.len() - KINDS_SAID));
    }
    out.join(", ")
}

/// How a component changes, in the order the lines go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Change {
    /// An image goes to another version.
    Image,
    /// A setting changes.
    Changed,
    /// New: in git, not in the cluster.
    Added,
    /// No longer in git: a prune deletes it.
    Removed,
}

impl Change {
    /// Its sign in a one-line summary.
    pub fn sign(self) -> &'static str {
        match self {
            Self::Image | Self::Changed => "~",
            Self::Added => "+",
            Self::Removed => "–",
        }
    }
}

/// One component's line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub change: Change,
    /// The component; for an image, the image's name.
    pub name: String,
    /// What changes in a few words: `v1 → v2` for an image, `replicas 5 → 3` for a setting.
    pub what: Option<String>,
    /// Under the name: an image's workloads, where a change was made, or the kinds it touches.
    pub detail: String,
    /// The resources it covers.
    pub resources: Vec<ObjectKey>,
    /// Changes of its resources left out as noise.
    pub noise: usize,
}

/// One resource that differs, as read.
struct Item {
    key: ObjectKey,
    change: Change,
    facts: Vec<Fact>,
    noise: usize,
}

fn key_of(r: &Res) -> ObjectKey {
    ObjectKey { group: r.group.clone(), kind: r.kind.clone(), namespace: r.namespace.clone(), name: r.name.clone() }
}

/// The resources that differ: the smart diff's (noise only left out), then those Argo CD
/// lists that the smart diff doesn't have. The number of noise changes of those left out.
fn items(diff: &DiffSummary, smart: Option<&SmartDiff>) -> (Vec<Item>, usize) {
    let mut out = Vec::new();
    let mut seen: BTreeSet<ObjectKey> = BTreeSet::new();
    let mut noise_only = 0;
    for r in smart.map_or(&[][..], |s| s.resources.as_slice()) {
        seen.insert(r.key.clone());
        if r.facts.is_empty() {
            noise_only += r.noise.len();
            continue;
        }
        let change = if r.facts.contains(&Fact::Added) {
            Change::Added
        } else if r.facts.contains(&Fact::Pruned) {
            Change::Removed
        } else {
            Change::Changed
        };
        let facts = r.facts.iter().filter(|f| !matches!(f, Fact::Added | Fact::Pruned)).cloned().collect();
        out.push(Item { key: r.key.clone(), change, facts, noise: r.noise.len() });
    }
    let listed = diff
        .changed
        .iter()
        .chain(&diff.unknown)
        .map(|r| (r, Change::Changed))
        .chain(diff.added.iter().map(|r| (r, Change::Added)))
        .chain(diff.pruned.iter().map(|r| (r, Change::Removed)));
    for (r, change) in listed {
        let key = key_of(r);
        if seen.insert(key.clone()) {
            out.push(Item { key, change, facts: Vec::new(), noise: 0 });
        }
    }
    (out, noise_only)
}

/// Some names, deduplicated in the order first met.
fn names(list: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in list {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// What a sync would change, one line per component (see the module's doc): `git_unchanged`
/// when git has not moved since the last sync, so that what differs was changed in the cluster.
pub fn lines(diff: &DiffSummary, smart: Option<&SmartDiff>, git_unchanged: bool) -> Vec<Line> {
    let (mut items, _) = items(diff, smart);
    // Each image bump once, with the components that run it; taken out of their resources.
    let mut images: Vec<(String, String, String, Vec<ObjectKey>)> = Vec::new();
    for item in items.iter_mut().filter(|i| i.change == Change::Changed) {
        let mut bumped = false;
        item.facts.retain(|f| match f {
            Fact::Image { from_repo, from_tag, to_repo, to_tag } if from_repo == to_repo => {
                let name = to_repo.rsplit('/').next().unwrap_or(to_repo).to_string();
                match images.iter_mut().find(|(n, f, t, _)| *n == name && f == from_tag && t == to_tag) {
                    Some(e) => e.3.push(item.key.clone()),
                    None => images.push((name, from_tag.clone(), to_tag.clone(), vec![item.key.clone()])),
                }
                bumped = true;
                false
            }
            _ => true,
        });
        // Its pods restart with the new image: said by the image's line.
        if bumped {
            item.facts.retain(|f| !matches!(f, Fact::PodTemplate { .. }));
        }
    }
    let mut out: Vec<Line> = images
        .into_iter()
        .map(|(name, from, to, keys)| {
            let workloads = names(keys.iter().map(|k| component_of(&k.name)));
            Line {
                change: Change::Image,
                name,
                what: Some(format!("{from} → {to}")),
                detail: workloads.join(", "),
                noise: 0,
                resources: keys,
            }
        })
        .collect();
    // What is left, by component, in the order first met.
    let mut components: Vec<(String, Vec<Item>)> = Vec::new();
    for item in items {
        // An image bump said whole by its image's line.
        if item.change == Change::Changed
            && item.facts.is_empty()
            && out.iter().any(|l| l.resources.contains(&item.key))
        {
            continue;
        }
        let name = component_of(&item.key.name);
        match components.iter_mut().find(|(n, _)| *n == name) {
            Some((_, list)) => list.push(item),
            None => components.push((name, vec![item])),
        }
    }
    for (name, list) in components {
        let all = |c: Change| list.iter().all(|i| i.change == c);
        let change = if all(Change::Added) {
            Change::Added
        } else if all(Change::Removed) {
            Change::Removed
        } else {
            Change::Changed
        };
        let keys: Vec<ObjectKey> = list.iter().map(|i| i.key.clone()).collect();
        let kinds = kinds_text(&keys);
        let (what, detail) = match change {
            Change::Added if git_unchanged => (None, format!("missing from the cluster, still in git · {kinds}")),
            Change::Added => (None, format!("new · {kinds}")),
            Change::Removed => (None, format!("no longer in git · {kinds}")),
            _ => {
                // Each fact once; a resource of it added or removed said by its kind.
                let mut said: Vec<String> = Vec::new();
                for i in &list {
                    let words: Vec<String> = match i.change {
                        Change::Added => vec![format!("new {}", kind_word(&i.key.kind))],
                        Change::Removed => vec![format!("{} no longer in git", kind_word(&i.key.kind))],
                        _ => i.facts.iter().map(Fact::text).collect(),
                    };
                    for w in words {
                        if !said.contains(&w) {
                            said.push(w);
                        }
                    }
                }
                let mut what: Vec<String> = said.iter().take(FACTS_SAID).cloned().collect();
                if said.len() > FACTS_SAID {
                    what.push(format!("+{} more", said.len() - FACTS_SAID));
                }
                let known = !said.is_empty();
                let detail = match (git_unchanged, known) {
                    (true, _) => "changed in the cluster, not in git".to_string(),
                    (false, true) => kinds,
                    (false, false) => format!("differs from git · {kinds}"),
                };
                ((!what.is_empty()).then(|| what.join("; ")), detail)
            }
        };
        out.push(Line { change, name, what, detail, noise: list.iter().map(|i| i.noise).sum(), resources: keys });
    }
    // Changed first, then added, then removed; the order met within each.
    out.sort_by_key(|l| match l.change {
        Change::Image | Change::Changed => 0,
        Change::Added => 1,
        Change::Removed => 2,
    });
    out
}

/// What a sync with prune would delete: what Argo CD marks to prune, and what the smart diff
/// finds only in the cluster.
pub fn pruned(diff: &DiffSummary, smart: Option<&SmartDiff>) -> Vec<Res> {
    let mut out = diff.pruned.clone();
    for r in smart.map_or(&[][..], |s| s.resources.as_slice()).iter().filter(|r| r.facts.contains(&Fact::Pruned)) {
        let k = &r.key;
        let res = Res {
            group: k.group.clone(),
            kind: k.kind.clone(),
            namespace: k.namespace.clone(),
            name: k.name.clone(),
            ..Default::default()
        };
        if !out.iter().any(|o| o.same(&res)) {
            out.push(res);
        }
    }
    out
}

/// The lines in one: `~ api · + search · – old-worker`.
pub fn one_line(lines: &[Line]) -> String {
    lines.iter().map(|l| format!("{} {}", l.change.sign(), l.name)).collect::<Vec<_>>().join(" · ")
}

/// The noise left out, in one sentence: `Left out as noise: 14 changes (checksum annotation
/// 9, chart label 5).`; None without any.
pub fn noise_sentence(smart: &SmartDiff) -> Option<String> {
    let n = smart.noise();
    if n == 0 {
        return None;
    }
    let by: Vec<String> = smart.noise_by_rule().iter().take(4).map(|(r, n)| format!("{r} {n}")).collect();
    Some(format!(
        "Left out as noise: {} ({}). They come with a real change and say nothing alone.",
        crate::plural::count(n, "change"),
        by.join(", ")
    ))
}

/// Whether git has not moved since the last sync: the revision Argo CD compares with is the
/// one deployed last. What differs was then changed in the cluster.
pub fn git_unchanged(summary: &Summary) -> bool {
    match (&summary.revision, summary.history.first()) {
        (Some(target), Some(last)) => !target.is_empty() && *target == last.revision,
        _ => false,
    }
}

/// An Argo CD state of resources out of sync, with what it means and the resources in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgoState {
    /// `OutOfSync`, `Missing`, `Prune`.
    pub state: &'static str,
    pub meaning: &'static str,
    pub resources: Vec<Res>,
    /// Resources past those listed.
    pub more: usize,
}

/// What is out of sync, by Argo CD state: `OutOfSync` (differs from git), `Missing` (in git, not
/// in the cluster), `Prune` (in the cluster, no longer in git); those with none left out.
pub fn argo_states(diff: &DiffSummary) -> Vec<ArgoState> {
    let differs: Vec<Res> = diff.changed.iter().chain(&diff.unknown).cloned().collect();
    [
        ArgoState { state: "OutOfSync", meaning: "differs from git", resources: differs, more: diff.more },
        ArgoState { state: "Missing", meaning: "in git, not in the cluster", resources: diff.added.clone(), more: 0 },
        ArgoState {
            state: "Prune",
            meaning: "in the cluster, no longer in git",
            resources: diff.pruned.clone(),
            more: 0,
        },
    ]
    .into_iter()
    .filter(|s| !s.resources.is_empty() || s.more > 0)
    .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::smartdiff::{self, objects};

    #[test]
    fn components_are_named_without_ordinals_and_data_prefixes() {
        assert_eq!(component_of("data-search-warm-0"), "search-warm");
        assert_eq!(component_of("data-search-warm-12"), "search-warm");
        assert_eq!(component_of("pgdata-reports-1"), "reports");
        assert_eq!(component_of("search-warm"), "search-warm");
        assert_eq!(component_of("daily-clean-v1"), "daily-clean-v1");
        assert_eq!(component_of("web-0"), "web");
        assert_eq!(component_of("data-"), "data-", "nothing left: kept as it is");
        assert_eq!(component_of("0"), "0");
        assert_eq!(component_of("-0"), "-0");
    }

    fn res(group: &str, kind: &str, name: &str) -> Res {
        Res {
            group: group.into(),
            kind: kind.into(),
            name: name.into(),
            namespace: "shop".into(),
            ..Default::default()
        }
    }

    fn deployment(name: &str, image: &str, replicas: u64) -> Value {
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": name, "namespace": "shop"},
            "spec": {"replicas": replicas, "template": {"spec": {"containers": [{"name": "main", "image": image}]}}},
        })
    }

    /// A StatefulSet, its Service, ConfigMap and three claims removed from git are one line.
    #[test]
    fn a_component_groups_its_resources() {
        let diff = DiffSummary {
            pruned: vec![
                res("apps", "StatefulSet", "search"),
                res("", "Service", "search"),
                res("", "ConfigMap", "search"),
                res("", "PersistentVolumeClaim", "data-search-0"),
                res("", "PersistentVolumeClaim", "data-search-1"),
                res("", "PersistentVolumeClaim", "data-search-2"),
            ],
            ..Default::default()
        };
        let lines = lines(&diff, None, false);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].change, Change::Removed);
        assert_eq!(lines[0].name, "search");
        assert_eq!(lines[0].detail, "no longer in git · StatefulSet, Service, ConfigMap, 3 PVC");
        assert_eq!(lines[0].resources.len(), 6);
    }

    /// Changed first, then added, then removed, whatever the order read.
    #[test]
    fn lines_go_changed_added_removed() {
        let diff = DiffSummary {
            pruned: vec![res("apps", "Deployment", "old")],
            added: vec![res("apps", "Deployment", "browser"), res("autoscaling", "HorizontalPodAutoscaler", "browser")],
            changed: vec![res("apps", "Deployment", "web")],
            ..Default::default()
        };
        let lines = lines(&diff, None, false);
        let order: Vec<(Change, &str)> = lines.iter().map(|l| (l.change, l.name.as_str())).collect();
        assert_eq!(order, [(Change::Changed, "web"), (Change::Added, "browser"), (Change::Removed, "old")]);
        assert_eq!(lines[1].detail, "new · Deployment, HPA");
        assert_eq!(lines[0].detail, "differs from git · Deployment", "nothing known of what differs");
        assert_eq!(one_line(&lines), "~ web · + browser · – old");
    }

    /// An image bump is one line per image and versions, with its workloads; a replicas change
    /// made in the cluster says so; noise only is left out.
    #[test]
    fn smart_diff_reads_in_plain_words() {
        let live = objects([
            deployment("web", "registry.example.com/shop/api:v1.4", 3),
            deployment("worker-0", "registry.example.com/shop/api:v1.4", 2),
            deployment("export", "registry.example.com/shop/export:v2", 5),
        ]);
        let desired = objects([
            deployment("web", "registry.example.com/shop/api:v1.5", 3),
            deployment("worker-0", "registry.example.com/shop/api:v1.5", 2),
            deployment("export", "registry.example.com/shop/export:v2", 3),
        ]);
        let smart = smartdiff::compare(&live, &desired);
        let lines = lines(&DiffSummary::default(), Some(&smart), true);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].change, Change::Image);
        assert_eq!(lines[0].name, "api");
        assert_eq!(lines[0].what.as_deref(), Some("v1.4 → v1.5"));
        assert_eq!(lines[0].detail, "web, worker");
        assert_eq!(lines[1].name, "export");
        assert_eq!(lines[1].what.as_deref(), Some("replicas 5 → 3"));
        assert_eq!(lines[1].detail, "changed in the cluster, not in git");
    }

    /// A resource the smart diff found noise only in gets no line, even when Argo CD lists it.
    #[test]
    fn noise_only_gets_no_line() {
        let mut before = deployment("web", "api:v1", 3);
        before["metadata"]["labels"] = json!({"helm.sh/chart": "web-1.0.0"});
        let mut after = before.clone();
        after["metadata"]["labels"] = json!({"helm.sh/chart": "web-1.0.1"});
        let smart = smartdiff::compare(&objects([before]), &objects([after]));
        let diff = DiffSummary { changed: vec![res("apps", "Deployment", "web")], ..Default::default() };
        assert!(lines(&diff, Some(&smart), false).is_empty());
        assert_eq!(
            noise_sentence(&smart).as_deref(),
            Some("Left out as noise: 1 change (chart label 1). They come with a real change and say nothing alone.")
        );
    }

    #[test]
    fn argo_states_say_what_they_mean() {
        let diff = DiffSummary {
            changed: vec![res("apps", "Deployment", "web")],
            pruned: vec![res("", "ConfigMap", "old")],
            ..Default::default()
        };
        let states = argo_states(&diff);
        let said: Vec<(&str, &str)> = states.iter().map(|s| (s.state, s.meaning)).collect();
        assert_eq!(said, [("OutOfSync", "differs from git"), ("Prune", "in the cluster, no longer in git")]);
    }

    #[test]
    fn git_unchanged_compares_the_target_with_the_last_deployment() {
        let mut s = Summary { revision: Some("abc1234".into()), ..Default::default() };
        assert!(!git_unchanged(&s), "never deployed");
        s.history.push(crate::k8s::argo::Deployment { revision: "abc1234".into(), ..Default::default() });
        assert!(git_unchanged(&s));
        s.revision = Some("def5678".into());
        assert!(!git_unchanged(&s));
    }
}
