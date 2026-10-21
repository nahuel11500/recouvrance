//! Plugins: your own commands as actions on resources.
//! Declared in ~/.config/recouvrance/plugins.yaml:
//!
//! ```yaml
//! plugins:
//!   - name: Tail with stern
//!     kinds: [deployments, statefulsets]
//!     command: stern -n $NAMESPACE $NAME
//!   - name: Open in Grafana
//!     kinds: [pods]
//!     command: xdg-open "https://grafana.example/d/pods?var-pod=$RECOUVRANCE_NAME"
//!     mode: background
//!     read_only: true
//! ```
//!
//! A plugin may change the cluster, so it needs write mode (and the context name typed on
//! protected contexts) unless it declares `read_only: true`.
//!
//! The object is passed in environment variables (`RECOUVRANCE_CONTEXT`, `RECOUVRANCE_NAMESPACE`,
//! `RECOUVRANCE_NAME`, `RECOUVRANCE_KIND`), never pasted into the command, so no value can
//! break out of its quotes. The shorthands `$CONTEXT`, `$NAMESPACE`, `$NAME` and `$KIND` become
//! references to those variables, quoted for where they stand. `KUBECONFIG` points at a
//! private copy with the context selected, so `kubectl` just works.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Run in a terminal page (interactive tools, streaming output).
    #[default]
    Terminal,
    /// Run in the background; the last line of output is shown in a notification.
    Background,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Plugin {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// kubectl names (`pods`, `deployments`, `certificates.cert-manager.io`) or `*`.
    #[serde(default)]
    pub kinds: Vec<String>,
    pub command: String,
    #[serde(default)]
    pub mode: Mode,
    /// Ask before running.
    #[serde(default)]
    pub confirm: bool,
    /// Only reads: runs in read-only mode and without the protected-context guard.
    /// Unset means it may write.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Deserialize, Default)]
struct File {
    #[serde(default)]
    plugins: Vec<Plugin>,
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("recouvrance").join("plugins.yaml")
}

/// All plugins, or the error that kept the file from loading.
pub fn load() -> Result<Vec<Plugin>, String> {
    match std::fs::read_to_string(path()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
        Ok(text) => crate::yaml::from_str::<File>(&text).map(|f| f.plugins).map_err(|e| {
            let at = e.location().map(|l| format!("line {}: ", l.line())).unwrap_or_default();
            format!("{at}{}", crate::yaml::message(&e))
        }),
    }
}

impl Plugin {
    /// Offered for this kind (`kubectl_name` like `deployments.apps`)?
    pub fn applies(&self, kubectl_name: &str, plural: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|k| k == "*" || k == kubectl_name || k == plural)
    }

    /// The shell command, with each shorthand turned into a reference to its environment
    /// variable, quoted for where it stands (bare, in double quotes or in single quotes).
    pub fn script(&self) -> String {
        let mut out = String::new();
        let mut quote = Quote::None;
        let mut rest = self.command.as_str();
        while let Some(c) = rest.chars().next() {
            let var = VARS.iter().find(|v| c == '$' && rest[1..].starts_with(**v));
            if let Some(v) = var {
                out.push_str(&match quote {
                    Quote::None => format!("\"$RECOUVRANCE_{v}\""),
                    Quote::Double => format!("${{RECOUVRANCE_{v}}}"),
                    Quote::Single => format!("'\"$RECOUVRANCE_{v}\"'"),
                });
                rest = &rest[1 + v.len()..];
                continue;
            }
            let mut len = c.len_utf8();
            match (quote, c) {
                (Quote::None, '\'') => quote = Quote::Single,
                (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
                (Quote::None, '"') => quote = Quote::Double,
                // An escaped character is copied as is (`\$NAME` stays literal).
                (Quote::None | Quote::Double, '\\') => len += rest[1..].chars().next().map_or(0, char::len_utf8),
                _ => {}
            }
            out.push_str(&rest[..len]);
            rest = &rest[len..];
        }
        out
    }
}

/// Shorthands, longest first so `$NAMESPACE` is not read as `$NAME` + "SPACE".
const VARS: [&str; 4] = ["NAMESPACE", "CONTEXT", "NAME", "KIND"];

#[derive(Clone, Copy)]
enum Quote {
    None,
    Single,
    Double,
}

/// The values a plugin reads, as environment variables.
pub fn env(context: &str, namespace: Option<&str>, name: &str, kind: &str) -> Vec<(String, String)> {
    [("CONTEXT", context), ("NAMESPACE", namespace.unwrap_or("")), ("NAME", name), ("KIND", kind)]
        .into_iter()
        .map(|(k, v)| (format!("RECOUVRANCE_{k}"), v.to_string()))
        .collect()
}

/// Plugins loaded at startup (and on "Reload" in Settings), with the load error if any.
#[derive(Default)]
pub struct Loaded(pub Vec<Plugin>, pub Option<String>);

impl gpui_kit::Global for Loaded {}

pub fn reload(cx: &mut gpui_kit::App) {
    let loaded = match load() {
        Ok(p) => Loaded(p, None),
        Err(e) => Loaded(Vec::new(), Some(e)),
    };
    cx.set_global(loaded);
}

/// Plugins offered for a kind.
pub fn for_kind(kubectl_name: &str, plural: &str, cx: &gpui_kit::App) -> Vec<Plugin> {
    cx.try_global::<Loaded>()
        .map(|l| l.0.iter().filter(|p| p.applies(kubectl_name, plural)).cloned().collect())
        .unwrap_or_default()
}

/// Ask the app to run a plugin (its script, and the values it reads from the environment).
pub struct RunPlugin {
    pub plugin: Plugin,
    pub command: String,
    pub env: Vec<(String, String)>,
    pub context: String,
    pub namespace: Option<String>,
}

pub const EXAMPLE: &str = "plugins:\n  - name: Tail with stern\n    kinds: [deployments, statefulsets]\n    command: stern -n $NAMESPACE $NAME\n    read_only: true\n  - name: Describe\n    kinds: ['*']\n    command: kubectl describe \"$RECOUVRANCE_KIND\" \"$RECOUVRANCE_NAME\" -n \"$RECOUVRANCE_NAMESPACE\" | less\n    read_only: true\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_and_matches() {
        let p = Plugin {
            name: "x".into(),
            description: String::new(),
            kinds: vec!["deployments".into()],
            command: "stern -n $NAMESPACE $NAME --context $CONTEXT".into(),
            mode: Mode::Terminal,
            confirm: false,
            read_only: false,
        };
        assert_eq!(
            p.script(),
            "stern -n \"$RECOUVRANCE_NAMESPACE\" \"$RECOUVRANCE_NAME\" --context \"$RECOUVRANCE_CONTEXT\""
        );
        assert!(p.applies("deployments.apps", "deployments"));
        assert!(!p.applies("pods", "pods"));
        let f: File = crate::yaml::from_str(EXAMPLE).unwrap();
        assert_eq!(f.plugins.len(), 2);
        assert!(f.plugins[1].applies("pods", "pods"));
        assert!(f.plugins[0].read_only);
        // Older files have no read_only: their plugins count as writes.
        let f: File = crate::yaml::from_str("plugins: [{name: a, command: x}]").unwrap();
        assert!(!f.plugins[0].read_only);
    }

    fn plugin(command: &str) -> Plugin {
        Plugin {
            name: "x".into(),
            description: String::new(),
            kinds: vec![],
            command: command.into(),
            mode: Mode::Background,
            confirm: false,
            read_only: false,
        }
    }

    #[test]
    fn shorthands_follow_quoting() {
        assert_eq!(
            plugin("open \"u?p=$NAME&n=$NAMESPACE\"").script(),
            "open \"u?p=${RECOUVRANCE_NAME}&n=${RECOUVRANCE_NAMESPACE}\""
        );
        assert_eq!(plugin("echo 'x $KIND'").script(), "echo 'x '\"$RECOUVRANCE_KIND\"''");
        assert_eq!(plugin("echo \\$NAME \"\\$NAME\"").script(), "echo \\$NAME \"\\$NAME\"");
    }

    /// Hostile values are data: they are never run, wherever the shorthand stands.
    #[test]
    fn values_never_run() {
        let evil = "a\"; echo pwned; '$(echo pwned)`echo pwned`";
        for command in ["printf %s $NAME", "printf %s \"$NAME\"", "printf %s '$NAME'"] {
            let out = std::process::Command::new("sh")
                .args(["-c", &plugin(command).script()])
                .envs(env("c", None, evil, "pods"))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), evil, "{command}");
        }
    }
}
