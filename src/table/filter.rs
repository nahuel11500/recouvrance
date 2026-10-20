//! The table filter: words, labels, namespaces, regexes and column expressions.

#[allow(unused_imports)]
use super::*;

/// The table filter. Space-separated terms, all must match:
/// `web` text in any column · `!web` / `-web` not · `app=web`, `app!=web`, `app` label (`l:app`)
/// · `ns:team-*` namespace glob · `/^web-\d+$/` regex on the name · quotes are not needed.
pub struct RowFilter {
    pub(super) terms: Vec<(bool, Term)>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Op {
    Glob,
    Gt,
    Ge,
    Lt,
    Le,
}

pub(super) enum Term {
    /// A column compared to a value: `status:Running`, `restarts>5`, `cpu>=100m`, `age<1h`.
    /// The last field is the term as typed (lowercase): when the table has no such
    /// column (`error:timeout`, `10:30`), it is searched as text instead.
    Column(String, Op, String, String),
    Text(String),
    Label(String, Option<String>),
    LabelNot(String, String),
    Namespace(String),
    Regex(regex::Regex),
}

impl RowFilter {
    pub fn parse(input: &str) -> Self {
        let terms = input
            .split_whitespace()
            .filter_map(|raw| {
                let (negated, t) =
                    match raw.strip_prefix('!').or_else(|| raw.strip_prefix('-').filter(|r| !r.is_empty())) {
                        Some(rest) => (true, rest),
                        None => (false, raw),
                    };
                if t.is_empty() {
                    return None;
                }
                let term = if let Some(c) = column_term(t) {
                    c
                } else if let Some(re) = t.strip_prefix('/').and_then(|r| r.strip_suffix('/')).filter(|r| !r.is_empty())
                {
                    // A regex still being typed (invalid) is ignored.
                    Term::Regex(regex::RegexBuilder::new(re).case_insensitive(true).build().ok()?)
                } else if let Some(ns) = t.strip_prefix("ns:") {
                    Term::Namespace(ns.to_string())
                } else if let Some(key) = t.strip_prefix("l:") {
                    match key.split_once('=') {
                        Some((k, v)) => Term::Label(k.to_string(), Some(v.to_string())),
                        None => Term::Label(key.to_string(), None),
                    }
                } else if let Some((k, v)) = t.split_once("!=") {
                    Term::LabelNot(k.to_string(), v.to_string())
                } else if let Some((k, v)) = t.split_once('=').filter(|(k, _)| !k.is_empty()) {
                    Term::Label(k.to_string(), Some(v.to_string()))
                } else {
                    Term::Text(t.to_lowercase())
                };
                Some((negated, term))
            })
            .collect();
        Self { terms }
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
                Term::Label(k, Some(v)) => r.label(k).is_some_and(|l| crate::mcp::glob(v, l)),
                Term::LabelNot(k, v) => !r.label(k).is_some_and(|l| crate::mcp::glob(v, l)),
                Term::Namespace(g) => r.namespace.as_deref().is_some_and(|ns| crate::mcp::glob(g, ns)),
                Term::Regex(re) => re.is_match(&r.name),
            };
            hit != *negated
        })
    }
}

/// `name:glob`, `name>v`, `name>=v`, `name<v`, `name<=v` with a column-like name.
/// `ns:` and `l:` keep their meaning; `k=v` stays a label match.
pub(super) fn column_term(t: &str) -> Option<Term> {
    let pos = t.find([':', '<', '>'])?;
    let (name, rest) = t.split_at(pos);
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || matches!(name, "ns" | "l")
    {
        return None;
    }
    let (op, value) = if let Some(v) = rest.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = rest.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = rest.strip_prefix('>') {
        (Op::Gt, v)
    } else if let Some(v) = rest.strip_prefix('<') {
        (Op::Lt, v)
    } else {
        (Op::Glob, &rest[1..])
    };
    if value.is_empty() {
        return None;
    }
    Some(Term::Column(name.to_lowercase().replace(['-', '_'], ""), op, value.to_string(), t.to_lowercase()))
}

/// Text anywhere in the row: name, namespace or a cell.
fn text_match(r: &Row, t: &str) -> bool {
    contains_ci(&r.name, t)
        || r.namespace.as_deref().is_some_and(|ns| contains_ci(ns, t))
        || r.cells.iter().skip(1).any(|c| contains_ci(c, t))
}

#[derive(Clone, Copy)]
pub(super) enum Unit {
    Cpu,
    Duration,
    Bytes,
    Plain,
}

pub(super) fn unit_of(column: &str) -> Unit {
    match column {
        "cpu" => Unit::Cpu,
        "memory" | "size" | "capacity" | "storage" => Unit::Bytes,
        c if c == "age" || c.starts_with("last") || c.contains("duration") || c.ends_with("ago") => Unit::Duration,
        _ => Unit::Plain,
    }
}

/// A cell as a number for its column: `3 (2h ago)` → 3, `250m` → 250 millicores,
/// `1.5Gi` → bytes, `5d3h` → seconds, `75%` → 75.
pub(super) fn quantity(text: &str, unit: Unit) -> Option<f64> {
    let t = text.trim();
    let first = t.split([' ', '/', ',']).next().unwrap_or(t).trim_end_matches('%');
    match unit {
        Unit::Cpu => {
            // A bare number means cores, like in a resource spec.
            let v = k8s::parse_cpu(first);
            (v > 0.0 || first.starts_with('0')).then_some(v)
        }
        Unit::Bytes => {
            let v = k8s::parse_bytes(first);
            (v > 0.0 || first.starts_with('0')).then_some(v)
        }
        Unit::Duration => {
            if let Ok(n) = first.parse::<f64>() {
                return Some(n);
            }
            let (mut total, mut num) = (0.0, String::new());
            for c in first.chars() {
                if c.is_ascii_digit() || c == '.' {
                    num.push(c);
                    continue;
                }
                let unit = match c {
                    's' => 1.0,
                    'm' => 60.0,
                    'h' => 3600.0,
                    'd' => 86400.0,
                    'y' => 31_536_000.0,
                    _ => return None,
                };
                total += num.parse::<f64>().ok()? * unit;
                num.clear();
            }
            num.is_empty().then_some(total)
        }
        Unit::Plain => first.parse::<f64>().ok().or_else(|| {
            let v = k8s::parse_bytes(first);
            (v > 0.0).then_some(v)
        }),
    }
}

pub(super) fn compare(column: &str, have: &str, op: Op, want: &str) -> bool {
    if op == Op::Glob {
        let (h, w) = (have.to_lowercase(), want.to_lowercase());
        return if w.contains('*') { crate::mcp::glob(&w, &h) } else { h == w || h.split([' ', ',']).any(|p| p == w) };
    }
    let unit = unit_of(column);
    let (Some(h), Some(w)) = (quantity(have, unit), quantity(want, unit)) else { return false };
    match op {
        Op::Gt => h > w,
        Op::Ge => h >= w,
        Op::Lt => h < w,
        Op::Le => h <= w,
        Op::Glob => unreachable!(),
    }
}

/// Case-insensitive `contains` for an already lowercase needle, without allocating.
pub(super) fn contains_ci(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        hay.as_bytes().windows(needle.len().max(1)).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
            || needle.is_empty()
    } else {
        hay.to_lowercase().contains(needle)
    }
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
            cells: cells.iter().map(|c| c.to_string()).collect(),
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
