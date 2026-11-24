//! Argo CD's own diff, read from its API server: what the next sync would change, as Argo CD
//! renders it from git, against the live objects. The Kubernetes API can't give it: git's
//! manifests are only behind Argo CD's own API and login.
//!
//! Off until a hub context's Argo CD server is set in Settings. It logs in with the token the
//! argocd CLI saved for that server (its config file), or with a read-only token saved in
//! Settings (0600, like the other secrets). It sends one GET, to that server only, when the
//! diff is asked for. The token is never shown: errors are scrubbed of it. A Secret's values
//! are masked here, whatever Argo CD sent (its own masking leaked predicted Secret states
//! before 3.2.12, 3.3.10 and 3.4.2).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use super::diagnosis::Res;
use crate::history::DiffLine;

/// Where the tokens set in Settings are kept, by context (mode 0600).
const TOKENS_FILE: &str = "argocd-tokens.json";
/// The largest answer read: an Application with thousands of resources sends three states each.
pub(super) const MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Resources whose diff is shown, at most; the others are counted.
const MAX_RESOURCES: usize = 200;
/// Lines of one resource's diff shown, at most.
const MAX_LINES: usize = 400;
/// A Secret's value, masked.
const MASK: &str = "••••••";
/// A Secret's value the sync would change, masked.
const MASK_CHANGED: &str = "•••••• (changed)";
/// `kubectl apply`'s copy of the object, values included.
const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

// MARK: Server

/// An Argo CD API server, as set in Settings: `https://argocd.example.com`, a path when Argo
/// CD is served under one (`--rootpath`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// `https://host[:port][/path]`, without a trailing slash.
    pub base: String,
    host: String,
    port: u16,
}

impl Server {
    /// The address typed, `https://` added when it has no scheme. Plain http only to this
    /// machine (a port-forward): a token is never sent in clear over the network.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("No Argo CD server set".into());
        }
        let full = if text.contains("://") { text.to_string() } else { format!("https://{text}") };
        let uri = http::Uri::try_from(full.as_str()).map_err(|_| format!("Not an address: {text}"))?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(format!("Not an http(s) address: {text}")),
        };
        let authority = uri.authority().ok_or_else(|| format!("No host in {text}"))?;
        if authority.as_str().contains('@') || uri.query().is_some() {
            return Err(format!("Give the server's address alone, without a user or a query: {text}"));
        }
        let (host, port) = host_port(authority.as_str(), https).ok_or_else(|| format!("No host in {text}"))?;
        if !https && !loopback(&host) {
            return Err("Use https: a token is only sent over plain http to this machine (a port-forward)".into());
        }
        let default = if https { 443 } else { 80 };
        let scheme = if https { "https" } else { "http" };
        let port_text = if port == default { String::new() } else { format!(":{port}") };
        let path = uri.path().trim_end_matches('/');
        Ok(Self { base: format!("{scheme}://{host}{port_text}{path}"), host, port })
    }

    /// `argocd.example.com`, with its port when it isn't the default: as `argocd login` takes it.
    pub fn name(&self) -> String {
        let rest = self.base.split_once("://").map_or(self.base.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or(rest).to_string()
    }
}

/// The host (lowercase) and port of `host[:port]`, the port of the scheme when none is said.
pub(super) fn host_port(authority: &str, https: bool) -> Option<(String, u16)> {
    let authority: http::uri::Authority = authority.parse().ok()?;
    let host = authority.host().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some((host, authority.port_u16().unwrap_or(if https { 443 } else { 80 })))
}

fn loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The Argo CD server set for `context`, if any (and whether it reads).
pub fn server_of(context: &str) -> Option<Result<Server, String>> {
    let text = crate::settings::with(|s| s.argocd_servers.get(context).cloned())?;
    (!text.trim().is_empty()).then(|| Server::parse(&text))
}

// MARK: Login

/// What the request logs in with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    /// The token set in Settings for the context.
    Settings,
    /// The argocd CLI's login to that server.
    Cli,
}

impl Login {
    pub fn text(self) -> &'static str {
        match self {
            Self::Settings => "the token set in Settings",
            Self::Cli => "the argocd CLI's login",
        }
    }
}

fn tokens_path() -> PathBuf {
    crate::paths::config_dir().join(TOKENS_FILE)
}

