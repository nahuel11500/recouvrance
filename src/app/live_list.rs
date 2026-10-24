//! One kind listed live across the connected clusters: each cluster's rows and the
//! columns they are laid out in, which clusters are still listing or refused, their
//! errors and the progress notice. Plain state, no UI: [`LiveList::apply`] takes what a
//! cluster's watch sent and says what the table must show again.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use gpui_kit::SharedString;

use super::view_cache::CachedView;
use crate::k8s::{ColumnDef, Row, TableMsg};

#[derive(Default)]
pub(super) struct LiveList {
    /// Each cluster's latest rows, in cluster order.
    rows: Vec<Vec<Arc<Row>>>,
    /// The columns each cluster's rows are laid out in: two clusters can serve a CRD from
    /// different operator releases, with other printer columns.
    columns: Vec<Arc<Vec<ColumnDef>>>,
    /// Clusters whose list is still arriving.
    listing: HashSet<usize>,
    /// Clusters that refused to list the kind (403).
    refused: HashSet<usize>,
    /// How many clusters are watched (those serving the kind).
    watched: usize,
    /// Each cluster's last error, until that cluster sends rows again.
    errors: BTreeMap<usize, SharedString>,
    /// Progress: "Loading…", "Updating… (showing the list from 2h ago)", "Connecting…".
    pub notice: Option<SharedString>,
}

/// What the table must do after a message, see [`LiveList::apply`].
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Update {
    /// Columns the table takes: the first cluster's, or the first to come.
    pub columns: Option<Arc<Vec<ColumnDef>>>,
    /// The rows changed: merge them into the table again.
    pub rows: bool,
    /// The order may have changed: keep the highlight on its object.
    pub follow: bool,
    /// Every cluster watched has answered.
    pub loaded: bool,
    /// Set once every cluster has answered and one refused: whether all of them did.
    pub forbidden: Option<bool>,
}

impl LiveList {
    /// A list of `clusters` clusters, starting with the ones in `watched`.
    pub fn start(clusters: usize, watched: impl IntoIterator<Item = usize>) -> Self {
        let watched: HashSet<usize> = watched.into_iter().collect();
        let notice = (!watched.is_empty()).then(|| "Loading…".into());
        Self {
            rows: vec![Vec::new(); clusters],
            columns: vec![Arc::default(); clusters],
            watched: watched.len(),
            listing: watched,
            notice,
            ..Self::default()
        }
    }

    /// Show `view`, a list seen recently, while the new one loads. A cluster that is not
    /// watched gets no rows: nothing would replace them.
    pub fn restore(&mut self, view: CachedView) {
        self.columns = vec![view.columns; self.rows.len()];
        self.rows = view.rows;
        keep_watched(&mut self.rows, &self.listing);
    }

    /// Whether some cluster is still listing.
    pub fn is_loading(&self) -> bool {
        !self.listing.is_empty()
    }

    /// What the bar above the list says: what failed (`failed`, then each cluster's
    /// error), then the progress notice. Both show: a failing cluster's rows may be the
    /// old ones the notice warns of.
    pub fn status_line(&self, failed: Option<&SharedString>) -> Option<SharedString> {
        let parts: Vec<&str> =
            failed.into_iter().chain(self.errors.values()).chain(&self.notice).map(AsRef::as_ref).collect();
        (!parts.is_empty()).then(|| parts.join("; ").into())
    }

    /// Every cluster's rows, each with its cluster's index, laid out in `columns`.
    pub fn merged(&self, columns: &[ColumnDef]) -> Vec<(usize, Arc<Row>)> {
        (self.rows.iter().zip(&self.columns))
            .enumerate()
            .flat_map(|(i, (rows, defs))| relayout(rows, defs, columns).into_iter().map(move |r| (i, r)))
            .collect()
    }

    /// The list to keep for coming back to it, laid out in `columns`: only a complete
    /// one, with rows, that the clusters did not refuse.
    pub fn snapshot(&self, columns: Arc<Vec<ColumnDef>>, forbidden: bool) -> Option<CachedView> {
        if self.is_loading() || forbidden || self.rows.iter().all(Vec::is_empty) {
            return None;
        }
        let rows = (self.rows.iter().zip(&self.columns)).map(|(rows, defs)| relayout(rows, defs, &columns)).collect();
        Some(CachedView { columns, rows, at: std::time::Instant::now() })
    }

