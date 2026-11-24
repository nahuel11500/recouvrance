//! Where Recouvrance keeps its files, following the XDG base directories.

use std::path::{Path, PathBuf};

/// The user's home directory (empty when `HOME` is not set).
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_default(PathBuf::from)
}

/// `$XDG_CONFIG_HOME/recouvrance`, by default `~/.config/recouvrance`.
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("recouvrance")
}

/// `$XDG_DATA_HOME/recouvrance`, by default `~/.local/share/recouvrance`.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("recouvrance")
}

/// Where short-lived private files go: `$XDG_RUNTIME_DIR` (a per-user tmpfs) when it is a
/// real directory, the system temporary directory otherwise.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// `~/Downloads`, where saved logs, files and kubeconfigs go.
pub fn downloads_dir() -> PathBuf {
    home().join("Downloads")
}

/// `~/.kube/config`, used when `KUBECONFIG` is not set.
pub fn default_kubeconfig() -> PathBuf {
    home().join(".kube").join("config")
}

/// A path typed by the user, with `~/` expanded.
pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// A file or folder of the config directory as the texts name it: `~/.config/recouvrance/x`,
/// or where `XDG_CONFIG_HOME` puts it. A text never names the default folder itself.
pub fn shown_config(rest: &str) -> String {
    shown(&config_dir(), &home(), rest)
}

/// A file or folder of the data directory as the texts name it:
/// `~/.local/share/recouvrance/x`, or where `XDG_DATA_HOME` puts it.
pub fn shown_data(rest: &str) -> String {
    shown(&data_dir(), &home(), rest)
}

/// `rest` under `base`, as the texts name it: with `~/` when it is under `home`.
fn shown(base: &Path, home: &Path, rest: &str) -> String {
    tilde_in(&base.join(rest), home)
}

/// `/home/me/x` as `~/x`, the way paths are kept in settings and shown.
pub fn tilde(p: &Path) -> String {
    tilde_in(p, &home())
}

fn tilde_in(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

/// An XDG base directory: the variable when it holds an absolute path (the spec says to
/// ignore a relative one), else `fallback` under the home directory.
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{config_dir, data_dir, expand, home, shown, shown_config, shown_data, tilde};

    #[test]
    fn shown_paths_are_the_ones_in_use() {
        assert_eq!(shown_config("rules.yaml"), tilde(&config_dir().join("rules.yaml")));
        assert_eq!(shown_data("audit.jsonl"), tilde(&data_dir().join("audit.jsonl")));
        let home = Path::new("/home/me");
        // Outside the home folder (an XDG variable set elsewhere): the path as it is.
        assert_eq!(shown(Path::new("/srv/cfg/recouvrance"), home, "rules.yaml"), "/srv/cfg/recouvrance/rules.yaml");
        // Under it: with `~/`, whatever the folder.
        assert_eq!(
            shown(Path::new("/home/me/.config/recouvrance"), home, "audit.jsonl"),
            "~/.config/recouvrance/audit.jsonl"
        );
        assert_eq!(
            shown(Path::new("/home/me/scratch/data/recouvrance"), home, "timeline"),
            "~/scratch/data/recouvrance/timeline"
        );
        // No home folder known: never `~/`.
        assert_eq!(shown(Path::new("/srv/cfg"), Path::new(""), "x"), "/srv/cfg/x");
    }

    /// The code of a line as far as this check goes: None for a comment line or one marked
    /// `allow-default-path`, else the line without a trailing `// …` comment (cut only where the
    /// quotes before it are balanced, so `"https://…"` stays). `in_block` follows `/* … */`.
    fn code_of<'a>(line: &'a str, in_block: &mut bool) -> Option<&'a str> {
        let code = line.trim_start();
        if *in_block {
            *in_block = !code.contains("*/");
            return None;
        }
        if code.starts_with("/*") {
            *in_block = !code.contains("*/");
            return None;
        }
        if code.starts_with("//") || code.contains("allow-default-path") {
            return None;
        }
        let cut = code.match_indices("//").map(|(i, _)| i).find(|&i| code[..i].matches('"').count().is_multiple_of(2));
        Some(cut.map_or(code, |i| &code[..i]))
    }

    #[test]
    fn comments_are_left_out_of_the_check() {
        let mut block = false;
        assert_eq!(code_of("    /// ~/x", &mut block), None);
        assert_eq!(code_of(r#"let a = "https://x"; // b"#, &mut block), Some(r#"let a = "https://x"; "#));
        assert_eq!(code_of("/* a", &mut block), None);
        assert_eq!(code_of("still a comment", &mut block), None);
        assert_eq!(code_of("*/", &mut block), None);
        assert_eq!(code_of("code", &mut block), Some("code"));
        assert_eq!(code_of(r#"f("x") // allow-default-path"#, &mut block), None);
    }

    /// The folders move with `XDG_CONFIG_HOME` and `XDG_DATA_HOME`: code names them with
    /// `shown_config` / `shown_data`, never with the defaults written out (#903). Comments may;
    /// a line that must can say `allow-default-path`.
    #[test]
    fn no_code_names_the_default_folders() {
        fn walk(dir: &Path, skip: &Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, skip, found);
                } else if path.extension().is_some_and(|e| e == "rs") && path != skip {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let mut in_block = false;
                    for (i, line) in text.lines().enumerate() {
                        let defaults = [concat!(".con", "fig/recouvrance"), concat!(".local/", "share/recouvrance")];
                        if code_of(line, &mut in_block).is_some_and(|code| defaults.iter().any(|d| code.contains(d))) {
                            found.push(format!("{}:{}", path.display(), i + 1));
                        }
                    }
                }
            }
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        walk(&src, &src.join("paths.rs"), &mut found);
        assert!(found.is_empty(), "default folders written out: {found:?}");
    }

    #[test]
    fn tilde_and_expand_round_trip() {
        let p = home().join("x").join("y.yaml");
        assert_eq!(tilde(&p), "~/x/y.yaml");
        assert_eq!(expand("~/x/y.yaml"), p);
        assert_eq!(expand("/etc/x"), std::path::PathBuf::from("/etc/x"));
        assert_eq!(tilde(Path::new("/etc/x")), "/etc/x");
    }
}
