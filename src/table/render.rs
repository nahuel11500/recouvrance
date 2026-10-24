//! Drawing the table: columns, cells (status dots, usage bars, links) and the row menu.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::ColumnSort;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Col, ResourceTable, normalized};
use crate::k8s;
use crate::k8s::{ColumnDef, Row};
use crate::status::{Tone, tone};

/// Keep the highlight on the selected object after its rows moved (an update, a filter,
/// a sort), and the row menu's highlight on its row. The table reports the move as a
/// selection: returns the row it moved to, for the app to skip (not a click).
pub fn follow_selection(
    t: &mut TableState<ResourceTable>,
    cx: &mut Context<TableState<ResourceTable>>,
) -> Option<usize> {
    let menu_open = t.right_clicked_row().is_some();
    let selected = t.delegate().selected.clone();
    let moved = match selected.and_then(|(c, uid)| t.delegate().visible_index_of(c, &uid)) {
        Some(ix) if t.selected_row() != Some(ix) => {
            // Without scrolling to it (which selecting does): the user may
            // have scrolled away, and rows moving must not pull the list back.
            let pending = t.vertical_scroll_handle.0.borrow_mut().deferred_scroll_to_item.take();
            t.set_selected_row(ix, cx);
            t.vertical_scroll_handle.0.borrow_mut().deferred_scroll_to_item = pending;
            Some(ix)
        }
        // Its row is gone or filtered out: no other row takes the highlight.
        None if t.selected_row().is_some() => {
            t.clear_selection(cx);
            None
        }
        _ => None,
    };
    // The row menu's highlight follows its row too (selecting clears it).
    if menu_open {
        let ix = t.delegate().menu_row.as_ref().and_then(|(c, uid)| t.delegate().visible_index_of(*c, uid));
        t.set_right_clicked_row(ix, cx);
    }
    moved
}

impl TableDelegate for ResourceTable {
    fn columns_count(&self, _: &App) -> usize {
        self.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.visible.len()
    }

    fn column(&self, col_ix: usize, cx: &App) -> Column {
        let col = self.cols[col_ix];
        if col == Col::Check {
            return Column::new("check", "").width(px(36. * crate::app::zoom(cx))).resizable(false);
        }
        let name = self.header(col).to_string();
        let saved = self.widths.get(&name).copied();
        let width = match (col, name.as_str()) {
            _ if saved.is_some() => saved.unwrap_or_default(),
            (_, "Name") => 340.,
            (_, "Message") => 640.,
            (_, "Object") => 320.,
            (_, "Last Seen" | "Type") => 90.,
            (Col::Namespace | Col::Context, _) => 170.,
            (Col::Cpu | Col::Memory, _) if self.capacity.is_some() || self.pods => 190.,
            (_, "Age" | "Ready" | "Restarts" | "Status" | "CPU" | "Memory") => 90.,
            _ => 150.,
        };
        // Room for the usage bars: widths saved before they existed were 90.
        let bars = matches!(col, Col::Cpu | Col::Memory) && (self.capacity.is_some() || self.pods);
        let width = if bars { width.max(170.) } else { width };
        // Not `.sort()`: the table's own control shows a two-way arrow on every column.
        // `render_th` sorts on a click and shows one arrow, on the sorted column only.
        Column::new(format!("c{col_ix}"), name).width(px(width * crate::app::zoom(cx))).movable(!self.is_fixed(col))
    }

