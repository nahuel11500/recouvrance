//! Counts in words: "1 key", "3 keys", "0 keys". Every count followed by its noun goes through
//! here, so a count of 1 never reads "1 keys".

use std::fmt::Display;

/// `n` and its noun, plural unless `n` is 1: `count(3, "key")` is "3 keys". For a noun whose
/// plural is not the singular plus "s", use `count_as`.
pub fn count(n: impl Display, one: &str) -> String {
    let n = n.to_string();
    if n == "1" { format!("1 {one}") } else { format!("{n} {one}s") }
}

/// `n` and its noun, with the plural given: `count_as(2, "success", "successes")`.
pub fn count_as(n: impl Display, one: &str, many: &str) -> String {
    let n = n.to_string();
    if n == "1" { format!("1 {one}") } else { format!("{n} {many}") }
}

/// The words that go with `n` when the number is written apart: `pick(n, "line", "lines")` for
/// "… 3 more lines".
pub fn pick<'a>(n: impl Display, one: &'a str, many: &'a str) -> &'a str {
    if n.to_string() == "1" { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn one_is_singular_and_every_other_count_plural() {
        assert_eq!(count(1usize, "key"), "1 key");
        assert_eq!(count(0usize, "key"), "0 keys");
        assert_eq!(count(2i64, "reason"), "2 reasons");
        assert_eq!(count(-1i64, "day"), "-1 days");
        assert_eq!(count("1", "core"), "1 core");
        assert_eq!(count_as(1u32, "success", "successes"), "1 success");
        assert_eq!(count_as(3u32, "success", "successes"), "3 successes");
        assert_eq!(pick(1, "differs", "differ"), "differs");
        assert_eq!(pick(4, "differs", "differ"), "differ");
    }

    proptest! {
        #[test]
        fn the_noun_is_singular_exactly_when_the_count_is_one(n in any::<i64>()) {
            let text = count(n, "pod");
            prop_assert_eq!(text.ends_with("pods"), n != 1);
            prop_assert!(text.starts_with(&format!("{n} ")));
        }
    }
}
