//! Table delegate for a live resource list.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::table::ColumnSort;
use gpui_kit::*;

use crate::k8s::{self, ColumnDef, Row};

mod filter;
mod render;
mod sort;

pub(crate) use filter::*;
pub(crate) use render::*;
use sort::normalized;

#[derive(Clone, Copy, PartialEq)]
enum Col {
    /// Checkbox for bulk actions.
    Check,
    Context,
    Namespace,
    Cpu,
    Memory,
    Cell(usize),
    Age(usize),
}

/// Allocatable (millicores, bytes) by node name.
type NodeCapacity = HashMap<String, (f64, f64)>;

/// Usage by namespace then name: looked up with the row's own strings, no key to build.
type UsageIndex = HashMap<String, HashMap<String, (f64, f64)>>;

pub struct ResourceTable {
    defs: Arc<Vec<ColumnDef>>,
    /// The kind each column's values name, by column definition (a Secret, a Node…).
    def_links: Vec<Option<&'static str>>,
    cols: Vec<Col>,
    show_namespace: bool,
    rows: Vec<Arc<Row>>,
    /// Context index of each row (parallel to `rows`).
    row_ctx: Vec<usize>,
    /// Context names; a Context column shows when there are several.
    contexts: Vec<String>,
    /// Indices into `rows` after filtering and sorting.
    visible: Vec<usize>,
    filter: String,
    /// `filter`, parsed once (not on every update of the rows).
    parsed: RowFilter,
    sort: Option<(Col, ColumnSort)>,
    /// The selected row: (context index, uid).
    pub selected: Option<(usize, String)>,
    usage: Option<HashMap<usize, UsageIndex>>,
    /// Pods' requests and limits by context, namespace and name, for their usage bars.
    resources: HashMap<usize, HashMap<String, HashMap<String, k8s::PodResources>>>,
    /// When several namespaces are selected, rows outside them are hidden.
    ns_filter: Option<std::collections::HashSet<String>>,
    /// Allocatable per context and node, for usage bars.
    capacity: Option<HashMap<usize, NodeCapacity>>,
    visible_cols: Option<Vec<String>>,
    /// Where row menu actions are dispatched (the app view).
    pub app_focus: Option<FocusHandle>,
    /// Kind is a Pod: offer logs and shell in the row menu.
    pub pods: bool,
    /// Kind is an Ingress: its hosts open the site.
    pub ingresses: bool,
    /// Which write actions the row menu offers for this kind.
    pub row_actions: Vec<&'static str>,
    /// Rows checked for bulk actions: uids by context index (a fast first load's
    /// stand-in uids are the same on every cluster).
    pub marked: HashMap<usize, std::collections::HashSet<String>>,
    /// Saved column widths by header (at zoom 1).
    pub widths: HashMap<String, f32>,
    /// The first full list has not arrived yet.
    pub loading: bool,
    /// A namespace cell was clicked: the app filters on it.
    pub pending_namespace: Option<String>,
    /// Listing this type was refused (403) on every cluster watched.
    pub forbidden: bool,
    /// A linked cell was clicked: (row context, kind, name, namespace) to open.
    pub pending_open: Option<(usize, String, String, Option<String>)>,
    /// The row the context menu was opened on: (context index, uid). Row menu
    /// actions use it, so a re-sort or a new row meanwhile does not shift them.
    pub menu_row: Option<(usize, String)>,
    /// When the left button last went down on a row itself (not on a checkbox,
    /// link or header): the selection that follows comes from a click.
    pub row_pressed: Option<std::time::Instant>,
}