/// The tokens set in Settings, by context. An unreadable file holds none: the CLI's login is
/// tried instead, and nothing else depends on it.
fn read_tokens() -> BTreeMap<String, String> {
    std::fs::read(tokens_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The contexts with a token set in Settings (one read of the file for them all).
pub fn token_contexts() -> std::collections::BTreeSet<String> {
    read_tokens().into_iter().filter(|(_, t)| !t.trim().is_empty()).map(|(c, _)| c).collect()
}

/// Keep `token` for `context` (empty: forget it), in a file only the user can read.
pub fn save_token(context: &str, token: &str) -> std::io::Result<()> {
    let mut tokens = read_tokens();
    let token = token.trim();
    if token.is_empty() {
        if tokens.remove(context).is_none() {
            return Ok(());
        }
    } else {
        tokens.insert(context.to_string(), token.to_string());
    }
    if tokens.is_empty() {
        return match std::fs::remove_file(tokens_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let text = serde_json::to_vec_pretty(&tokens).map_err(std::io::Error::other)?;
    crate::private_files::write_atomic(&tokens_path(), &text)
}

/// The argocd CLI's config file: `$ARGOCD_CONFIG_DIR/config`, else `~/.argocd/config` when
/// that folder exists (the CLI's older place), else `$XDG_CONFIG_HOME/argocd/config`.
pub(super) fn cli_config_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("ARGOCD_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("config");
    }
    let legacy = crate::paths::home().join(".argocd");
    if legacy.is_dir() {
        return legacy.join("config");
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(|| crate::paths::home().join(".config"), PathBuf::from);
    config.join("argocd").join("config")
}

/// The argocd CLI's config file (`argocd login` writes it).
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub(super) struct CliConfig {
    pub current_context: String,
    pub contexts: Vec<CliContext>,
    pub servers: Vec<CliServer>,
    pub users: Vec<CliUser>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct CliContext {
    pub name: String,
    pub server: String,
    pub user: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub(super) struct CliServer {
    /// `host[:port]`, without a scheme.
    pub server: String,
    /// `--plaintext`: http, no TLS.
    pub plain_text: bool,
    /// `--insecure`: the server's certificate is not checked.
    pub insecure: bool,
    /// `--grpc-web-root-path`: Argo CD is served under this path.
    pub grpc_web_root_path: String,
    /// `--core`: the CLI talks to the Kubernetes API, there is no API server to ask.
    pub core: bool,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub(super) struct CliUser {
    pub name: String,
    pub auth_token: String,
}

/// The token the argocd CLI saved for `server` in its config `text`: the current context's
/// when it is that server, else the first context that is.
fn cli_token(text: &str, server: &Server) -> Option<String> {
    let config: CliConfig = crate::yaml::from_str(text).ok()?;
    let matches = |c: &CliContext| {
        config.servers.iter().any(|s| {
            s.server == c.server
                && !s.core
                && host_port(&s.server, !s.plain_text).is_some_and(|(h, p)| h == server.host && p == server.port)
        })
    };
    let mut contexts: Vec<&CliContext> = config.contexts.iter().filter(|c| matches(c)).collect();
    contexts.sort_by_key(|c| c.name != config.current_context);
    contexts.iter().find_map(|c| {
        let user = config.users.iter().find(|u| u.name == c.user)?;
        let token = user.auth_token.trim();
        (!token.is_empty()).then(|| token.to_string())
    })
}

/// Whether a JWT's `exp` is past at `now` (seconds). A token that isn't a JWT, or has no
/// `exp`, is left for the server to judge.
pub(super) fn expired(token: &str, now: i64) -> bool {
    let Some(payload) = token.split('.').nth(1) else { return false };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else {
        return false;
    };
    serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v.get("exp")?.as_i64()).is_some_and(|exp| exp <= now)
}

/// The token to send for `context`'s `server`, and where it comes from: Settings first, then
/// the argocd CLI's login. What to do when there is none, or it has expired.
fn login(context: &str, server: &Server) -> Result<(String, Login), String> {
    let name = server.name();
    let relogin = format!("run `argocd login {name}` (with `--sso` for SSO) or set a read-only token in Settings");
    let now = jiff::Timestamp::now().as_second();
    if let Some(token) = read_tokens().remove(context).filter(|t| !t.trim().is_empty()) {
        if expired(&token, now) {
            return Err(format!("The token set in Settings for {context} has expired: set a new one"));
        }
        return Ok((token, Login::Settings));
    }
    let path = cli_config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    match cli_token(&text, server) {
        Some(token) if expired(&token, now) => Err(format!("The argocd CLI's login to {name} has expired: {relogin}")),
        Some(token) => Ok((token, Login::Cli)),
        None => Err(format!("No login to {name} ({} has none): {relogin}", path.display())),
    }
}

/// The Argo CD server set for the hub `context`, the token to send it and where that comes
/// from; or why there is none to ask. Reads files: call it off the UI thread.
pub(super) fn login_for(context: &str) -> Result<(Server, String, Login), String> {
    let server = match server_of(context) {
        Some(Ok(server)) => server,
        Some(Err(e)) => return Err(e),
        None => return Err(format!("No Argo CD server is set for {context} in Settings")),
    };
    let (token, how) = login(context, &server)?;
    Ok((server, token, how))
}

/// GET `path` (of Argo CD's API) on `server` with `token`; errors never hold the token.
pub(super) async fn get_path(server: &Server, token: &str, path: &str) -> Result<Value, String> {
    get_path_within(server, token, path, TIMEOUT).await
}

/// The same, waiting up to `timeout` for the answer.
pub(super) async fn get_path_within(
    server: &Server,
    token: &str,
    path: &str,
    timeout: std::time::Duration,
) -> Result<Value, String> {
    let url = format!("{}{path}", server.base);
    super::api::send_within(http::Method::GET, url, token, None, false, server, timeout).await.map_err(|f| f.text)
}

/// `text` with every copy of `token` replaced: an error never shows it, whatever echoed it.
pub(super) fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() { text.to_string() } else { text.replace(token, "[token]") }
}

// MARK: Request

/// A path segment or query value, percent-encoded but for the characters names use.
pub(super) fn encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The URL of the Application `namespace/name`'s managed resources on `server`.
fn managed_resources_url(server: &Server, namespace: &str, name: &str) -> String {
    format!("{}/api/v1/applications/{}/managed-resources?appNamespace={}", server.base, encode(name), encode(namespace))
}

/// What Argo CD answered when it wasn't a success, said plainly.
pub(super) fn refusal(status: http::StatusCode, body: &str, server: &Server) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").or_else(|| v.get("error")).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect::<String>().trim().to_string());
    let name = server.name();
    match status.as_u16() {
        401 => format!(
            "Argo CD at {name} refused the login (expired or revoked?): run `argocd login {name}` again or set a new token"
        ),
        403 => format!("Argo CD at {name} doesn't let this login read the Application ({said})"),
        404 => format!("Argo CD at {name} has no such Application: is it this context's Argo CD? ({said})"),
        _ if status.is_redirection() => {
            format!("Argo CD at {name} answered with a redirect: set the address it redirects to in Settings")
        }
        _ => format!("Argo CD at {name} answered HTTP {status}: {said}"),
    }
}

/// GET `url` with the token, at most `MAX_BYTES`; redirects are not followed, so the token
/// never goes to another address (see `api::send`).
async fn get(url: String, token: String, server: Server) -> Result<Value> {
    super::api::send(http::Method::GET, url, &token, None, false, &server).await.map_err(|f| anyhow!(f.text))
}

// MARK: Diff

/// What a sync would do to one resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Modified,
    /// Not live yet: the sync creates it.
    Created,
    /// No longer in git: a sync with prune deletes it.
    Pruned,
}

impl Change {
    pub fn text(self) -> &'static str {
        match self {
            Self::Modified => "changed",
            Self::Created => "created by the sync",
            Self::Pruned => "no longer in git: a sync with prune deletes it",
        }
    }
}

/// One resource the next sync would change, as a diff from live (`-`) to git (`+`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceDiff {
    pub res: Res,
    pub change: Change,
    pub lines: Vec<DiffLine>,
    /// Lines of its diff left out (past `MAX_LINES`).
    pub more_lines: usize,
}

/// Argo CD's diff of an Application, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDiff {
    /// `argocd.example.com`: where it was read.
    pub server: String,
    pub login: Login,
    /// The resources that differ, in Argo CD's order, at most `MAX_RESOURCES`.
    pub changed: Vec<ResourceDiff>,
    /// Resources that differ past those shown.
    pub more: usize,
    /// Resources that don't differ.
    pub same: usize,
    /// Hooks left out: they run during a sync rather than being kept in sync.
    pub hooks: usize,
}

