//! Reading fields of Kubernetes objects held as JSON, by JSON pointer (`/status/phase`).
//! Missing and mistyped fields read alike: as nothing.

use serde_json::Value;

/// The string at `path`, or None.
pub fn str_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    v.pointer(path).and_then(Value::as_str)
}

/// The string at `path`, or "" when it is missing.
pub fn str_or_empty<'a>(v: &'a Value, path: &str) -> &'a str {
    str_at(v, path).unwrap_or_default()
}

/// The string at `path` when it is there and not empty.
pub fn nonempty_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    str_at(v, path).filter(|s| !s.is_empty())
}

/// The list at `path`, or an empty one.
pub fn arr_at<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    v.pointer(path).and_then(Value::as_array).map_or(&[][..], Vec::as_slice)
}

/// The whole number at `path`, or None.
pub fn i64_at(v: &Value, path: &str) -> Option<i64> {
    v.pointer(path).and_then(Value::as_i64)
}

/// The timestamp at `path` (RFC 3339, as Kubernetes writes them), or None.
pub fn time_at(v: &Value, path: &str) -> Option<jiff::Timestamp> {
    str_at(v, path)?.parse().ok()
}

/// The same value, the keys of every object in another order (picked by `seed`): for tests
/// of views that must not care about key order.
#[cfg(test)]
pub fn reordered(v: &Value, seed: u64) -> Value {
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(&String, &Value)> = m.iter().collect();
            entries.sort_by_key(|(k, _)| {
                let mut h = DefaultHasher::new();
                (seed, k.as_str()).hash(&mut h);
                h.finish()
            });
            Value::Object(entries.into_iter().map(|(k, v)| (k.clone(), reordered(v, seed))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| reordered(i, seed)).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn missing_and_mistyped_read_as_nothing() {
        let v = json!({"a": {"s": "x", "e": "", "n": 3, "l": [1], "t": "2026-01-01T00:00:00Z"}});
        assert_eq!(super::str_at(&v, "/a/s"), Some("x"));
        assert_eq!(super::str_at(&v, "/a/n"), None);
        assert_eq!(super::str_or_empty(&v, "/a/missing"), "");
        assert_eq!(super::nonempty_at(&v, "/a/e"), None);
        assert_eq!(super::arr_at(&v, "/a/l").len(), 1);
        assert!(super::arr_at(&v, "/a/s").is_empty());
        assert_eq!(super::i64_at(&v, "/a/n"), Some(3));
        assert!(super::time_at(&v, "/a/t").is_some());
        assert!(super::time_at(&v, "/a/s").is_none());
    }
}
