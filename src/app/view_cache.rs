//! The last lists seen, to show again at once when you come back to them: the rows on
//! screen while a fresh list loads behind, replaced in one go once it is complete.

use std::sync::Arc;

use crate::k8s::{ColumnDef, Row};

/// Lists kept, and rows kept across all of them (rows are shared `Arc`s, so this bounds
/// how long old rows stay alive more than it costs memory).
const MAX_VIEWS: usize = 8;
const MAX_ROWS: usize = 10_000;

/// One list as it was last shown.
#[derive(Clone)]
pub struct CachedView {
    pub columns: Arc<Vec<ColumnDef>>,
    /// Rows per cluster, in cluster order.
    pub rows: Vec<Vec<Arc<Row>>>,
    pub at: std::time::Instant,
}

impl CachedView {
    fn len(&self) -> usize {
        self.rows.iter().map(Vec::len).sum()
    }
}

/// Recently shown lists by view key (clusters, kind, namespace, custom columns), most
/// recent last.
#[derive(Default)]
pub struct ViewCache {
    views: Vec<(String, CachedView)>,
}

impl ViewCache {
    /// Keep `view` under `key`, dropping the oldest lists past the limits. A list bigger
    /// than the whole budget is not kept, nor one without its columns (it would show
    /// rows with no cells): those leave what is kept under `key` as it was.
    pub fn put(&mut self, key: String, view: CachedView) {
        if view.columns.is_empty() {
            return;
        }
        self.views.retain(|(k, _)| *k != key);
        if view.len() > MAX_ROWS {
            return;
        }
        self.views.push((key, view));
        while self.views.len() > MAX_VIEWS || self.views.iter().map(|(_, v)| v.len()).sum::<usize>() > MAX_ROWS {
            self.views.remove(0);
        }
    }

    pub fn get(&self, key: &str) -> Option<&CachedView> {
        self.views.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::{CachedView, MAX_ROWS, MAX_VIEWS, ViewCache};
    use std::sync::Arc;

    fn view(rows: usize) -> CachedView {
        let row = Arc::new(crate::k8s::Row {
            uid: "u".into(),
            name: "n".into(),
            namespace: None,
            created: None,
            cells: Vec::new(),
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        });
        let columns = Arc::new(vec![crate::k8s::ColumnDef { name: "Name".into(), priority: 0 }]);
        CachedView { columns, rows: vec![vec![row; rows]], at: std::time::Instant::now() }
    }

    #[test]
    fn keeps_the_most_recent_lists_within_the_limits() {
        let mut c = ViewCache::default();
        for i in 0..MAX_VIEWS + 2 {
            c.put(format!("v{i}"), view(10));
        }
        assert!(c.get("v0").is_none() && c.get("v1").is_none(), "the oldest go first");
        assert!(c.get(&format!("v{}", MAX_VIEWS + 1)).is_some());
        // Putting a key again moves it to the end, so it is kept longest.
        c.put("v2".into(), view(10));
        c.put("big".into(), view(MAX_ROWS - 15));
        assert!(c.get("v2").is_some(), "kept: put again most recently");
        assert!(c.get("v3").is_none(), "dropped to stay within the row budget");
        // A list over the whole budget is never kept, and replaces nothing.
        c.put("huge".into(), view(MAX_ROWS + 1));
        assert!(c.get("huge").is_none() && c.get("big").is_some());
    }

    #[test]
    fn a_list_without_columns_replaces_nothing() {
        let mut c = ViewCache::default();
        c.put("pods".into(), view(10));
        c.put("pods".into(), CachedView { columns: Arc::default(), ..view(5) });
        assert_eq!(c.get("pods").map(|v| (v.columns.len(), v.len())), Some((1, 10)));
    }
}
