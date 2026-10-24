//! The command palette's index of object names across the connected clusters, loaded in
//! the background one list (kind and cluster) at a time.

use std::time::{Duration, Instant};

use gpui_kit::Task;
use gpui_kit::assets::IconName as Lucide;

use crate::ui::palette::{Item as PaletteItem, Target};

/// Kinds the palette searches across whole clusters.
pub(super) const SEARCH_KINDS: [&str; 12] = [
    "deployments",
    "statefulsets",
    "daemonsets",
    "cronjobs",
    "services",
    "ingresses",
    "configmaps",
    "secrets",
    "persistentvolumeclaims",
    "namespaces",
    "nodes",
    "pods",
];

/// How long an index is used before the palette loads it again.
const FRESH_FOR: Duration = Duration::from_secs(300);

/// An object's name, found on the cluster at `ctx` (its place in the connected list).
#[derive(Clone)]
pub(super) struct Entry {
    pub ctx: usize,
    pub kind: String,
    pub label: String,
    pub icon: Lucide,
    pub namespace: Option<String>,
    pub name: String,
}

impl Entry {
    /// The palette line for this object; `clusters` names the connected clusters.
    pub(super) fn item(&self, clusters: &[String]) -> PaletteItem {
        PaletteItem {
            label: self.name.clone(),
            detail: format!(
                "{}{}{}",
                self.label,
                self.namespace.as_ref().map(|n| format!(" · {n}")).unwrap_or_default(),
                on_cluster(clusters, self.ctx)
            ),
            icon: self.icon,
            target: Target::Object {
                ctx: self.ctx,
                kind: self.kind.clone(),
                name: self.name.clone(),
                namespace: self.namespace.clone(),
            },
        }
    }
}

/// " · cluster" when several clusters are connected, nothing otherwise.
pub(super) fn on_cluster(clusters: &[String], ctx: usize) -> String {
    match clusters.get(ctx) {
        Some(name) if clusters.len() > 1 => format!(" · {name}"),
        _ => String::new(),
    }
}

#[derive(Default)]
pub(super) struct SearchIndex {
    entries: Vec<Entry>,
    /// The lists that couldn't be loaded: kind and cluster.
    failed: Vec<String>,
    built: Option<Instant>,
    /// Lists loaded so far, out of `total`.
    done: usize,
    total: usize,
    tasks: Vec<Task<()>>,
}

impl SearchIndex {
    pub(super) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Never loaded, or loaded too long ago.
    pub(super) fn is_stale(&self) -> bool {
        self.built.is_none_or(|t| t.elapsed() > FRESH_FOR)
    }

    /// Start loading again, `total` lists: drops the entries and the loads still running.
    pub(super) fn restart(&mut self, total: usize) {
        *self = Self { built: Some(Instant::now()), total, ..Self::default() };
    }

    /// Keep a list's load running while the index lives.
    pub(super) fn push_task(&mut self, task: Task<()>) {
        self.tasks.push(task);
    }

    /// One list loaded: its entries, or the list that failed. Returns the palette's status.
    pub(super) fn add(&mut self, entries: Vec<Entry>, failed: Option<String>) -> Option<String> {
        self.entries.extend(entries);
        self.failed.extend(failed);
        self.done += 1;
        self.status()
    }

    /// Forget everything (the entries point at clusters by their place in the list).
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// The palette's status line: progress while loading, then the lists that failed.
    pub(super) fn status(&self) -> Option<String> {
        if self.done < self.total {
            return Some(format!("indexing… {}/{}", self.done, self.total));
        }
        match self.failed.as_slice() {
            [] => None,
            [one] => Some(format!("Couldn't list {one}")),
            [first, rest @ ..] => Some(format!("Couldn't list {first} and {} more", rest.len())),
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::assets::IconName as Lucide;

    use super::{Entry, SearchIndex};

    fn entry(name: &str) -> Entry {
        Entry {
            ctx: 0,
            kind: "pods".into(),
            label: "Pods".into(),
            icon: Lucide::Box,
            namespace: None,
            name: name.into(),
        }
    }

    #[test]
    fn progress_then_failed_lists_are_said() {
        let mut index = SearchIndex::default();
        assert!(index.is_stale());
        assert_eq!(index.status(), None);
        index.restart(3);
        assert!(!index.is_stale());
        assert_eq!(index.status().as_deref(), Some("indexing… 0/3"));
        assert_eq!(index.add(vec![entry("a")], None).as_deref(), Some("indexing… 1/3"));
        index.add(vec![], Some("nodes on prod".into()));
        assert_eq!(index.add(vec![entry("b")], None).as_deref(), Some("Couldn't list nodes on prod"));
        assert_eq!(index.entries().len(), 2);
        index.restart(2);
        index.add(vec![], Some("pods on a".into()));
        assert_eq!(index.add(vec![], Some("pods on b".into())).as_deref(), Some("Couldn't list pods on a and 1 more"));
        assert!(index.entries().is_empty(), "a restart drops the old entries");
        index.clear();
        assert!(index.is_stale());
    }

    #[test]
    fn names_the_cluster_only_when_several() {
        let one = ["prod".to_string()];
        let two = ["prod".to_string(), "staging".to_string()];
        assert_eq!(entry("web").item(&one).detail, "Pods");
        let mut e = entry("web");
        e.ctx = 1;
        e.namespace = Some("team".into());
        assert_eq!(e.item(&two).detail, "Pods · team · staging");
    }
}
