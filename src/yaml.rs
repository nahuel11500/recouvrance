//! YAML in and out, through serde-saphyr (safe Rust, maintained; kube uses it too).
//! One place for the parser options, so every caller reads YAML the same way.
//!
//! The fuzz targets in `fuzz/` build this file on its own: keep it to std, regex and serde.

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use serde_saphyr::Error;

/// Only `true`/`false` are booleans: `yes`, `no`, `on`, `off` stay strings, as they did
/// with `serde_yaml_ng` (YAML 1.2), so a ConfigMap value `"yes"` survives an edit.
/// A number with a leading zero is octal, as kubectl reads it: `defaultMode: 0400` is 256.
fn options() -> serde_saphyr::Options {
    serde_saphyr::options! { strict_booleans: true, legacy_octal_numbers: true }
}

pub fn from_str<T: DeserializeOwned>(text: &str) -> Result<T, Error> {
    serde_saphyr::from_str_with_options(text, options())
}

/// Every document of a `---` separated stream, empty ones included (as null).
pub fn documents(text: &str) -> Result<Vec<serde_json::Value>, Error> {
    serde_saphyr::from_multiple_with_options(text, options())
}

/// The text after `---` when `line` starts a new document (`---`, `--- # web`), else None.
pub fn doc_start(line: &str) -> Option<&str> {
    line.strip_prefix("---").filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// The error in one line, without its position or the source excerpt (for the UI, which
/// shows the position itself): "unclosed bracket '['".
pub fn message(e: &Error) -> String {
    let text = e.without_snippet().to_string();
    let first = text.lines().next().unwrap_or_default();
    let first = first.strip_prefix("error: ").unwrap_or(first);
    // "line 3 column 8: …" before, or "… at line 3, column 8" after.
    let first = match first.split_once(": ") {
        Some((at, rest)) if at.starts_with("line ") => rest,
        _ => first,
    };
    first.rsplit_once(" at line ").map_or(first, |(m, _)| m).to_string()
}

pub fn to_string<T: Serialize>(value: &T) -> Result<String, serde_saphyr::ser::Error> {
    serde_saphyr::to_string(value)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    /// What the YAML view shows must read back as the same object.
    #[test]
    fn values_survive_a_round_trip() {
        let v = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "app", "labels": {"app.kubernetes.io/name": "web"}, "annotations": {"a": ""}},
            "data": {
                "yes": "yes", "no": "no", "on": "on", "off": "off", "y": "y",
                "true": "true", "null": "null", "tilde": "~", "empty": "",
                "octal": "0123", "number": "1e3", "float": "1.0", "nan": ".nan", "inf": ".inf",
                "date": "2026-01-01", "colon": "a: b", "dash": "- x", "hash": "a # b", "quote": "it's \"q\"",
                "multi": "line one\nline two\n", "lead": " leading space", "unicode": "é ✓ 漢",
                "star": "*x", "amp": "&x", "bang": "!x", "percent": "%x", "at": "@x", "backtick": "`x",
            },
            "spec": {"replicas": 3, "big": 9007199254740993_u64, "neg": -1, "ratio": 0.5, "flag": false,
                     "list": [], "map": {}, "nested": [{"a": [1, 2]}, null]},
        });
        let text = super::to_string(&v).unwrap();
        let back: Value = super::from_str(&text).unwrap();
        assert_eq!(back, v, "\n{text}");
    }

    #[test]
    fn yaml_11_words_are_strings_and_streams_split() {
        let v: Value = super::from_str("a: yes\nb: off\nc: true\nd: 0x10\n").unwrap();
        assert_eq!(v, json!({"a": "yes", "b": "off", "c": true, "d": 16}));
        let docs = super::documents("kind: A\n---\n---\nkind: B\n").unwrap();
        assert_eq!(docs.iter().filter(|d| !d.is_null()).count(), 2);
        let e = super::documents("kind: X\nspec:\n  bad: [\n").unwrap_err();
        assert_eq!(super::message(&e), "unclosed bracket '['");
        assert_eq!(e.location().map(|l| (l.line(), l.column())), Some((3, 8)));
    }

    /// kubectl reads `0400` as octal; read as decimal it would be mode 0620.
    #[test]
    fn leading_zero_numbers_are_octal() {
        let v: Value = super::from_str("a: 0400\nb: 0644\nc: 0\nd: 10\ne: \"0400\"\nf: 0o17\n").unwrap();
        assert_eq!(v, json!({"a": 256, "b": 420, "c": 0, "d": 10, "e": "0400", "f": 15}));
        let docs = super::documents("defaultMode: 0440\n").unwrap();
        assert_eq!(docs, vec![json!({"defaultMode": 288})]);
    }
}
