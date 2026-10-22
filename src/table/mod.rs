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
#[allow(unused_imports)]
pub(crate) use filter::*;
#[allow(unused_imports)]
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
    pub selected_uid: Option<String>,
    usage: Option<HashMap<usize, UsageIndex>>,
    /// When several namespaces are selected, rows outside them are hidden.
    ns_filter: Option<std::collections::HashSet<String>>,
    /// Allocatable per context and node, for usage bars.
    capacity: Option<HashMap<usize, NodeCapacity>>,
    visible_cols: Option<Vec<String>>,
    /// Where row menu actions are dispatched (the app view).
    pub app_focus: Option<FocusHandle>,
    /// Kind is a Pod: offer logs and shell in the row menu.
    pub pods: bool,
    /// Which write actions the row menu offers for this kind.
    pub row_actions: Vec<&'static str>,
    /// Rows checked for bulk actions, by uid.
    pub marked: std::collections::HashSet<String>,
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
            selected_uid: None,
            usage: None,
            ns_filter: None,
            capacity: None,
            visible_cols: None,
            app_focus: None,
            pods: false,
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

    /// Checked rows that pass the filter, with their context index.
    pub fn marked_visible_rows(&self) -> Vec<(usize, Arc<Row>)> {
        self.visible
            .iter()
            .filter(|&&i| self.marked.contains(&self.rows[i].uid))
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
            .filter(|(r, _)| self.marked.contains(&r.uid))
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
        let all = !self.visible.is_empty() && self.visible.iter().all(|&i| self.marked.contains(&self.rows[i].uid));
        if all {
            self.marked.clear();
        } else {
            self.marked.extend(self.visible.iter().map(|&i| self.rows[i].uid.clone()));
        }
    }

    pub fn reset(
        &mut self,
        show_namespace: bool,
        contexts: Vec<String>,
        visible_cols: Option<Vec<String>>,
        widths: HashMap<String, f32>,
    ) {
        let (app_focus, pods, row_actions) = (self.app_focus.take(), self.pods, std::mem::take(&mut self.row_actions));
        *self = Self { show_namespace, contexts, visible_cols, widths, app_focus, pods, row_actions, ..Self::new() };
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
            .map(|i| i + 1)
            .unwrap_or(self.cols.len());
        if mem {
            self.cols.insert(at, Col::Memory);
        }
        if cpu {
            self.cols.insert(at, Col::Cpu);
        }
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
        self.recompute();
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

    pub fn visible_index_of(&self, uid: &str) -> Option<usize> {
        self.visible.iter().position(|&i| self.rows[i].uid == uid)
    }

    pub fn total(&self) -> usize {
        self.rows.len()
    }

    pub fn rows_count_visible(&self) -> usize {
        self.visible.len()
    }

    fn recompute(&mut self) {
        let filter = &self.parsed;
        // Column names as typed in filters: "restarts", "nominatednode", "cpu", "age".
        let named: Vec<(String, Col)> = if filter.has_columns() {
            self.cols.iter().filter(|c| **c != Col::Check).map(|c| (normalized(self.header(*c)), *c)).collect()
        } else {
            Vec::new()
        };
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