impl ResourceTable {
    pub fn new() -> Self {
        Self {
            defs: Arc::new(Vec::new()),
            def_links: Vec::new(),
            cols: Vec::new(),
            show_namespace: false,
            rows: Vec::new(),
            row_ctx: Vec::new(),
            contexts: Vec::new(),
            visible: Vec::new(),
            filter: String::new(),
            parsed: RowFilter::parse(""),
            sort: None,
            selected: None,
            usage: None,
            resources: HashMap::new(),
            ns_filter: None,
            capacity: None,
            visible_cols: None,
            app_focus: None,
            pods: false,
            ingresses: false,
            row_actions: Vec::new(),
            marked: Default::default(),
            widths: HashMap::new(),
            loading: false,
            pending_namespace: None,
            forbidden: false,
            pending_open: None,
            menu_row: None,
            row_pressed: None,
        }
    }

    /// Header names of the columns as shown, for saving widths and order.
    pub fn headers(&self) -> Vec<String> {
        self.cols.iter().map(|c| self.header(*c).to_string()).collect()
    }

    /// Movable columns in display order (what the column choice saves).
    pub fn movable_order(&self) -> Vec<String> {
        let mut order: Vec<String> =
            self.cols.iter().filter(|c| !self.is_fixed(**c)).map(|c| self.header(*c).to_string()).collect();
        order.extend(self.pending_usage_cols());
        order
    }

    /// Shown column names (what the column choice saves), including CPU and
    /// Memory when metrics have not arrived yet but the user keeps them.
    pub fn shown_columns(&self) -> Vec<String> {
        let mut shown: Vec<String> =
            self.column_choices().into_iter().filter(|(_, shown)| *shown).map(|(n, _)| n).collect();
        shown.extend(self.pending_usage_cols());
        shown
    }

    /// Usage columns that will show once metrics arrive: saving a column list
    /// before then must not drop them.
    fn pending_usage_cols(&self) -> Vec<String> {
        if self.usage.is_some() {
            return Vec::new();
        }
        ["CPU", "Memory"]
            .into_iter()
            .filter(|n| !self.defs.iter().any(|d| d.name == *n))
            .filter(|n| self.visible_cols.as_ref().is_none_or(|v| v.iter().any(|c| c == n)))
            .map(String::from)
            .collect()
    }

    /// The column order was changed by dragging: later rebuilds keep it.
    pub fn remember_order(&mut self, order: Vec<String>) {
        self.visible_cols = Some(order);
    }

    fn is_fixed(&self, c: Col) -> bool {
        matches!(c, Col::Check | Col::Context | Col::Namespace | Col::Cell(0))
    }

    /// Whether the row of this context and uid is checked.
    pub fn is_marked(&self, ctx: usize, uid: &str) -> bool {
        self.marked.get(&ctx).is_some_and(|m| m.contains(uid))
    }

    /// Check or uncheck a row.
    pub fn toggle_mark(&mut self, ctx: usize, uid: &str) {
        let m = self.marked.entry(ctx).or_default();
        if !m.remove(uid) {
            m.insert(uid.to_string());
        }
    }

    /// Checked rows that pass the filter, with their context index.
    pub fn marked_visible_rows(&self) -> Vec<(usize, Arc<Row>)> {
        self.visible
            .iter()
            .filter(|&&i| self.is_marked(self.row_ctx[i], &self.rows[i].uid))
            .map(|&i| (self.row_ctx[i], self.rows[i].clone()))
            .collect()
    }

    /// A row by context index and uid, shown or not.
    pub fn find_row(&self, ctx: usize, uid: &str) -> Option<Arc<Row>> {
        self.rows.iter().zip(&self.row_ctx).find(|(r, c)| **c == ctx && r.uid == uid).map(|(r, _)| r.clone())
    }

    /// Checked rows that still exist, with their context index.
    pub fn marked_rows(&self) -> Vec<(usize, Arc<Row>)> {
        self.rows
            .iter()
            .zip(&self.row_ctx)
            .filter(|(r, c)| self.is_marked(**c, &r.uid))
            .map(|(r, c)| (*c, r.clone()))
            .collect()
    }