    /// The header: a click sorts ascending, then descending, then back to the list order.
    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let col = self.cols[col_ix];
        if col == Col::Check {
            return div().into_any_element();
        }
        let sort = self.sort.filter(|(c, _)| *c == col).map(|(_, s)| s);
        h_flex()
            .id(("th", col_ix))
            .size_full()
            .gap_1()
            .justify_between()
            .child(div().overflow_hidden().text_ellipsis().child(self.header(col).to_string()))
            .when_some(sort, |this, s| {
                let icon = if s == ColumnSort::Ascending { Lucide::SortAscending } else { Lucide::SortDescending };
                this.child(Icon::new(icon).size_3().flex_none().text_color(cx.theme().secondary_foreground))
            })
            .on_click(cx.listener(move |t, _, _, cx| {
                let d = t.delegate_mut();
                d.sort = match sort {
                    None => Some((col, ColumnSort::Ascending)),
                    Some(ColumnSort::Ascending) => Some((col, ColumnSort::Descending)),
                    _ => None,
                };
                d.recompute();
                follow_selection(t, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    /// A press on the row itself (checkboxes, links and namespaces stop it first).
    fn render_tr(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        div().id(("row", row_ix)).on_mouse_down(
            MouseButton::Left,
            cx.listener(|t, _, _, _| t.delegate_mut().row_pressed = Some(std::time::Instant::now())),
        )
    }

    fn move_column(&mut self, col_ix: usize, to_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) {
        if col_ix < self.cols.len() && to_ix < self.cols.len() {
            let c = self.cols.remove(col_ix);
            self.cols.insert(to_ix, c);
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        #[cfg(feature = "dev")]
        let _t = crate::dev::timing::span("table cell");
        let (Some(row), Some(ctx)) = (self.row(row_ix), self.row_context(row_ix)) else {
            return div().into_any_element();
        };
        let col = self.cols[col_ix];
        if col == Col::Check {
            let uid = row.uid.clone();
            let checked = self.is_marked(ctx, &uid);
            return div()
                .id(SharedString::from(format!("mark-{ctx}-{uid}")))
                .size_full()
                .flex()
                .items_center()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |t, _, _, cx| {
                    t.delegate_mut().toggle_mark(ctx, &uid);
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(Checkbox::new(SharedString::from(format!("cb-{ctx}-{}", row.uid))).checked(checked))
                .into_any_element();
        }
        if let (Col::Cpu | Col::Memory, Some(cap)) = (col, self.capacity_of(ctx, row))
            && let Some(used) = self.usage_of(ctx, row)
        {
            let (value, max, color) = match col {
                Col::Cpu => (used.0, cap.0, cx.theme().blue),
                _ => (used.1, cap.1, cx.theme().success),
            };
            let pct = if max > 0.0 { (value / max).clamp(0.0, 1.0) } else { 0.0 };
            return div()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(56.)).flex_shrink_0().child(self.cell(ctx, row, col).into_owned()))
                        .child(
                            div()
                                .flex_1()
                                .h(px(6.))
                                .rounded_full()
                                .bg(cx.theme().secondary)
                                .child(div().h_full().rounded_full().bg(color).w(relative(pct as f32))),
                        )
                        .child(
                            div()
                                .w(px(34.))
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{:.0}%", pct * 100.0)),
                        ),
                )
                .into_any_element();
        }
        if let (Col::Cpu | Col::Memory, true) = (col, self.pods)
            && let Some(used) = self.usage_of(ctx, row)
        {
            let res = self.resources_of(ctx, row);
            return usage_bar(&row.uid, col, used, res, cx);
        }
        let cell = self.cell(ctx, row, col);
        let theme = cx.theme();
        // Clicking a namespace shows only that namespace.
        if col == Col::Namespace && !cell.is_empty() {
            let ns = cell.into_owned();
            let text = ns.clone();
            return div()
                .id(SharedString::from(format!("ns-{}", row.uid)))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .cursor_pointer()
                .hover(|s| s.underline())
                .when(row.deleting, |d| d.opacity(0.5))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |t, _, _, cx| {
                    t.delegate_mut().pending_namespace = Some(ns.clone());
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(text)
                .into_any_element();
        }
        // Cells that name another object (a CRD's Secret, a pod's Node…) open it.
        if let Some(kind) = self.link_kind(col).filter(|_| linkable(&cell)) {
            let (target, ns) = (cell.into_owned(), row.namespace.clone());
            let text = target.clone();
            return div()
                .id(SharedString::from(format!("lnk-{}-{col_ix}", row.uid)))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(theme.blue)
                .cursor_pointer()
                .hover(|s| s.underline())
                .when(row.deleting, |d| d.opacity(0.5))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |t, _, _, cx| {
                    t.delegate_mut().pending_open = Some((ctx, kind.to_string(), target.clone(), ns.clone()));
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(text)
                .into_any_element();
        }
        let header = self.header(col);
        // Pods: one dot per container (green ready, red failing, grey not ready).
        if self.pods
            && header == "Ready"
            && let Some((ready, total)) =
                cell.split_once('/').and_then(|(r, t)| Some((r.parse::<usize>().ok()?, t.parse::<usize>().ok()?)))
        {
            let status = self.cells_text(row, "Status").unwrap_or_default();
            let failing = !matches!(
                status,
                "Running"
                    | "Completed"
                    | "Succeeded"
                    | "Pending"
                    | "ContainerCreating"
                    | "PodInitializing"
                    | "Terminating"
            );
            let done = matches!(status, "Completed" | "Succeeded");
            return h_flex()
                .gap_1()
                .when(row.deleting, |d| d.opacity(0.5))
                .children((0..total.min(8)).map(|i| {
                    let c = if done {
                        theme.muted_foreground
                    } else if i < ready {
                        theme.success
                    } else if failing {
                        theme.danger
                    } else {
                        theme.muted_foreground.opacity(0.5)
                    };
                    div().size(px(8.)).rounded_sm().bg(c)
                }))
                .child(div().pl_1().text_xs().text_color(theme.muted_foreground).child(cell.into_owned()))
                .into_any_element();
        }
        if col == Col::Context {
            let dot: Hsla = crate::settings::context_color(&cell)
                .map(|c| rgb(c).into())
                .unwrap_or(theme.muted_foreground.opacity(0.4));
            return h_flex()
                .gap_2()
                .overflow_hidden()
                .child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(dot))
                .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(cell.into_owned()))
                .into_any_element();
        }
        let mut color = None;
        let mut weight = None;
        // "3 (2d4h ago)" -> "3".
        let mut shown: &str = &cell;
        match (col, header) {
            (Col::Cell(0), "Name") => {
                color = Some(theme.blue);
                weight = Some(FontWeight::MEDIUM);
            }
            (Col::Age(_), _) => color = Some(theme.muted_foreground),
            (_, "Status") => color = status_color(&cell, cx),
            (_, "Type") if cell == "Warning" => color = Some(theme.warning),
            (_, "Type") if cell == "Normal" => color = Some(theme.muted_foreground),
            (_, "Reason") if self.cells_text(row, "Type") == Some("Warning") => color = Some(theme.warning),
            (_, "Ready") => color = ready_color(&cell, row, &self.defs, cx),
            (_, "Restarts") => {
                // Orange only when recent or frequent.
                let last = cell.split_once('(').map(|(_, r)| r.trim_end_matches(')'));
                if let Some((n, _)) = cell.split_once(' ') {
                    shown = n;
                }
                let count: i64 = shown.parse().unwrap_or(0);
                let recent = last.is_some_and(|l| !l.contains('h') && !l.contains('d'));
                color = Some(if count >= 5 || (count > 0 && recent) {
                    theme.warning
                } else if count > 0 {
                    theme.foreground
                } else {
                    theme.muted_foreground
                });
            }
            _ => {}
        }
        let dot = (header == "Status" && !shown.is_empty()).then_some(color).flatten();
        // Being deleted: dimmed, and said so next to the name.
        let text = if row.deleting && col == Col::Cell(0) { format!("{shown}  (deleting)") } else { shown.to_string() };
        h_flex()
            .gap_1p5()
            .overflow_hidden()
            .when(row.deleting, |d| d.opacity(0.5))
            .when_some(dot, |d, c| d.child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(c)))
            .child(
                div()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .when_some(color, |d, c| d.text_color(c))
                    .when_some(weight, |d, w| d.font_weight(w))
                    .child(text),
            )
            .into_any_element()
    }

    fn context_menu(
        &mut self,
        _row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        use crate::app::{
            RowCopyName, RowDelete, RowEvents, RowFavorite, RowLogs, RowOpen, RowOpenTab, RowRestart, RowRun, RowScale,
            RowShell, RowYaml,
        };
        let menu = match &self.app_focus {
            Some(f) => menu.action_context(f.clone()),
            None => menu,
        };
        let menu = menu
            .menu_with_icon("Open", Lucide::PanelRightOpen, Box::new(RowOpen))
            .menu_with_icon("Open in new tab", Lucide::SquarePlus, Box::new(RowOpenTab))
            .when(self.pods, |m| {
                m.menu_with_icon("Logs", Lucide::ScrollText, Box::new(RowLogs)).menu_with_icon(
                    "Shell",
                    Lucide::SquareTerminal,
                    Box::new(RowShell),
                )
            })
            .menu_with_icon("YAML", Lucide::FileCode, Box::new(RowYaml))
            .menu_with_icon("Events", Lucide::Bell, Box::new(RowEvents))
            .separator()
            .menu_with_icon("Copy name", Lucide::Copy, Box::new(RowCopyName))
            .menu_with_icon("Add to favorites", Lucide::Star, Box::new(RowFavorite));
        let acts = &self.row_actions;
        menu.when(!acts.is_empty(), |m| m.separator())
            .when(acts.contains(&"run"), |m| m.menu_with_icon("Run now…", Lucide::Play, Box::new(RowRun)))
            .when(acts.contains(&"scale"), |m| m.menu_with_icon("Scale…", Lucide::ArrowUpDown, Box::new(RowScale)))
            .when(acts.contains(&"restart"), |m| m.menu_with_icon("Restart…", Lucide::RotateCw, Box::new(RowRestart)))
            .when(acts.contains(&"delete"), |m| m.menu_with_icon("Delete…", Lucide::Trash, Box::new(RowDelete)))
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let msg = if self.forbidden && self.rows.is_empty() {
            "Your access does not allow listing this type here"
        } else if self.loading && self.rows.is_empty() {
            "Loading…"
        } else if self.rows.is_empty() {
            "No resources"
        } else {
            "No matches"
        };
        div().size_full().flex().items_center().justify_center().text_color(cx.theme().muted_foreground).child(msg)
    }
}

fn status_color(status: &str, cx: &App) -> Option<Hsla> {
    let t = cx.theme();
    Some(match tone(status)? {
        Tone::Good => t.success,
        Tone::Done => t.muted_foreground,
        Tone::Busy => t.warning,
        Tone::Bad => t.danger,
    })
}

/// Green when all replicas/containers are ready, amber otherwise; muted for finished pods.
fn ready_color(text: &str, row: &Row, defs: &[ColumnDef], cx: &App) -> Option<Hsla> {
    let t = cx.theme();
    let (ready, total) = text.split_once('/')?;
    let (ready, total) = (ready.parse::<u32>().ok()?, total.parse::<u32>().ok()?);
    let finished = defs
        .iter()
        .position(|d| d.name == "Status")
        .and_then(|i| row.cells.get(i))
        .is_some_and(|s| s == "Completed" || s == "Succeeded");
    Some(if finished {
        t.muted_foreground
    } else if ready == total && total > 0 {
        t.success
    } else if total == 0 {
        t.muted_foreground
    } else {
        t.warning
    })
}

/// The kind a column's values name, from its header ("Secret", "SecretName", "Node",
/// "ClusterIssuer"…); None for plain values. "Issuer" and "Store" are not links: they may
/// name the namespaced kind or its cluster-wide twin (a ClusterIssuer, a ClusterSecretStore).
pub(super) fn link_kind_of(header: &str) -> Option<&'static str> {
    Some(match normalized(header).as_str() {
        "secret" | "secretname" | "secretref" | "targetsecret" | "tlssecret" => "secrets",
        "configmap" | "configmapname" => "configmaps",
        "service" | "servicename" => "services",
        "serviceaccount" | "serviceaccountname" => "serviceaccounts",
        "node" | "nodename" => "nodes",
        "pod" | "podname" => "pods",
        "deployment" => "deployments",
        "statefulset" => "statefulsets",
        "pvc" | "claim" | "persistentvolumeclaim" => "persistentvolumeclaims",
        "volume" | "persistentvolume" => "persistentvolumes",
        "storageclass" => "storageclasses",
        "clusterissuer" => "clusterissuers",
        "secretstore" => "secretstores",
        "clustersecretstore" => "clustersecretstores",
        _ => return None,
    })
}

/// Whether a cell names one object: lists, `<none>` and `-` are not links.
pub(super) fn linkable(value: &str) -> bool {
    let v = value.trim();
    !(v.is_empty() || v == "<none>" || v == "-" || v.contains([',', ' ', '/']))
}

/// A pod's CPU or memory: its usage as the bar, a dark tick at its request and a red one
/// at its limit. Orange above the request, red near the limit; exact figures on hover.
fn usage_bar(
    uid: &str,
    col: Col,
    used: (f64, f64),
    res: Option<k8s::PodResources>,
    cx: &mut Context<TableState<ResourceTable>>,
) -> AnyElement {
    let theme = cx.theme();
    let cpu = col == Col::Cpu;
    let format: fn(f64) -> String = if cpu { k8s::format_cpu } else { k8s::format_bytes };
    let (value, color) = if cpu { (used.0, theme.blue) } else { (used.1, theme.success) };
    let (request, limit) = res.map(|r| if cpu { r.cpu } else { r.mem }).unwrap_or_default();
    let figure = div().w(px(52.)).flex_shrink_0().text_right().child(format(value));
    if request.is_none() && limit.is_none() {
        // Nothing to measure it against: a bar would only show the scale.
        let note = if res.is_some() { "no request" } else { "" };
        return h_flex()
            .size_full()
            .gap_2()
            .child(figure)
            .child(div().text_xs().text_color(theme.muted_foreground).whitespace_nowrap().child(note))
            .into_any_element();
    }
    // The scale leaves room past the furthest mark, so none sits on the edge.
    let max = [Some(value), request, limit].into_iter().flatten().fold(0.0, f64::max).max(1e-9) * 1.15;
    let at = |v: f64| relative((v / max).clamp(0.0, 1.0) as f32);
    let fill = match (request, limit) {
        (_, Some(l)) if value >= l * 0.9 => theme.danger,
        (Some(r), _) if value > r => theme.warning,
        _ => color,
    };
    let tip = [
        Some(format!("using {}", format(value))),
        request.map(|r| format!("request {}", format(r))),
        Some(limit.map_or("no limit".to_string(), |l| format!("limit {}", format(l)))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let tick =
        |v: f64, color: Hsla| div().absolute().top(px(-3.)).h(px(12.)).w(px(2.)).rounded_sm().bg(color).left(at(v));
    h_flex()
        .id(SharedString::from(format!("{}-{uid}", if cpu { "cpu" } else { "mem" })))
        .size_full()
        .gap_2()
        .tooltip(move |w, cx| component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
        .child(figure)
        .child(
            div()
                .relative()
                .flex_1()
                .h(px(6.))
                .rounded_full()
                .bg(theme.secondary)
                .child(div().absolute().left_0().top_0().h_full().rounded_full().bg(fill).w(at(value)))
                .when_some(request, |d, r| d.child(tick(r, theme.foreground.opacity(0.7))))
                .when_some(limit, |d, l| d.child(tick(l, theme.danger))),
        )
        .into_any_element()
}

#[cfg(test)]
mod link_tests {
    use super::{link_kind_of, linkable};

    #[test]
    fn links() {
        assert_eq!(link_kind_of("Secret Name"), Some("secrets"));
        assert_eq!(link_kind_of("NODE"), Some("nodes"));
        assert_eq!(link_kind_of("Status"), None);
        // An Issuer or a Store may be cluster-wide: no link that could open the wrong kind.
        assert_eq!(link_kind_of("Issuer"), None);
        assert_eq!(link_kind_of("Store"), None);
        assert_eq!(link_kind_of("ClusterIssuer"), Some("clusterissuers"));
        assert!(linkable("db-creds"));
        assert!(!linkable("<none>") && !linkable("a,b") && !linkable(" ") && !linkable("ns/name"));
    }
}