/// A state Argo CD sends as JSON text; missing, empty or `null` is nothing.
fn state(item: &Value, key: &str) -> Value {
    item.get(key).and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null)
}

/// The fields a diff shows: not the status (Argo CD doesn't sync it) nor the field managers.
fn cleaned(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("status");
    }
    if let Some(m) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("managedFields");
    }
    crate::history::drop_nulls(&mut v);
    v
}

fn is_secret(res: &Res, live: &Value, target: &Value) -> bool {
    let core_secret = |v: &Value| v.get("kind").and_then(Value::as_str) == Some("Secret") && v["apiVersion"] == "v1";
    (res.group.is_empty() && res.kind == "Secret") || core_secret(live) || core_secret(target)
}

/// Mask a Secret's values on both sides: the keys stay, and a value the sync would change
/// reads as changed, but no value is shown, nor `kubectl apply`'s copy of them.
fn mask_secret_pair(live: &mut Value, target: &mut Value) {
    let pointer = format!("/metadata/annotations/{}", LAST_APPLIED.replace('/', "~1"));
    let live_values: Vec<(String, Option<Value>)> = ["data", "stringData"]
        .into_iter()
        .map(|f| (f.to_string(), live.get(f).cloned()))
        .chain([(pointer.clone(), live.pointer(&pointer).cloned())])
        .collect();
    let mask_all = |v: &mut Value| {
        for field in ["data", "stringData"] {
            if let Some(data) = v.get_mut(field).and_then(Value::as_object_mut) {
                data.values_mut().for_each(|x| *x = Value::String(MASK.into()));
            }
        }
        if let Some(x) = v.pointer_mut(&pointer) {
            *x = Value::String(MASK.into());
        }
    };
    mask_all(live);
    // Target: each value masked, said changed when it differs from live's.
    for (field, before) in live_values {
        if field.starts_with('/') {
            if let Some(x) = target.pointer_mut(&field) {
                let same = before.as_ref() == Some(&*x);
                *x = Value::String((if same { MASK } else { MASK_CHANGED }).into());
            }
            continue;
        }
        if let Some(data) = target.get_mut(&field).and_then(Value::as_object_mut) {
            for (key, x) in data {
                let same = before.as_ref().and_then(|b| b.get(key)) == Some(&*x);
                *x = Value::String((if same { MASK } else { MASK_CHANGED }).into());
            }
        }
    }
}