    /// The visible rows and columns as CSV (what you see, in order).
    pub fn to_csv(&self) -> String {
        let cols: Vec<Col> = self.cols.iter().copied().filter(|c| *c != Col::Check).collect();
        let esc = |s: &str| {
            if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
        };
        let mut out = cols.iter().map(|c| esc(self.header(*c))).collect::<Vec<_>>().join(",");
        out.push('\n');
        for &i in &self.visible {
            let (row, ctx) = (&self.rows[i], self.row_ctx[i]);
            out.push_str(&cols.iter().map(|c| esc(&self.cell(ctx, row, *c))).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
        out
    }

    /// Check every row that passes the filter, or uncheck all if they all are.
    pub fn toggle_all_visible(&mut self) {
        let all = !self.visible.is_empty()
            && self.visible.iter().all(|&i| self.is_marked(self.row_ctx[i], &self.rows[i].uid));
        if all {
            self.marked.clear();
        } else {
            for &i in &self.visible {
                self.marked.entry(self.row_ctx[i]).or_default().insert(self.rows[i].uid.clone());
            }
        }
    }

    pub fn reset(
        &mut self,
        show_namespace: bool,
        contexts: Vec<String>,
        visible_cols: Option<Vec<String>>,
        widths: HashMap<String, f32>,
    ) {
        let (app_focus, pods, ingresses, row_actions) =
            (self.app_focus.take(), self.pods, self.ingresses, std::mem::take(&mut self.row_actions));
        *self = Self {
            show_namespace,
            contexts,
            visible_cols,
            app_focus,
            pods,
            ingresses,
            row_actions,
            widths,
            ..Self::new()
        };
    }

    /// Column names the user chose for this kind, or None for the defaults.
    pub fn set_visible_columns(&mut self, visible: Option<Vec<String>>) {
        self.visible_cols = visible;
        let defs = self.defs.clone();
        self.set_columns(defs);
    }

    /// Every column this kind offers, with whether it is shown.
    pub fn column_choices(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self
            .defs
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, d)| {
                (d.name.clone(), self.cols.iter().any(|c| matches!(c, Col::Cell(x) | Col::Age(x) if *x == i)))
            })
            .collect();
        if self.usage.is_some() {
            out.push(("CPU".into(), self.cols.contains(&Col::Cpu)));
            out.push(("Memory".into(), self.cols.contains(&Col::Memory)));
        }
        out
    }

    fn default_visible(d: &ColumnDef) -> bool {
        // Like `kubectl get` without `-o wide`, but Node is too useful to hide.
        d.priority == 0 || d.name == "Node"
    }

    /// Columns from the server. A relist sends the same ones again: keep the
    /// order, sort and usage columns the table has then.
    /// The columns the server sent for this kind.
    pub fn defs(&self) -> Arc<Vec<ColumnDef>> {
        self.defs.clone()
    }

    pub fn update_columns(&mut self, defs: Arc<Vec<ColumnDef>>) {
        if !self.cols.is_empty() && same_names(&self.defs, &defs) {
            self.set_defs(defs);
            return;
        }
        self.set_columns(defs);
    }

    fn set_defs(&mut self, defs: Arc<Vec<ColumnDef>>) {
        self.def_links = defs.iter().map(|d| link_kind_of(&d.name)).collect();
        self.defs = defs;
    }