    /// Take a message from cluster `ctx` (named `name`). `shown` is the columns the
    /// table shows now, empty when it has none yet.
    pub fn apply(&mut self, ctx: usize, name: &str, msg: TableMsg, shown: &[ColumnDef]) -> Update {
        let mut up = Update::default();
        match msg {
            TableMsg::Columns(cols) => {
                // This cluster's rows from before (a relist after a CRD upgrade) move under
                // its new columns, so they stay laid out in what `columns` says.
                if let (Some(rows), Some(defs)) = (self.rows.get_mut(ctx), self.columns.get_mut(ctx)) {
                    if !rows.is_empty() && !same_names(defs, &cols) {
                        *rows = relayout(rows, defs, &cols);
                        up.rows = true;
                    }
                    *defs = cols.clone();
                }
                // The table shows the first cluster's columns (or the first to come); the
                // other clusters' rows are laid out under them by column name. A relist
                // sends them again: the table keeps the user's sort and column order.
                if shown.is_empty() || ctx == 0 {
                    let changed = !same_names(shown, &cols);
                    up.columns = Some(cols);
                    // New columns sort the rows again: the highlight stays on its object.
                    up.rows |= changed && self.rows.iter().any(|r| !r.is_empty());
                    up.follow = changed;
                }
            }
            TableMsg::Rows(rows) => {
                // The notice holds while another cluster is still listing: its rows are not
                // there yet, or are the old ones.
                if !self.listing.iter().any(|&ix| ix != ctx) {
                    self.notice = None;
                }
                self.errors.remove(&ctx);
                if let Some(slot) = self.rows.get_mut(ctx) {
                    *slot = rows;
                }
                up.rows = true;
            }
            TableMsg::Listed => {
                self.listing.remove(&ctx);
                up.loaded = self.finish();
            }
            TableMsg::Error(e) => {
                self.errors.insert(ctx, format!("{name}: {e} (retrying)").into());
            }
            TableMsg::Forbidden(e) => {
                self.errors.insert(ctx, format!("{name}: your access does not allow listing this type ({e})").into());
                self.listing.remove(&ctx);
                self.refused.insert(ctx);
                // The watch ends here: rows listed before (or cached) would look live.
                if let Some(slot) = self.rows.get_mut(ctx).filter(|s| !s.is_empty()) {
                    slot.clear();
                    up.rows = true;
                }
                up.loaded = self.finish();
                // Forbidden only when every cluster watched refused, whichever answers last.
                up.forbidden = up.loaded.then_some(self.refused.len() >= self.watched);
            }
        }
        up
    }

    /// Whether every cluster has answered; the notice goes then.
    fn finish(&mut self) -> bool {
        let done = self.listing.is_empty();
        if done {
            self.notice = None;
        }
        done
    }
}

/// Drop rows that newer rows in the same batch replace; everything else keeps its order.
pub(super) fn coalesce(batch: Vec<TableMsg>) -> Vec<TableMsg> {
    let last_rows = batch.iter().rposition(|m| matches!(m, TableMsg::Rows(_)));
    batch
        .into_iter()
        .enumerate()
        .filter(|(i, m)| !matches!(m, TableMsg::Rows(_)) || Some(*i) == last_rows)
        .map(|(_, m)| m)
        .collect()
}

fn same_names(a: &[ColumnDef], b: &[ColumnDef]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name == y.name)
}

/// Rows whose cells are laid out in `from`, moved under `to` by column name: a column
/// `from` lacks is empty. Rows with no known columns on either side stay as they are.
fn relayout(rows: &[Arc<Row>], from: &[ColumnDef], to: &[ColumnDef]) -> Vec<Arc<Row>> {
    if from.is_empty() || to.is_empty() || same_names(from, to) {
        return rows.to_vec();
    }
    let at: Vec<Option<usize>> = to.iter().map(|d| from.iter().position(|f| f.name == d.name)).collect();
    rows.iter()
        .map(|r| {
            Arc::new(Row {
                uid: r.uid.clone(),
                name: r.name.clone(),
                namespace: r.namespace.clone(),
                created: r.created,
                cells: at.iter().map(|i| i.and_then(|i| r.cells.get(i)).cloned().unwrap_or_default()).collect(),
                labels: r.labels.clone(),
                deleting: r.deleting,
                deleted_at: r.deleted_at,
            })
        })
        .collect()
}

