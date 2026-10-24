//! The table filter: words, labels, namespaces, regexes and column expressions.

use std::borrow::Cow;

use crate::k8s::Row;

/// The table filter. Space-separated terms, all must match:
/// `web` text in any column · `!web` / `-web` not · `app=web`, `app!=web`, `app` label (`l:app`)
/// · `ns:team-*` namespace glob · `/^web-\d+$/` regex on the name · quotes are not needed.
pub struct RowFilter {
    pub(super) terms: Vec<(bool, Term)>,
}

pub(crate) use crate::filter_syntax::*;

impl RowFilter {
    pub fn parse(input: &str) -> Self {
        Self { terms: parse_terms(input) }
    }

    pub fn matches(&self, r: &Row) -> bool {
        self.matches_with(r, &|_: &str| None::<Cow<str>>)
    }

    /// Column names this filter compares (to resolve before matching).
    pub fn has_columns(&self) -> bool {
        self.terms.iter().any(|(_, t)| matches!(t, Term::Column(..)))
    }

    /// With `column(name)` giving a row's cell text for a column name (normalized:
    /// lowercase, no spaces); a term on an unknown column is plain text search.
    pub fn matches_with<'a>(&self, r: &Row, column: &dyn Fn(&str) -> Option<Cow<'a, str>>) -> bool {
        self.terms.iter().all(|(negated, t)| {
            let hit = match t {
                Term::Column(name, op, want, raw) => match column(name) {
                    Some(have) => compare(name, &have, *op, want),
                    None => text_match(r, raw),
                },
                Term::Text(t) => text_match(r, t),
                Term::Label(k, None) => r.label(k).is_some(),
                Term::Label(k, Some(v)) => r.label(k).is_some_and(|l| crate::glob::glob(v, l)),
                Term::LabelNot(k, v) => !r.label(k).is_some_and(|l| crate::glob::glob(v, l)),
                Term::Namespace(g) => r.namespace.as_deref().is_some_and(|ns| crate::glob::glob(g, ns)),
                Term::Regex(re) => re.is_match(&r.name),
            };
            hit != *negated
        })
    }
}

/// Text anywhere in the row: name, namespace or a cell.
fn text_match(r: &Row, t: &str) -> bool {
    contains_ci(&r.name, t)
        || r.namespace.as_deref().is_some_and(|ns| contains_ci(ns, t))
        || r.cells.iter().skip(1).any(|c| contains_ci(c, t))
}

#[cfg(test)]
mod filter_tests {
    use super::{Row, RowFilter};

    fn row(name: &str, ns: &str, labels: &str, cells: &[&str]) -> Row {
        Row {
            uid: name.into(),
            name: name.into(),
            namespace: Some(ns.into()),
            created: None,
            cells: cells.iter().map(ToString::to_string).collect(),
            labels: labels.into(),
            deleting: false,
            deleted_at: None,
        }
    }

    #[test]
    fn filter_terms() {
        let r = row("web-7f9", "team-a", "app=web\ntier=front", &["web-7f9", "1/1", "Running"]);
        let m = |f: &str| RowFilter::parse(f).matches(&r);
        assert!(m(""));
        assert!(m("WEB run"));
        assert!(!m("!running"));
        assert!(m("-pending"));
        assert!(m("app=web"));
        assert!(m("app=w*"));
        assert!(!m("app!=web"));
        assert!(m("tier!=back"));
        assert!(m("l:tier"));
        assert!(!m("l:missing"));
        assert!(m("ns:team-*"));
        assert!(!m("ns:team-b"));
        assert!(m("/^web-\\w+$/"));
        assert!(!m("/^api/"));
        assert!(m("/[/"), "an invalid regex is ignored");
    }

    #[test]
    fn column_terms() {
        let r = row("web-7f9", "team-a", "", &["web-7f9", "1/1", "Running", "3 (2h ago)", "250m", "1.5Gi", "5d3h"]);
        let cols = ["name", "ready", "status", "restarts", "cpu", "memory", "age"];
        let col = |n: &str| cols.iter().position(|c| *c == n).map(|i| std::borrow::Cow::Borrowed(r.cells[i].as_str()));
        let m = |f: &str| RowFilter::parse(f).matches_with(&r, &col);
        assert!(m("status:running"));
        assert!(!m("status:pending"));
        assert!(m("status:run*"));
        assert!(m("restarts>2"));
        assert!(!m("restarts>=4"));
        assert!(m("cpu>100m"));
        assert!(!m("cpu>0.5"));
        assert!(m("memory>1Gi"));
        assert!(m("age>1d"));
        assert!(!m("age<1h"));
        assert!(m("!status:pending"));
        assert!(m("ns:team-*"), "ns: stays a namespace match");
        assert!(!m("nosuchcolumn:x"));
    }

    #[test]
    fn unknown_columns_are_text() {
        let r = row("api-1", "team-a", "", &["api-1", "CrashLoopBackOff", "error:timeout at 10:30"]);
        let cols = ["name", "status", "message"];
        let col = |n: &str| cols.iter().position(|c| *c == n).map(|i| std::borrow::Cow::Borrowed(r.cells[i].as_str()));
        let m = |f: &str| RowFilter::parse(f).matches_with(&r, &col);
        assert!(m("error:timeout"), "not a column: searched as text");
        assert!(m("10:30"));
        assert!(m("ERROR:Timeout"));
        assert!(!m("!error:timeout"));
        assert!(!m("warning:disk"));
        assert!(m("status:crash*"), "a real column still compares");
        assert!(!m("message:other"));
        // Without column names (plain matching), column-like words are text too.
        assert!(RowFilter::parse("error:timeout").matches(&r));
    }
}