    pub fn set_columns(&mut self, defs: Arc<Vec<ColumnDef>>) {
        let same = same_names(&self.defs, &defs);
        let mut cols = vec![Col::Check];
        if self.contexts.len() > 1 {
            cols.push(Col::Context);
        }
        for (i, d) in defs.iter().enumerate() {
            if i == 0 && self.show_namespace {
                cols.push(Col::Namespace);
            }
            // The name column always stays.
            let shown = i == 0
                || match &self.visible_cols {
                    Some(v) => v.contains(&d.name),
                    None => Self::default_visible(d),
                };
            if !shown {
                continue;
            }
            cols.push(if d.name == "Age" { Col::Age(i) } else { Col::Cell(i) });
        }
        if defs.is_empty() && self.show_namespace {
            cols.push(Col::Namespace);
        }
        // The saved choice also keeps the order the user dragged columns into.
        if let Some(order) = &self.visible_cols {
            let fixed = cols
                .iter()
                .take_while(|c| matches!(c, Col::Check | Col::Context | Col::Namespace | Col::Cell(0)))
                .count();
            let pos = |c: &Col| match c {
                Col::Cell(i) | Col::Age(i) => order.iter().position(|n| *n == defs[*i].name).unwrap_or(usize::MAX),
                _ => usize::MAX,
            };
            cols[fixed..].sort_by_key(pos);
        }
        self.set_defs(defs);
        self.cols = cols;
        self.insert_usage_cols();
        // Same columns (one shown or hidden): keep sorting by the column if it is still there.
        self.sort = self.sort.filter(|(c, _)| same && self.cols.contains(c));
        self.recompute();
    }

    /// Node usage with allocatable; shown as bars with a percentage.
    pub fn set_node_usage(&mut self, ctx: usize, usage: k8s::NodeUsage) -> bool {
        let pod_like: k8s::PodUsage = usage.iter().map(|(n, (u, _))| ((String::new(), n.clone()), *u)).collect();
        let capacity = self.capacity.get_or_insert_default();
        capacity.insert(ctx, usage.into_iter().map(|(n, (_, cap))| (n, cap)).collect());
        self.set_usage(ctx, pod_like)
    }

    /// Pod usage from metrics-server; adds CPU and Memory columns after Status.
    pub fn set_usage(&mut self, ctx: usize, usage: k8s::PodUsage) -> bool {
        let added = self.usage.is_none();
        let mut index = UsageIndex::new();
        for ((ns, name), u) in usage {
            index.entry(ns).or_default().insert(name, u);
        }
        self.usage.get_or_insert_default().insert(ctx, index);
        if added {
            self.insert_usage_cols();
        }
        self.recompute();
        added
    }

    fn insert_usage_cols(&mut self) {
        if self.usage.is_none() || self.cols.contains(&Col::Cpu) || self.cols.is_empty() {
            return;
        }
        let wants = |name: &str| self.visible_cols.as_ref().is_none_or(|v| v.iter().any(|c| c == name));
        let (cpu, mem) = (wants("CPU"), wants("Memory"));
        let at = self
            .cols
            .iter()
            .position(|c| matches!(c, Col::Cell(i) if self.defs[*i].name == "Status"))
            .map_or(self.cols.len(), |i| i + 1);
        if mem {
            self.cols.insert(at, Col::Memory);
        }
        if cpu {
            self.cols.insert(at, Col::Cpu);
        }
    }

    /// Pods' requests and limits, for the usage bars.
    pub fn set_resources(&mut self, ctx: usize, resources: k8s::PodResourcesMap) {
        let mut index: HashMap<String, HashMap<String, k8s::PodResources>> = HashMap::new();
        for ((ns, name), r) in resources {
            index.entry(ns).or_default().insert(name, r);
        }
        self.resources.insert(ctx, index);
    }

    fn resources_of(&self, ctx: usize, row: &Row) -> Option<k8s::PodResources> {
        let ns = row.namespace.as_deref().unwrap_or_default();
        self.resources.get(&ctx)?.get(ns)?.get(row.name.as_str()).copied()
    }

    fn usage_of(&self, ctx: usize, row: &Row) -> Option<(f64, f64)> {
        let ns = row.namespace.as_deref().unwrap_or_default();
        self.usage.as_ref()?.get(&ctx)?.get(ns)?.get(row.name.as_str()).copied()
    }

    /// Allocatable of a node row, for its usage bars.
    fn capacity_of(&self, ctx: usize, row: &Row) -> Option<(f64, f64)> {
        self.capacity.as_ref()?.get(&ctx)?.get(row.name.as_str()).copied()
    }