/// Argo CD's managed resources (its answer) as diffs from live to git: each resource that
/// differs, a Secret's values masked.
pub(super) fn diffs(body: &Value, server: String, login: Login) -> GitDiff {
    let items = body.get("items").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let mut out = GitDiff { server, login, changed: Vec::new(), more: 0, same: 0, hooks: 0 };
    for item in items {
        if item.get("hook").and_then(Value::as_bool).unwrap_or(false) {
            out.hooks += 1;
            continue;
        }
        let Some(res) = Res::of(item) else { continue };
        // Argo CD says whether it differs; an older one may not, then the states tell.
        if item.get("modified").and_then(Value::as_bool) == Some(false) {
            out.same += 1;
            continue;
        }
        let pick = |a: &str, b: &str| match state(item, a) {
            Value::Null => state(item, b),
            v => v,
        };
        let mut live = cleaned(pick("normalizedLiveState", "liveState"));
        let mut target = cleaned(pick("predictedLiveState", "targetState"));
        if is_secret(&res, &live, &target) {
            mask_secret_pair(&mut live, &mut target);
        }
        let change = match (live.is_null(), target.is_null()) {
            (true, false) => Change::Created,
            (false, true) => Change::Pruned,
            _ => Change::Modified,
        };
        let mut lines =
            crate::history::diff_lines(&crate::history::diff_text(&live), &crate::history::diff_text(&target));
        if lines.is_empty() {
            out.same += 1;
            continue;
        }
        if out.changed.len() == MAX_RESOURCES {
            out.more += 1;
            continue;
        }
        let more_lines = lines.len().saturating_sub(MAX_LINES);
        lines.truncate(MAX_LINES);
        out.changed.push(ResourceDiff { res, change, lines, more_lines });
    }
    out
}

