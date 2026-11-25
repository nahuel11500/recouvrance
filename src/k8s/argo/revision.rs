//! Revisions and image tags made short to read.

/// Characters of a commit sha kept, as git shows it.
const KEEP: usize = 7;

/// A revision or an image tag as shown: each commit sha in it (40 hex characters, or a
/// sha256's 64, standing alone or after a `-`, `:`, `/`...) cut to its first 7, so that
/// `main-eab9300daf858b46f4c0e5a983b5474428e9096c` reads `main-eab9300`. Anything else (a
/// version, a chart's, a branch) is kept whole. Where it is shown cut, show the whole value on
/// hover and copy it whole.
pub fn short_revision(rev: &str) -> String {
    let bytes = rev.as_bytes();
    let alnum = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_alphanumeric);
    let mut out = String::with_capacity(rev.len());
    let mut i = 0;
    while i < bytes.len() {
        // A run of hex digits that starts a word.
        if bytes[i].is_ascii_hexdigit() && (i == 0 || !alnum(i - 1)) {
            let end = bytes[i..].iter().position(|b| !b.is_ascii_hexdigit()).map_or(bytes.len(), |n| i + n);
            let sha = matches!(end - i, 40 | 64) && !alnum(end);
            // Hex digits are ASCII: these are character boundaries.
            out.push_str(&rev[i..if sha { i + KEEP } else { end }]);
            i = end;
        } else {
            let c = rev[i..].chars().next().unwrap_or_default();
            out.push(c);
            i += c.len_utf8().max(1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::short_revision;
    use proptest::prelude::*;

    #[test]
    fn a_sha_is_cut_and_its_prefix_kept() {
        let sha = "eab9300daf858b46f4c0e5a983b5474428e9096c";
        assert_eq!(short_revision(sha), "eab9300");
        assert_eq!(short_revision(&format!("main-{sha}")), "main-eab9300");
        assert_eq!(short_revision(&format!("release-2024-11-{sha}")), "release-2024-11-eab9300");
        assert_eq!(short_revision(&format!("sha256:{sha}{}", &sha[..24])), "sha256:eab9300");
        assert_eq!(short_revision(&format!("api main-{sha}, worker {sha}")), "api main-eab9300, worker eab9300");
        // Kept whole: versions, a word that only ends in hex, a run too short or too long.
        for kept in ["1.5.0", "v2.14.3", "latest", "@0a1b2c3d4e5f", "deadbeef", &format!("x{sha}"), &format!("{sha}0")]
        {
            assert_eq!(short_revision(kept), kept);
        }
    }

    /// Hex digits, `n` of them.
    fn hex(n: usize) -> impl Strategy<Value = String> {
        proptest::collection::vec(proptest::sample::select(b"0123456789abcdefABCDEF".to_vec()), n)
            .prop_map(|b| String::from_utf8(b).unwrap_or_default())
    }

    proptest! {
        /// Never longer, cut only once, and a branch's sha leaves its branch and 7 digits.
        #[test]
        fn never_longer(s in "\\PC{0,80}", prefix in "[a-z][a-z0-9.]{0,15}-", sha in hex(40)) {
            let short = short_revision(&s);
            prop_assert!(short.len() <= s.len());
            prop_assert_eq!(short_revision(&short), short);
            let tagged = short_revision(&format!("{prefix}{sha}"));
            prop_assert_eq!(&tagged, &format!("{prefix}{}", &sha[..7]));
            prop_assert!(tagged.len() <= 16 + 1 + 7);
        }

        /// A tag with no sha in it is kept as it is.
        #[test]
        fn a_tag_without_a_sha_is_kept(tag in "[a-zA-Z0-9._-]{0,60}") {
            let has_sha = tag
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| matches!(w.len(), 40 | 64) && w.chars().all(|c| c.is_ascii_hexdigit()));
            prop_assume!(!has_sha);
            prop_assert_eq!(short_revision(&tag), tag);
        }
    }
}