    /// Rows from every selected context, tagged with the context index.
    pub fn set_rows(&mut self, rows: Vec<(usize, Arc<Row>)>) {
        (self.row_ctx, self.rows) = rows.into_iter().unzip();
        self.follow_real_uids();
        self.recompute();
    }

    /// Rows of a big list's fast first load stand in namespace/name for the uid
    /// (`Row::is_partial`): once the full rows bring the real uids, checks, the
    /// selection and the row menu move to them, each on its own cluster's row.
    fn follow_real_uids(&mut self) {
        let checked = self.marked.iter().flat_map(|(c, m)| m.iter().map(move |u| (*c, u)));
        let picked = self.selected.iter().chain(&self.menu_row).map(|(c, u)| (*c, u));
        let wanted: Vec<(usize, String)> =
            checked.chain(picked).filter(|(_, u)| u.contains('/')).map(|(c, u)| (c, u.clone())).collect();
        if wanted.is_empty() {
            return;
        }
        let keys: HashMap<(usize, &str, &str), &str> = wanted
            .iter()
            .filter_map(|(c, u)| u.split_once('/').map(|(ns, name)| ((*c, ns, name), u.as_str())))
            .collect();
        // (context, stand-in uid) -> the real uid of the row it names there.
        let mut real: HashMap<(usize, &str), String> = HashMap::new();
        let mut partial = false;
        for (r, &ctx) in self.rows.iter().zip(&self.row_ctx) {
            if r.is_partial() {
                partial = true;
                continue;
            }
            if let Some(stand_in) = keys.get(&(ctx, r.namespace.as_deref().unwrap_or_default(), r.name.as_str())) {
                real.insert((ctx, stand_in), r.uid.clone());
            }
        }
        for ((ctx, stand_in), uid) in real {
            if let Some(m) = self.marked.get_mut(&ctx)
                && m.remove(stand_in)
            {
                m.insert(uid.clone());
            }
            for picked in [&mut self.selected, &mut self.menu_row].into_iter().flatten() {
                if picked.0 == ctx && picked.1 == stand_in {
                    picked.1 = uid.clone();
                }
            }
        }
        // Every row is full: a stand-in left names an object gone meanwhile. Dropped, so
        // later updates don't look for it again.
        if !partial {
            self.marked.values_mut().for_each(|m| m.retain(|u| !u.contains('/')));
            self.selected.take_if(|(_, u)| u.contains('/'));
            self.menu_row.take_if(|(_, u)| u.contains('/'));
        }
    }

    pub fn row_context(&self, visible_ix: usize) -> Option<usize> {
        self.visible.get(visible_ix).map(|&i| self.row_ctx[i])
    }

    pub fn set_namespace_filter(&mut self, filter: Option<std::collections::HashSet<String>>) {
        self.ns_filter = filter;
        self.recompute();
    }

    pub fn set_filter(&mut self, filter: String) {
        if filter != self.filter {
            self.parsed = RowFilter::parse(&filter);
            self.filter = filter;
        }
        self.recompute();
    }

    pub fn row(&self, visible_ix: usize) -> Option<&Arc<Row>> {
        self.visible.get(visible_ix).map(|&i| &self.rows[i])
    }

    /// Where the row of this context and uid is shown, if it is.
    pub fn visible_index_of(&self, ctx: usize, uid: &str) -> Option<usize> {
        self.visible.iter().position(|&i| self.row_ctx[i] == ctx && self.rows[i].uid == uid)
    }

    pub fn total(&self) -> usize {
        self.rows.len()
    }

    pub fn rows_count_visible(&self) -> usize {
        self.visible.len()
    }

