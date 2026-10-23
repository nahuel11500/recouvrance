//! Sort keys: cells compare as quantities of their column ("10 (5m ago)", "2m", "1Gi").

#[allow(unused_imports)]
use super::*;

/// Numbers sort before text, numbers by value and text alphabetically. Text borrows
/// the cell: sorting a big list allocates nothing per row.
#[derive(Debug)]
pub(super) enum SortKey<'a> {
    Num(f64),
    Text(&'a str),
}

impl Ord for SortKey<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.total_cmp(b),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            (SortKey::Num(_), SortKey::Text(_)) => std::cmp::Ordering::Less,
            (SortKey::Text(_), SortKey::Num(_)) => std::cmp::Ordering::Greater,
        }
    }
}

impl PartialOrd for SortKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SortKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for SortKey<'_> {}

/// A cell's sort key: its quantity for the column when it reads as one, else its text.
pub(super) fn sort_key(text: &str, unit: Unit) -> SortKey<'_> {
    // Plain columns only count a number with an optional size suffix ("3 (2h ago)",
    // "1/1", "10Gi"), so names like "2fa-api" stay text.
    let strict = || {
        let first = text.trim().split([' ', '/', ',']).next().unwrap_or_default().trim_end_matches('%');
        let suffix = first.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        matches!(suffix, "" | "Ki" | "Mi" | "Gi" | "Ti" | "k" | "K" | "M" | "G" | "T")
    };
    let n = match unit {
        Unit::Plain if !strict() => None,
        _ => quantity(text, unit),
    };
    match n {
        Some(n) if n.is_finite() => SortKey::Num(n),
        _ => SortKey::Text(text),
    }
}

impl ResourceTable {
    /// Sort the visible rows by the chosen column.
    pub(super) fn sort_visible(&mut self) {
        let Some((col, sort)) = self.sort else { return };
        let visible = std::mem::take(&mut self.visible);
        let rows = &self.rows;
        let unit = unit_of(&normalized(self.header(col)));
        let key = |i: usize| -> SortKey<'_> {
            let r = &rows[i];
            let ctx = self.row_ctx[i];
            match col {
                Col::Check => SortKey::Num(if self.is_marked(ctx, &r.uid) { 0.0 } else { 1.0 }),
                Col::Context => SortKey::Text(self.contexts.get(ctx).map(String::as_str).unwrap_or_default()),
                Col::Namespace => SortKey::Text(r.namespace.as_deref().unwrap_or_default()),
                Col::Cpu => SortKey::Num(self.usage_of(ctx, r).map_or(-1.0, |u| u.0)),
                Col::Memory => SortKey::Num(self.usage_of(ctx, r).map_or(-1.0, |u| u.1)),
                Col::Age(_) => SortKey::Num(-r.created.map(|t| t.as_second()).unwrap_or(0) as f64),
                Col::Cell(c) => sort_key(r.cells.get(c).map(String::as_str).unwrap_or_default(), unit),
            }
        };
        let mut keyed: Vec<(SortKey, usize)> = visible.into_iter().map(|i| (key(i), i)).collect();
        keyed.sort_by(|a, b| {
            let ord = a.0.cmp(&b.0);
            if sort == ColumnSort::Descending { ord.reverse() } else { ord }
        });
        self.visible = keyed.into_iter().map(|(_, i)| i).collect();
    }
}

/// A header as typed in filters: "Nominated Node" -> "nominatednode".
pub(super) fn normalized(header: &str) -> String {
    header.to_lowercase().replace([' ', '-', '_'], "")
}

#[cfg(test)]
mod sort_tests {
    use super::{SortKey, Unit, sort_key, unit_of};

    fn sorted(cells: &[&str], column: &str) -> Vec<String> {
        let unit: Unit = unit_of(column);
        let mut keyed: Vec<(SortKey, &str)> = cells.iter().map(|c| (sort_key(c, unit), *c)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        keyed.into_iter().map(|(_, c)| c.to_string()).collect()
    }

    #[test]
    fn restarts_sort_by_count() {
        assert_eq!(
            sorted(&["10 (5m ago)", "2", "0", "3 (2h ago)"], "restarts"),
            ["0", "2", "3 (2h ago)", "10 (5m ago)"]
        );
    }

    #[test]
    fn durations_sort_by_length() {
        assert_eq!(sorted(&["10s", "2m", "1h", "45s", "3d"], "lastseen"), ["10s", "45s", "2m", "1h", "3d"]);
    }

    #[test]
    fn quantities_sort_by_value() {
        assert_eq!(sorted(&["1Gi", "512Mi", "2Gi"], "memory"), ["512Mi", "1Gi", "2Gi"]);
        assert_eq!(sorted(&["1", "250m", "100m"], "cpu"), ["100m", "250m", "1"]);
    }

    #[test]
    fn text_sorts_after_numbers() {
        assert_eq!(sorted(&["web", "10", "api", "9", "<none>"], "status"), ["9", "10", "<none>", "api", "web"]);
        assert_eq!(sorted(&["NaN", "1"], "ready"), ["1", "NaN"]);
        assert_eq!(sorted(&["2fa-api", "10", "1/1"], "name"), ["1/1", "10", "2fa-api"]);
    }
}