/// Argo CD's diff of the Application `namespace/name`, read from the Argo CD server set for
/// its hub `context`. One GET; errors never hold the token.
pub async fn git_diff(context: String, namespace: String, name: String) -> Result<GitDiff, String> {
    // Off the UI thread: the config files are read here too.
    let read = crate::k8s::run_abortable(async move {
        let server = match server_of(&context) {
            Some(Ok(server)) => server,
            Some(Err(e)) => return Ok(Err(e)),
            None => return Ok(Err(format!("No Argo CD server is set for {context}"))),
        };
        let (token, login) = match login(&context, &server) {
            Ok(found) => found,
            Err(e) => return Ok(Err(e)),
        };
        let url = managed_resources_url(&server, &namespace, &name);
        let shown = server.name();
        Ok(match get(url, token.clone(), server).await {
            // A big answer is worked out here too, not on the UI thread.
            Ok(body) => Ok(diffs(&body, shown, login)),
            Err(e) => Err(scrub(&format!("{e:#}"), &token)),
        })
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::{Change, Login, Server, cli_token, diffs, expired, managed_resources_url, scrub};
    use crate::history::DiffLine;
    use base64::Engine as _;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    #[test]
    fn a_server_reads_as_its_https_base() {
        let s = Server::parse("argocd.example.com").unwrap();
        assert_eq!(s.base, "https://argocd.example.com");
        assert_eq!(s.name(), "argocd.example.com");
        let s = Server::parse(" https://ArgoCD.example.com:443/argo-cd/ ").unwrap();
        assert_eq!(s.base, "https://argocd.example.com/argo-cd");
        assert_eq!(s.name(), "argocd.example.com");
        assert_eq!(Server::parse("https://argocd.example.com:8443").unwrap().name(), "argocd.example.com:8443");
        // A token never goes over plain http, but to this machine (a port-forward).
        assert!(Server::parse("http://argocd.example.com").is_err());
        assert_eq!(Server::parse("http://localhost:8080").unwrap().base, "http://localhost:8080");
        assert!(Server::parse("http://127.0.0.1:8080").is_ok());
        assert!(Server::parse("https://user@argocd.example.com").is_err());
        assert!(Server::parse("https://argocd.example.com/?x=1").is_err());
        assert!(Server::parse("ftp://argocd.example.com").is_err());
        assert!(Server::parse("").is_err());
        let s = Server::parse("argocd.example.com/argo").unwrap();
        assert_eq!(
            managed_resources_url(&s, "argocd", "web app"),
            "https://argocd.example.com/argo/api/v1/applications/web%20app/managed-resources?appNamespace=argocd"
        );
    }

    const CLI_CONFIG: &str = "
contexts:
- name: other
  server: other.example.com
  user: other
- name: argocd.example.com
  server: argocd.example.com:443
  user: argocd.example.com
- name: core
  server: kubernetes
  user: core
current-context: other
servers:
- server: other.example.com
  grpc-web: true
- server: argocd.example.com:443
  insecure: false
- server: kubernetes
  core: true
users:
- name: other
  auth-token: other-token
- name: argocd.example.com
  auth-token: the-token
  refresh-token: refresh
";

    #[test]
    fn the_cli_token_is_the_one_of_that_server() {
        let server = Server::parse("https://argocd.example.com").unwrap();
        assert_eq!(cli_token(CLI_CONFIG, &server).as_deref(), Some("the-token"));
        let other = Server::parse("other.example.com").unwrap();
        assert_eq!(cli_token(CLI_CONFIG, &other).as_deref(), Some("other-token"));
        // Another port is another server; core mode has no API server.
        assert_eq!(cli_token(CLI_CONFIG, &Server::parse("argocd.example.com:8443").unwrap()), None);
        assert_eq!(cli_token(CLI_CONFIG, &Server::parse("kubernetes").unwrap()), None);
        assert_eq!(cli_token("not: [yaml", &server), None);
        assert_eq!(cli_token("", &server), None);
    }

    fn jwt(payload: &Value) -> String {
        let b = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!("{}.{}.sig", b(b"{\"alg\":\"HS256\"}"), b(payload.to_string().as_bytes()))
    }

    #[test]
    fn an_expired_jwt_is_seen_before_sending_it() {
        assert!(expired(&jwt(&json!({"exp": 100})), 100));
        assert!(!expired(&jwt(&json!({"exp": 200})), 100));
        // No exp, or not a JWT: the server judges.
        assert!(!expired(&jwt(&json!({"sub": "me"})), 100));
        assert!(!expired("opaque-token", 100));
    }

    fn item(kind: &str, live: &Value, target: &Value) -> Value {
        let text = |v: &Value| if v.is_null() { "null".to_string() } else { v.to_string() };
        json!({
            "group": if kind == "Deployment" { "apps" } else { "" },
            "kind": kind, "namespace": "shop", "name": "web",
            "normalizedLiveState": text(live), "predictedLiveState": text(target),
            "liveState": text(live), "targetState": text(target),
            "modified": live != target,
        })
    }

    fn text_of(lines: &[DiffLine]) -> String {
        lines
            .iter()
            .map(|l| match l {
                DiffLine::Gap => String::new(),
                DiffLine::Same(t) | DiffLine::Removed(t) | DiffLine::Added(t) => t.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn what_the_sync_changes_is_a_diff_from_live_to_git() {
        let live = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web",
            "managedFields": [{"manager": "x"}]}, "spec": {"replicas": 2}, "status": {"replicas": 2}});
        let target = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
            "spec": {"replicas": 3}, "status": {"replicas": 9}});
        let cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "web"}});
        let body = json!({"items": [
            item("Deployment", &live, &target),
            item("ConfigMap", &cm, &cm),
            item("ConfigMap", &Value::Null, &cm),
            item("ConfigMap", &cm, &Value::Null),
            {"kind": "Job", "name": "migrate", "hook": true, "targetState": "{}"},
        ]});
        let d = diffs(&body, "argocd.example.com".into(), Login::Cli);
        assert_eq!((d.same, d.hooks, d.more), (1, 1, 0));
        let changes: Vec<Change> = d.changed.iter().map(|r| r.change).collect();
        assert_eq!(changes, [Change::Modified, Change::Created, Change::Pruned]);
        let lines = &d.changed[0].lines;
        assert!(lines.contains(&DiffLine::Removed("  replicas: 2".into())), "{lines:?}");
        assert!(lines.contains(&DiffLine::Added("  replicas: 3".into())), "{lines:?}");
        // Neither the status nor the field managers.
        let all = text_of(lines);
        assert!(!all.contains("status") && !all.contains("managedFields"), "{all}");
    }

    proptest! {
        /// Whatever a Secret holds, live or in git, none of its values is shown; a value the
        /// sync changes reads as changed, one it keeps doesn't.
        #[test]
        fn a_secret_value_is_never_shown(
            values in prop::collection::btree_map("[a-z]{1,4}", ("[A-Za-z0-9]{6,12}", "[A-Za-z0-9]{6,12}", any::<bool>()), 1..5),
            string_data in any::<bool>(),
            annotate in any::<bool>(),
        ) {
            let field = if string_data { "stringData" } else { "data" };
            let mut live = serde_json::Map::new();
            let mut git = serde_json::Map::new();
            for (k, (a, b, same)) in &values {
                live.insert(k.clone(), json!(a));
                git.insert(k.clone(), json!(if *same { a } else { b }));
            }
            let secret = |data: &serde_json::Map<String, Value>| {
                let mut s = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "web"}, field: data});
                if annotate {
                    let copy = json!({field: data}).to_string();
                    s["metadata"]["annotations"] = json!({ (super::LAST_APPLIED): copy });
                }
                s
            };
            let body = json!({"items": [item("Secret", &secret(&live), &secret(&git))]});
            let d = diffs(&body, "a".into(), Login::Settings);
            let shown: String = d.changed.iter().map(|r| text_of(&r.lines)).collect();
            for (a, b, _) in values.values() {
                prop_assert!(!shown.contains(a.as_str()) && !shown.contains(b.as_str()), "{}", shown);
            }
            let changed = values.values().any(|(a, b, same)| !same && a != b);
            prop_assert_eq!(shown.contains(super::MASK_CHANGED), changed, "{}", shown);
        }

        /// The token is never in what an error says, wherever it was echoed.
        #[test]
        fn an_error_never_shows_the_token(token in "[A-Za-z0-9._-]{8,40}", before in ".{0,20}", after in ".{0,20}") {
            let said = scrub(&format!("{before}{token}{after}{token}"), &token);
            prop_assert!(!said.contains(&token));
        }
    }
}