    fn recompute(&mut self) {
        #[cfg(feature = "dev")]
        let _t = crate::dev::timing::span("table filter+sort");
        let filter = &self.parsed;
        // Column names as typed in filters: "restarts", "nominatednode", "cpu", "age".
        let named: Vec<(String, Col)> = if filter.has_columns() { self.filter_columns() } else { Vec::new() };
        self.visible = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                if let (Some(set), Some(ns)) = (&self.ns_filter, &r.namespace)
                    && !set.contains(ns)
                {
                    return false;
                }
                if named.is_empty() {
                    return filter.matches(r);
                }
                let ctx = self.row_ctx[i];
                filter.matches_with(r, &|name: &str| {
                    named.iter().find(|(n, _)| n == name).map(|(_, c)| self.cell(ctx, r, *c))
                })
            })
            .collect();
        self.sort_visible();
    }

    /// Every column a filter can name, shown or hidden (`ip:10.4.2.17` with IP hidden).
    fn filter_columns(&self) -> Vec<(String, Col)> {
        let mut cols = vec![Col::Context, Col::Namespace];
        cols.extend(
            self.defs.iter().enumerate().map(|(i, d)| if d.name == "Age" { Col::Age(i) } else { Col::Cell(i) }),
        );
        if self.usage.is_some() {
            cols.extend([Col::Cpu, Col::Memory]);
        }
        cols.into_iter().map(|c| (normalized(self.header(c)), c)).collect()
    }

    /// A cell's text; borrowed from the row when it is stored as shown.
    fn cell<'a>(&'a self, ctx: usize, row: &'a Row, col: Col) -> Cow<'a, str> {
        match col {
            Col::Check => Cow::Borrowed(""),
            Col::Context => Cow::Borrowed(self.contexts.get(ctx).map(String::as_str).unwrap_or_default()),
            Col::Namespace => Cow::Borrowed(row.namespace.as_deref().unwrap_or_default()),
            Col::Cpu => self.usage_of(ctx, row).map(|u| k8s::format_cpu(u.0)).unwrap_or_default().into(),
            Col::Memory => self.usage_of(ctx, row).map(|u| k8s::format_bytes(u.1)).unwrap_or_default().into(),
            Col::Age(i) => match row.created {
                Some(t) => k8s::age(t).into(),
                None => Cow::Borrowed(row.cells.get(i).map(String::as_str).unwrap_or_default()),
            },
            Col::Cell(i) => Cow::Borrowed(row.cells.get(i).map(String::as_str).unwrap_or_default()),
        }
    }

    /// A cell by column name (from the server's column definitions).
    pub fn cells_text<'a>(&self, row: &'a Row, name: &str) -> Option<&'a str> {
        let i = self.defs.iter().position(|d| d.name == name)?;
        row.cells.get(i).map(String::as_str)
    }

    fn header(&self, col: Col) -> &str {
        match col {
            Col::Check => "",
            Col::Context => "Context",
            Col::Namespace => "Namespace",
            Col::Cpu => "CPU",
            Col::Memory => "Memory",
            Col::Age(i) | Col::Cell(i) => &self.defs[i].name,
        }
    }

    /// The kind a column's values name (see `link_kind_of`), from the cached headers.
    fn link_kind(&self, col: Col) -> Option<&'static str> {
        match col {
            Col::Cell(i) | Col::Age(i) => self.def_links.get(i).copied().flatten(),
            _ => None,
        }
    }
}

fn same_names(a: &[ColumnDef], b: &[ColumnDef]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name == y.name)
}

#[cfg(test)]
mod uid_tests {
    use super::{Arc, ResourceTable, Row};

    fn row(uid: &str, name: &str) -> (usize, Arc<Row>) {
        let row = Row {
            uid: uid.into(),
            name: name.into(),
            namespace: Some("team-a".into()),
            created: None,
            cells: vec![name.into()],
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        };
        (0, Arc::new(row))
    }

