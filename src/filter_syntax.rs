//! The table filter's syntax: terms parsed from what you type, and how a cell compares to a
//! value ("restarts>5", "cpu>=100m", "age<1h"). Matching against rows is in the app.
//!
//! The fuzz targets in `fuzz/` build this file on its own: keep it to std, regex and serde.

use crate::glob::glob;
use crate::quantity::{parse_bytes, parse_cpu};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Op {
    Glob,
    Gt,
    Ge,
    Lt,
    Le,
}

pub enum Term {
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

/// The terms of a filter, each with whether it is negated (`!web`, `-web`).
pub fn parse_terms(input: &str) -> Vec<(bool, Term)> {
    input
        .split_whitespace()
        .filter_map(|raw| {
            let (negated, t) = match raw.strip_prefix('!').or_else(|| raw.strip_prefix('-').filter(|r| !r.is_empty())) {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            if t.is_empty() {
                return None;
            }
            let term = if let Some(c) = column_term(t) {
                c
            } else if let Some(re) = t.strip_prefix('/').and_then(|r| r.strip_suffix('/')).filter(|r| !r.is_empty()) {
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
        .collect()
}

/// `name:glob`, `name>v`, `name>=v`, `name<v`, `name<=v` with a column-like name.
/// `ns:` and `l:` keep their meaning; `k=v` stays a label match.
pub fn column_term(t: &str) -> Option<Term> {
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

#[derive(Clone, Copy)]
pub enum Unit {
    Cpu,
    Duration,
    Bytes,
    Plain,
}

pub fn unit_of(column: &str) -> Unit {
    match column {
        "cpu" => Unit::Cpu,
        "memory" | "size" | "capacity" | "storage" => Unit::Bytes,
        c if c == "age" || c.starts_with("last") || c.contains("duration") || c.ends_with("ago") => Unit::Duration,
        _ => Unit::Plain,
    }
}

/// A cell as a number for its column: `3 (2h ago)` → 3, `250m` → 250 millicores,
/// `1.5Gi` → bytes, `5d3h` → seconds, `75%` → 75.
pub fn quantity(text: &str, unit: Unit) -> Option<f64> {
    let t = text.trim();
    let first = t.split([' ', '/', ',']).next().unwrap_or(t).trim_end_matches('%');
    match unit {
        Unit::Cpu => {
            // A bare number means cores, like in a resource spec.
            let v = parse_cpu(first);
            (v > 0.0 || first.starts_with('0')).then_some(v)
        }
        Unit::Bytes => {
            let v = parse_bytes(first);
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
            let v = parse_bytes(first);
            (v > 0.0).then_some(v)
        }),
    }
}

pub fn compare(column: &str, have: &str, op: Op, want: &str) -> bool {
    if op == Op::Glob {
        let (h, w) = (have.to_lowercase(), want.to_lowercase());
        return if w.contains('*') { glob(&w, &h) } else { h == w || h.split([' ', ',']).any(|p| p == w) };
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
pub fn contains_ci(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        hay.as_bytes().windows(needle.len().max(1)).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
            || needle.is_empty()
    } else {
        hay.to_lowercase().contains(needle)
    }
}