/// Empty the rows of the clusters not in `watched`.
fn keep_watched<T>(rows: &mut [Vec<T>], watched: &HashSet<usize>) {
    for (ix, slot) in rows.iter_mut().enumerate() {
        if !watched.contains(&ix) {
            slot.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{LiveList, SharedString, TableMsg, Update, coalesce, keep_watched, relayout};
    use crate::k8s::{ColumnDef, Row};

    fn defs(names: &[&str]) -> Arc<Vec<ColumnDef>> {
        Arc::new(names.iter().map(|n| ColumnDef { name: n.to_string(), priority: 0 }).collect())
    }

    fn row_with(name: &str, cells: &[&str]) -> Arc<Row> {
        Arc::new(Row {
            uid: name.into(),
            name: name.into(),
            namespace: None,
            created: None,
            cells: cells.iter().map(|c| (*c).into()).collect(),
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        })
    }

    fn row(name: &str) -> Arc<Row> {
        row_with(name, &[name, "1d"])
    }

    fn names(list: &LiveList, columns: &[ColumnDef]) -> Vec<String> {
        list.merged(columns).iter().map(|(c, r)| format!("{c}:{}", r.name)).collect()
    }

    #[test]
    fn the_old_list_notice_stays_while_another_cluster_lists() {
        let mut list = LiveList::start(2, [0, 1]);
        assert_eq!(list.notice.as_deref(), Some("Loading…"));
        list.apply(0, "a", TableMsg::Rows(vec![row("x")]), &[]);
        assert_eq!(list.notice.as_deref(), Some("Loading…"), "b has not answered");
        // A failing cluster's error does not hide the notice: its rows may be the old ones.
        list.apply(1, "b", TableMsg::Error("timeout".into()), &[]);
        assert_eq!(list.status_line(None).as_deref(), Some("b: timeout (retrying); Loading…"));
        let failed = SharedString::from("c: unreachable");
        assert_eq!(list.status_line(Some(&failed)).as_deref(), Some("c: unreachable; b: timeout (retrying); Loading…"));
        // Its rows clear its error; the notice stays while a is still listing.
        list.apply(1, "b", TableMsg::Rows(vec![row("y")]), &[]);
        assert_eq!(list.status_line(None).as_deref(), Some("Loading…"));
        list.apply(0, "a", TableMsg::Listed, &[]);
        // Nobody else is listing now: b's rows end the notice.
        list.apply(1, "b", TableMsg::Rows(vec![row("y")]), &[]);
        assert_eq!(list.status_line(None), None);
        assert!(list.is_loading(), "until b says it listed everything");
        let up = list.apply(1, "b", TableMsg::Listed, &[]);
        assert!(up.loaded && !list.is_loading());
    }

    #[test]
    fn forbidden_only_when_every_cluster_refused_whichever_answers_last() {
        for order in [[0, 1], [1, 0]] {
            let mut list = LiveList::start(2, [0, 1]);
            let first = list.apply(order[0], "a", TableMsg::Forbidden("403".into()), &[]);
            assert_eq!((first.loaded, first.forbidden), (false, None));
            let last = list.apply(order[1], "b", TableMsg::Listed, &[]);
            assert!(last.loaded && last.forbidden.is_none(), "one cluster listed: not forbidden");

            let mut list = LiveList::start(2, [0, 1]);
            list.apply(order[0], "a", TableMsg::Forbidden("403".into()), &[]);
            let last = list.apply(order[1], "b", TableMsg::Forbidden("403".into()), &[]);
            assert_eq!(last.forbidden, Some(true));
        }
        // A cluster that does not serve the kind is not waited for, nor counted.
        let mut list = LiveList::start(3, [2]);
        assert_eq!(list.apply(2, "c", TableMsg::Forbidden("403".into()), &[]).forbidden, Some(true));
    }

    #[test]
    fn a_refusal_clears_rows_that_would_look_live() {
        let mut list = LiveList::start(1, [0]);
        list.apply(0, "a", TableMsg::Rows(vec![row("x")]), &[]);
        let up = list.apply(0, "a", TableMsg::Forbidden("403".into()), &[]);
        assert!(up.rows);
        assert!(list.merged(&[]).is_empty());
        assert!(list.snapshot(defs(&["Name"]), false).is_none(), "nothing to keep");
    }

    #[test]
    fn the_table_takes_the_first_clusters_columns() {
        let cols = defs(&["Name", "Age"]);
        let mut list = LiveList::start(2, [0, 1]);
        // The second cluster answers first: the table has nothing yet, it takes them.
        let up = list.apply(1, "b", TableMsg::Columns(cols.clone()), &[]);
        assert_eq!(up, Update { columns: Some(cols.clone()), follow: true, ..Update::default() });
        // Then the first cluster's columns win, once the table has some.
        let other = defs(&["Name", "Ready", "Age"]);
        assert!(list.apply(0, "a", TableMsg::Columns(other.clone()), &cols).columns.is_some());
        assert!(list.apply(1, "b", TableMsg::Columns(cols.clone()), &other).columns.is_none());
        // The same columns again (a relist): taken, without sorting or merging again.
        let up = list.apply(0, "a", TableMsg::Columns(other.clone()), &other);
        assert_eq!(up, Update { columns: Some(other), ..Update::default() });
    }

    #[test]
    fn rows_move_under_a_clusters_new_columns() {
        let mut list = LiveList::start(1, [0]);
        let (old, new) = (defs(&["Name", "Age"]), defs(&["Age", "Name"]));
        list.apply(0, "a", TableMsg::Columns(old.clone()), &[]);
        list.apply(0, "a", TableMsg::Rows(vec![row("x")]), &old);
        let up = list.apply(0, "a", TableMsg::Columns(new.clone()), &old);
        assert!(up.rows && up.columns.is_some());
        assert_eq!(list.merged(&new)[0].1.cells, ["1d", "x"]);
        assert_eq!(names(&list, &new), ["0:x"]);
    }

    #[test]
    fn a_cached_list_shows_only_the_clusters_watched() {
        let cols = defs(&["Name", "Age"]);
        let mut list = LiveList::start(3, [0, 2]);
        list.restore(super::CachedView {
            columns: cols.clone(),
            rows: vec![vec![row("p")], vec![row("s")], vec![row("d")]],
            at: std::time::Instant::now(),
        });
        assert_eq!(names(&list, &cols), ["0:p", "2:d"]);
        assert!(list.snapshot(cols.clone(), false).is_none(), "still loading");
        list.apply(0, "a", TableMsg::Listed, &cols);
        list.apply(2, "c", TableMsg::Listed, &cols);
        let kept = list.snapshot(cols, false).unwrap();
        assert_eq!(kept.rows.iter().map(Vec::len).collect::<Vec<_>>(), [1, 0, 1]);
    }

    #[test]
    fn cells_move_under_the_shown_columns() {
        let rows = vec![row_with("web", &["web", "True", "le", "1d"])];
        // Another operator release: Issuer before Ready, and no Status column.
        let from = defs(&["Name", "Ready", "Issuer", "Age"]);
        let to = defs(&["Name", "Issuer", "Ready", "Status", "Age"]);
        let moved = relayout(&rows, &from, &to);
        assert_eq!(moved[0].cells, ["web", "le", "True", "", "1d"]);
        assert_eq!(moved[0].uid, "web");
        // Same columns, or none known: the rows themselves.
        assert!(Arc::ptr_eq(&relayout(&rows, &from, &from)[0], &rows[0]));
        assert!(Arc::ptr_eq(&relayout(&rows, &[], &to)[0], &rows[0]));
        assert!(Arc::ptr_eq(&relayout(&rows, &from, &[])[0], &rows[0]));
    }

    #[test]
    fn rows_of_clusters_not_watched_are_dropped() {
        let mut rows = vec![vec!["prod-pod"], vec!["staging-pod"], vec!["dev-pod"]];
        keep_watched(&mut rows, &[0, 2].into_iter().collect());
        assert_eq!(rows, [vec!["prod-pod"], vec![], vec!["dev-pod"]]);
    }

    #[test]
    fn coalesces_rows() {
        let rows = |n: usize| TableMsg::Rows((0..n).map(|i| row(&i.to_string())).collect());
        let batch = vec![rows(1), TableMsg::Columns(Arc::default()), rows(2), TableMsg::Listed, rows(3)];
        let kept: Vec<String> = coalesce(batch)
            .iter()
            .map(|m| match m {
                TableMsg::Rows(r) => format!("rows{}", r.len()),
                TableMsg::Columns(_) => "columns".into(),
                TableMsg::Listed => "listed".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(kept, ["columns", "listed", "rows3"]);
        assert_eq!(coalesce(vec![TableMsg::Error("x".into())]).len(), 1);
    }
}