    #[test]
    fn stand_in_uids_follow_the_full_rows() {
        let mut t = ResourceTable::new();
        t.set_rows(vec![row("team-a/web-1", "web-1"), row("team-a/web-2", "web-2"), row("team-a/web-3", "web-3")]);
        t.toggle_mark(0, "team-a/web-1");
        t.toggle_mark(0, "team-a/web-2");
        t.selected = Some((0, "team-a/web-2".into()));
        t.menu_row = Some((0, "team-a/web-3".into()));
        // Still the fast rows (a later page): nothing moves.
        t.set_rows(vec![row("team-a/web-1", "web-1"), row("team-a/web-2", "web-2"), row("team-a/web-3", "web-3")]);
        assert!(t.is_marked(0, "team-a/web-1"));
        t.set_rows(vec![row("u1", "web-1"), row("u2", "web-2"), row("u3", "web-3")]);
        let mut marked: Vec<&str> = t.marked[&0].iter().map(String::as_str).collect();
        marked.sort_unstable();
        assert_eq!(marked, ["u1", "u2"]);
        assert_eq!(t.selected, Some((0, "u2".into())));
        assert_eq!(t.menu_row, Some((0, "u3".into())));
        assert_eq!(t.marked_rows().len(), 2);
        // Checked, then deleted before the full rows came: forgotten.
        t.toggle_mark(0, "team-a/gone");
        t.set_rows(vec![row("u1", "web-1")]);
        assert!(!t.is_marked(0, "team-a/gone"));
    }

    #[test]
    fn stand_in_uids_stay_on_their_cluster() {
        let on = |ctx: usize, (_, r): (usize, Arc<Row>)| (ctx, r);
        let mut t = ResourceTable::new();
        t.set_rows(vec![on(0, row("team-a/web-1", "web-1")), on(1, row("team-a/web-1", "web-1"))]);
        // Checked and selected on the second cluster only.
        t.toggle_mark(1, "team-a/web-1");
        t.selected = Some((1, "team-a/web-1".into()));
        assert!(!t.is_marked(0, "team-a/web-1"));
        assert_eq!(t.visible_index_of(1, "team-a/web-1"), Some(1));
        t.set_rows(vec![on(0, row("prod-uid", "web-1")), on(1, row("staging-uid", "web-1"))]);
        assert_eq!(t.marked_rows().iter().map(|(c, r)| (*c, r.uid.as_str())).collect::<Vec<_>>(), [(1, "staging-uid")]);
        assert_eq!(t.selected, Some((1, "staging-uid".into())));
    }
}

#[cfg(test)]
mod table_tests {
    use super::{Arc, ColumnDef, ResourceTable, Row};

    fn def(name: &str, priority: i32) -> ColumnDef {
        ColumnDef { name: name.into(), priority }
    }

    fn row(name: &str, cells: &[&str]) -> Arc<Row> {
        Arc::new(Row {
            uid: name.into(),
            name: name.into(),
            namespace: Some("team-a".into()),
            created: None,
            cells: cells.iter().map(ToString::to_string).collect(),
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        })
    }

    #[test]
    fn filters_on_hidden_columns() {
        let mut t = ResourceTable::new();
        t.set_columns(Arc::new(vec![def("Name", 0), def("Status", 0), def("IP", 1), def("Nominated Node", 1)]));
        assert!(!t.headers().iter().any(|h| h == "IP"), "IP is hidden by default");
        t.set_rows(vec![
            (0, row("web-1", &["web-1", "Running", "10.4.2.17", "worker-3"])),
            (0, row("web-2", &["web-2", "Running", "10.4.2.18", "<none>"])),
        ]);
        let shown = |t: &mut ResourceTable, f: &str| {
            t.set_filter(f.into());
            t.rows_count_visible()
        };
        assert_eq!(shown(&mut t, "ip:10.4.2.17"), 1);
        assert_eq!(shown(&mut t, "nominatednode:worker-3"), 1);
        assert_eq!(shown(&mut t, "status:running"), 2);
        assert_eq!(shown(&mut t, "namespace:team-a"), 2);
        assert_eq!(shown(&mut t, "error:timeout"), 0, "not a column: text search");
    }
}
