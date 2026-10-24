//! Where Recouvrance keeps its files, following the XDG base directories.

use std::path::{Path, PathBuf};

/// The user's home directory (empty when `HOME` is not set).
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
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

/// `/home/me/x` as `~/x`, the way paths are kept in settings.
pub fn tilde(p: &Path) -> String {
    let home = home();
    match p.strip_prefix(&home) {
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
    use super::{expand, home, tilde};

    #[test]
    fn tilde_and_expand_round_trip() {
        let p = home().join("x").join("y.yaml");
        assert_eq!(tilde(&p), "~/x/y.yaml");
        assert_eq!(expand("~/x/y.yaml"), p);
        assert_eq!(expand("/etc/x"), std::path::PathBuf::from("/etc/x"));
        assert_eq!(tilde(std::path::Path::new("/etc/x")), "/etc/x");
    }
}
