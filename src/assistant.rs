//! In-app assistant: Claude, using Recouvrance's own MCP server as an agent named
//! "Claude". Its access, approvals and audit log are the ones any agent
//! gets (Agents page). It runs through your local Claude Code (your subscription,
//! no key needed) or, optionally, the Anthropic API with a key. The key and the
//! agent token are stored with mode 0600.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use serde_json::{Value, json};

use crate::k8s::{run, run_abortable};
use crate::mcp::Hub;
use crate::private_files::{PrivateDir, TempFile};

/// The one agent: your Claude Code over MCP and the in-app assistant share it.
pub const AGENT_NAME: &str = "Claude";
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
const MAX_STEPS: usize = 16;
/// Longest one HTTP request may take: a tool call can wait for an approval, then run.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn read_secret(name: &str) -> Option<String> {
    std::fs::read_to_string(crate::paths::config_dir().join(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_secret(name: &str, value: &str) -> std::io::Result<()> {
    crate::private_files::write_atomic(&crate::paths::config_dir().join(name), value.as_bytes())
}

/// The API key: saved in Settings, else `ANTHROPIC_API_KEY`.
pub fn api_key() -> Option<String> {
    read_secret("anthropic-key").or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()))
}

pub fn save_api_key(key: &str) -> std::io::Result<()> {
    if key.trim().is_empty() {
        let _ = std::fs::remove_file(crate::paths::config_dir().join("anthropic-key"));
        return Ok(());
    }
    write_secret("anthropic-key", key.trim())
}

/// The main agent (created read-only, never Secrets, the first time) and its token.
/// It is the agent named "Claude" only: another agent's token is never rotated for it.
pub fn main_agent(hub: &Hub) -> Result<(String, String)> {
    let existing = main_agent_id(hub);
    let saved =
        read_secret("main-agent").and_then(|s| s.split_once(' ').map(|(id, t)| (id.to_string(), t.to_string())));
    match (existing, saved) {
        // The saved token is used only while it is still this agent's ("New token" replaces it).
        (Some(id), Some((sid, token))) if id == sid && hub.agent_by_token(&token).is_some_and(|a| a.id == id) => {
            Ok((id, token))
        }
        (Some(id), _) => {
            // The token is only stored hashed: issue a new one (Connect Claude Code updates it there).
            let token = hub.rotate_token(&id)?;
            save_main_token(&id, &token)?;
            Ok((id, token))
        }
        (None, _) => {
            let (agent, token) = hub.create_agent(AGENT_NAME.into())?;
            hub.update_agent(&agent.id, |a| {
                a.policy.rules[0].contexts = vec!["*".into()];
                a.policy.rules[0].note = "Read everything (allow more below)".into();
            })?;
            save_main_token(&agent.id, &token)?;
            Ok((agent.id, token))
        }
    }
}

fn main_agent_id(hub: &Hub) -> Option<String> {
    hub.config().agents.iter().find(|a| a.name == AGENT_NAME).map(|a| a.id.clone())
}

/// After "New token" on the Agents page: the main agent's new token is the one the assistant uses.
pub fn token_rotated(hub: &Hub, id: &str, token: &str) -> std::io::Result<()> {
    if main_agent_id(hub).as_deref() != Some(id) {
        return Ok(());
    }
    save_main_token(id, token)
}

fn save_main_token(id: &str, token: &str) -> std::io::Result<()> {
    write_secret("main-agent", &format!("{id} {token}"))
}

pub fn agent_token(hub: &Hub) -> Result<String> {
    main_agent(hub).map(|(_, t)| t)
}

/// Register Recouvrance's MCP server in your Claude Code (user scope), replacing an old entry.
pub async fn connect_claude_code(hub: Arc<Hub>) -> Result<String> {
    let bin = claude_code().context("Claude Code is not installed (claude.com/claude-code)")?;
    let token = agent_token(&hub)?;
    let url = format!("http://127.0.0.1:{}/mcp", hub.port());
    run(async move {
        // Replace our entry if it is there already.
        let _ =
            tokio::process::Command::new(&bin).args(["mcp", "remove", "recouvrance", "--scope", "user"]).output().await;
        let out = tokio::process::Command::new(&bin)
            .args([
                "mcp",
                "add",
                "--scope",
                "user",
                "--transport",
                "http",
                "recouvrance",
                &url,
                "--header",
                &format!("Authorization: Bearer {token}"),
            ])
            .output()
            .await?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(format!("Claude Code now has Recouvrance's tools (server \"recouvrance\" → {url})"))
    })
    .await
}

/// Whether your Claude Code has the "recouvrance" MCP server registered.
pub async fn claude_code_connected() -> bool {
    let Some(bin) = claude_code() else { return false };
    run(async move {
        let out = tokio::process::Command::new(&bin).args(["mcp", "get", "recouvrance"]).output().await?;
        Ok(out.status.success())
    })
    .await
    .unwrap_or(false)
}

// MARK: HTTP

/// Where the API key is sent. `RECOUVRANCE_ANTHROPIC_URL` (a local stand-in for tests)
/// only exists in test and dev builds, so a release never sends the key elsewhere.
fn anthropic_url() -> String {
    #[cfg(any(test, feature = "dev"))]
    if let Ok(url) = std::env::var("RECOUVRANCE_ANTHROPIC_URL") {
        return url;
    }
    "https://api.anthropic.com".into()
}

async fn post(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<(http::HeaderMap, String)> {
    tokio::time::timeout(REQUEST_TIMEOUT, post_inner(url, headers, body))
        .await
        .map_err(|_| anyhow::anyhow!("no answer after {} minutes", REQUEST_TIMEOUT.as_secs() / 60))?
}

async fn post_inner(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<(http::HeaderMap, String)> {
    let https = hyper_rustls::HttpsConnectorBuilder::new().with_native_roots()?.https_or_http().enable_http1().build();
    let client: hyper_util::client::legacy::Client<_, Full<Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
    let mut req = http::Request::post(url).header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Full::new(Bytes::from(serde_json::to_vec(body)?)))?;
    let resp = client.request(req).await?;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await?.to_bytes();
    let text = String::from_utf8_lossy_owned(bytes.into());
    if !parts.status.is_success() {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(String::from))
            .unwrap_or(text);
        bail!("HTTP {}: {msg}", parts.status);
    }
    Ok((parts.headers, text))
}

/// GET a URL as text (follows a few redirects); for loading YAML from the web.
/// At most 10 MB, 20 s overall, and never from https down to http.
pub async fn get_url(url: String) -> Result<String> {
    const MAX_BYTES: usize = 10 * 1024 * 1024;
    let fetch = async move {
        let https =
            hyper_rustls::HttpsConnectorBuilder::new().with_native_roots()?.https_or_http().enable_http1().build();
        let client: hyper_util::client::legacy::Client<_, Full<Bytes>> =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
        let mut url = url;
        for _ in 0..5 {
            let req = http::Request::get(&url).header("user-agent", "recouvrance").body(Full::new(Bytes::new()))?;
            let resp = client.request(req).await?;
            let (parts, body) = resp.into_parts();
            if parts.status.is_redirection() {
                let next = parts
                    .headers
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .context("redirect without a location")?;
                let next = resolve_location(&url, next).context("bad redirect location")?;
                if url.starts_with("https://") && !next.starts_with("https://") {
                    bail!("refusing a redirect from https to {next}");
                }
                url = next;
                continue;
            }
            let too_big = || anyhow::anyhow!("larger than {} MB", MAX_BYTES / (1024 * 1024));
            let declared = parts
                .headers
                .get(http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if declared.is_some_and(|n| n > MAX_BYTES as u64) {
                return Err(too_big());
            }
            let bytes = http_body_util::Limited::new(body, MAX_BYTES)
                .collect()
                .await
                .map_err(|e| if e.is::<http_body_util::LengthLimitError>() { too_big() } else { anyhow::anyhow!(e) })?
                .to_bytes();
            if !parts.status.is_success() {
                bail!("HTTP {}", parts.status);
            }
            return Ok(String::from_utf8_lossy_owned(bytes.into()));
        }
        bail!("too many redirects")
    };
    tokio::time::timeout(std::time::Duration::from_secs(20), fetch).await.context("timed out after 20 s")?
}

/// A redirect's Location made absolute against the URL that answered (RFC 3986).
fn resolve_location(base: &str, location: &str) -> Option<String> {
    let base = base.split('#').next()?;
    let (scheme, rest) = base.split_once("://")?;
    let (authority, path_query) = match rest.find(['/', '?']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let base_path = path_query.split('?').next().unwrap_or("");
    let loc = location.trim();
    // Absolute: a scheme before any '/', '?' or '#'.
    if let Some(i) = loc.find(':')
        && !loc[..i].is_empty()
        && loc[..i].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return Some(loc.to_string());
    }
    if let Some(r) = loc.strip_prefix("//") {
        return Some(format!("{scheme}://{r}"));
    }
    let (path, tail) = match loc.find(['?', '#']) {
        Some(i) => loc.split_at(i),
        None => (loc, ""),
    };
    if path.is_empty() {
        // Only a query or a fragment: same path (and same query for a fragment).
        let q = if tail.starts_with('#') { path_query.find('?').map_or("", |i| &path_query[i..]) } else { "" };
        return Some(format!("{scheme}://{authority}{base_path}{q}{tail}"));
    }
    let merged = if path.starts_with('/') {
        path.to_string()
    } else {
        let dir = base_path.rfind('/').map_or("/", |i| &base_path[..=i]);
        format!("{dir}{path}")
    };
    // Remove `.` and `..` segments.
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = merged.split('/').skip(1).collect();
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    Some(format!("{scheme}://{authority}/{}{tail}", out.join("/")))
}

#[cfg(test)]
mod get_url_tests {
    use super::resolve_location as r;

    #[test]
    fn resolves_redirects() {
        let base = "https://example.com/a/b/c.yaml?x=1";
        assert_eq!(r(base, "https://other.io/y").as_deref(), Some("https://other.io/y"));
        assert_eq!(r(base, "http://other.io/y").as_deref(), Some("http://other.io/y"));
        assert_eq!(r(base, "//cdn.io/z").as_deref(), Some("https://cdn.io/z"));
        assert_eq!(r(base, "/root.yaml").as_deref(), Some("https://example.com/root.yaml"));
        assert_eq!(r(base, "d.yaml").as_deref(), Some("https://example.com/a/b/d.yaml"));
        assert_eq!(r(base, "../d.yaml?q=2").as_deref(), Some("https://example.com/a/d.yaml?q=2"));
        assert_eq!(r(base, "./").as_deref(), Some("https://example.com/a/b/"));
        assert_eq!(r("https://example.com", "x").as_deref(), Some("https://example.com/x"));
        assert_eq!(r(base, "?y=2").as_deref(), Some("https://example.com/a/b/c.yaml?y=2"));
    }
}

/// A JSON-RPC result from a streamable HTTP response (JSON or SSE).
fn rpc_result(text: &str) -> Result<Value> {
    let json = if text.trim_start().starts_with('{') {
        serde_json::from_str::<Value>(text)?
    } else {
        let data = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .rfind(|l| l.starts_with('{'))
            .context("empty MCP response")?;
        serde_json::from_str::<Value>(data)?
    };
    if let Some(e) = json.get("error") {
        bail!("{}", e["message"].as_str().unwrap_or("MCP error"));
    }
    Ok(json["result"].clone())
}

/// A session with Recouvrance's MCP server, as the assistant agent.
pub(crate) struct Mcp {
    url: String,
    token: String,
    session: String,
}

impl Mcp {
    pub(crate) async fn connect(port: u16, token: String) -> Result<Self> {
        let url = format!("http://127.0.0.1:{port}/mcp");
        let auth = format!("Bearer {token}");
        let headers = [("authorization", auth.as_str()), ("accept", "application/json, text/event-stream")];
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "recouvrance-assistant", "version": "1"}}});
        let (h, _) = post(&url, &headers, &init).await.context("Recouvrance's MCP server (is agent access on?)")?;
        let session = h.get("mcp-session-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let mcp = Self { url, token, session };
        let _ = mcp.call_raw(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
        Ok(mcp)
    }

    async fn call_raw(&self, body: Value) -> Result<String> {
        let auth = format!("Bearer {}", self.token);
        let headers = [
            ("authorization", auth.as_str()),
            ("accept", "application/json, text/event-stream"),
            ("mcp-session-id", self.session.as_str()),
        ];
        Ok(post(&self.url, &headers, &body).await?.1)
    }

    async fn tools(&self) -> Result<Vec<Value>> {
        let r = rpc_result(&self.call_raw(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await?)?;
        Ok(r["tools"].as_array().cloned().unwrap_or_default())
    }

    /// Text output of a tool; denials and errors come back as text for the model to read.
    pub(crate) async fn call(&self, name: &str, args: Value) -> String {
        let body =
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": name, "arguments": args}});
        match self.call_raw(body).await.and_then(|t| rpc_result(&t)) {
            Ok(r) => r["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            Err(e) => format!("error: {e:#}"),
        }
    }
}

// MARK: Conversation

/// What the assistant did, for the chat view.
#[derive(Clone, Debug)]
pub enum Step {
    Text(String),
    /// A tool call started (its result follows as `Tool`).
    Running {
        name: String,
        args: String,
    },
    Tool {
        name: String,
        args: String,
        result: String,
    },
}

/// One question → Claude, calling Recouvrance's tools until it answers.
/// `history` is the Messages API conversation so far (updated in place).
/// A chat so far: API messages, or the Claude Code session to resume.
#[derive(Clone, Default)]
pub struct Conversation {
    pub messages: Vec<Value>,
    pub session: Option<String>,
    /// Claude Code's working directory, kept for the chat so `--resume` finds its session.
    work: Option<Arc<PrivateDir>>,
}

/// How the assistant reaches Claude.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Your local Claude Code (`claude -p`): your subscription, no API key.
    ClaudeCode(PathBuf),
    /// The Anthropic API with your key.
    Api,
}

/// The `claude` CLI, also where installers put it when PATH misses it (desktop launchers).
pub fn claude_code() -> Option<PathBuf> {
    let home = crate::paths::home();
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([home.join(".local/bin"), home.join(".claude/local"), PathBuf::from("/usr/local/bin")])
        .map(|d| d.join("claude"))
        .find(|p| p.is_file())
}

/// The configured backend (`assistant_backend`), if usable.
pub fn backend() -> Option<Backend> {
    use crate::settings::AssistantBackend;
    let code = || claude_code().map(Backend::ClaudeCode);
    let api = || api_key().map(|_| Backend::Api);
    match crate::settings::with(|s| s.assistant_backend) {
        AssistantBackend::ClaudeCode => code(),
        AssistantBackend::Api => api(),
        AssistantBackend::Auto => code().or_else(api),
    }
}

/// One question → Claude, calling Recouvrance's tools until it answers. Dropping the
/// future stops it, and Claude Code with it.
pub async fn ask(
    hub: Arc<Hub>,
    conv: Conversation,
    question: String,
    scope: String,
    on_step: impl FnMut(Step) + Send + 'static,
) -> Result<Conversation> {
    match backend().context("no assistant backend: install Claude Code, or add an API key")? {
        Backend::ClaudeCode(bin) => ask_claude_code(hub, bin, conv, question, scope, on_step).await,
        Backend::Api => {
            let model = crate::settings::get().assistant_model;
            let messages = ask_api(hub, model, conv.messages, question, scope, on_step).await?;
            Ok(Conversation { messages, ..Conversation::default() })
        }
    }
}

fn system_prompt(scope: &str, repos: &[PathBuf]) -> String {
    let mut prompt = format!(
        "You are the assistant inside Recouvrance, a Kubernetes desktop app. Answer from the live clusters using the Recouvrance tools. \
         The user is looking at: {scope}. Start with my_permissions if unsure what you may do. \
         Prefer find_problems, describe_resource, get_events and get_logs to investigate. \
         Never change anything unless the user explicitly asks; writes may need the user's approval in the app. \
         Be concise: short paragraphs, kubectl-style names, and quote the evidence (event or log line)."
    );
    if !repos.is_empty() {
        let list: Vec<String> = repos.iter().map(|r| r.display().to_string()).collect();
        prompt.push_str(&format!(
            "\n\nThe source code of the team's services can be read with Read, Grep and Glob (read-only) in: {}. \
             Look there for the code behind an error, its configuration and its Helm chart.",
            list.join(", ")
        ));
    }
    let notes = crate::settings::get().assistant_notes;
    if !notes.trim().is_empty() {
        prompt.push_str(&format!(
            "\n\nThe team's notes for investigations (where things run, dashboards, runbooks, repositories). \
             Follow them where they apply:\n{}",
            notes.trim()
        ));
    }
    prompt
}

/// The question the Diagnosis card's "Investigate" asks: find the cause of what the
/// diagnosis found, read-only, from the object's own evidence.
pub fn investigation(context: &str, kind: &str, namespace: Option<&str>, name: &str, findings: &str) -> String {
    let target = match namespace {
        Some(ns) => format!("{kind} {name} in namespace {ns}"),
        None => format!("{kind} {name}"),
    };
    format!(
        "Investigate why {target} (context {context}) is failing. Recouvrance's diagnosis:\n{findings}\n\n\
         Find the root cause from the evidence: describe_resource on it and on the workload that owns it, get_events, \
         get_logs (with previous=true after a restart or OOMKilled), its usage_history against requests and limits, \
         and what changed recently (rollout revisions, Helm release, GitOps). Use the team's notes and code where they apply. \
         Change nothing. Answer with: the cause, the evidence (quote the lines), and the fix (the command or manifest change)."
    )
}

/// The team's code folders the assistant may read (Claude Code only): those that exist.
fn code_dirs() -> Vec<PathBuf> {
    crate::settings::get().assistant_repos.iter().map(|r| crate::paths::expand(r)).filter(|p| p.is_dir()).collect()
}

/// The `claude -p` arguments. The question comes last, after `--`, so one starting with `-`
/// is never read as an option.
fn claude_args(
    question: &str,
    mcp_config: &std::path::Path,
    scope: &str,
    model: &str,
    session: Option<&str>,
    repos: &[PathBuf],
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["-p", "--output-format", "stream-json", "--verbose", "--strict-mcp-config"]
            .into_iter()
            .map(Into::into)
            .collect();
    if repos.is_empty() {
        args.extend(["--tools".into(), "".into(), "--allowedTools".into(), "mcp__recouvrance".into()]);
    } else {
        // Only the file readers, and `--restricted` keeps them inside the working
        // directories: the code folders and the private one (no kubeconfig, no keys).
        args.extend(
            ["--restricted", "--tools", "Read,Grep,Glob", "--allowedTools", "mcp__recouvrance,Read,Grep,Glob"]
                .map(Into::into),
        );
        for r in repos {
            args.extend(["--add-dir".into(), r.into()]);
        }
    }
    args.extend(["--mcp-config".into(), mcp_config.into()]);
    args.extend(["--append-system-prompt".into(), system_prompt(scope, repos).into()]);
    if model != DEFAULT_MODEL && !model.is_empty() {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(s) = session {
        args.extend(["--resume".into(), s.into()]);
    }
    args.extend(["--".into(), question.into()]);
    args
}

/// Through the local Claude Code CLI (your login/subscription): only Recouvrance's MCP
/// server is available to it, its built-in tools are off.
async fn ask_claude_code(
    hub: Arc<Hub>,
    bin: PathBuf,
    conv: Conversation,
    question: String,
    scope: String,
    mut on_step: impl FnMut(Step) + Send + 'static,
) -> Result<Conversation> {
    use tokio::io::AsyncBufReadExt as _;
    let token = agent_token(&hub)?;
    let port = hub.port();
    let model = crate::settings::get().assistant_model;
    run_abortable(async move {
        // A private working dir (no project CLAUDE.md) and an MCP config readable only by you.
        let work = match &conv.work {
            Some(w) => w.clone(),
            None => Arc::new(PrivateDir::create().context("creating the assistant's private directory")?),
        };
        let cfg = json!({"mcpServers": {"recouvrance": {"type": "http", "url": format!("http://127.0.0.1:{port}/mcp"), "headers": {"Authorization": format!("Bearer {token}")}}}});
        let cfg_file = TempFile::create(work.path().join(format!("mcp-{}.json", uuid::Uuid::new_v4().simple())), cfg.to_string().as_bytes())?;
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.current_dir(work.path())
            .args(claude_args(&question, cfg_file.path(), &scope, &model, conv.session.as_deref(), &code_dirs()))
            // `claude -p` reads a stdin that is not a terminal: give it none.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("starting claude")?;
        // Read alongside stdout, keeping the last line: a full stderr pipe would block claude.
        let stderr = child.stderr.take().context("no error output")?;
        let last_err = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            let mut last = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    last = line;
                }
            }
            last
        });
        let mut lines = tokio::io::BufReader::new(child.stdout.take().context("no output")?).lines();
        let mut pending: HashMap<String, (String, String)> = HashMap::new();
        let mut session = conv.session.clone();
        let mut error = None;
        while let Some(line) = lines.next_line().await? {
            let Ok(ev) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(id) = ev["session_id"].as_str() {
                session = Some(id.to_string());
            }
            match ev["type"].as_str() {
                Some("assistant") => {
                    for b in ev.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                        match b["type"].as_str() {
                            Some("text") => on_step(Step::Text(b["text"].as_str().unwrap_or_default().to_string())),
                            Some("tool_use") => {
                                let name = b["name"].as_str().unwrap_or_default().trim_start_matches("mcp__recouvrance__").to_string();
                                on_step(Step::Running { name: name.clone(), args: b["input"].to_string() });
                                pending.insert(b["id"].as_str().unwrap_or_default().to_string(), (name, b["input"].to_string()));
                            }
                            _ => {}
                        }
                    }
                }
                Some("user") => {
                    for b in ev.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                        if b["type"] != "tool_result" {
                            continue;
                        }
                        let result = match &b["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
                            other => other.to_string(),
                        };
                        if let Some((name, args)) = pending.remove(b["tool_use_id"].as_str().unwrap_or_default()) {
                            on_step(Step::Tool { name, args, result });
                        }
                    }
                }
                Some("result") if ev["is_error"] == true => error = Some(ev["result"].as_str().unwrap_or("Claude Code failed").to_string()),
                _ => {}
            }
        }
        let status = child.wait().await?;
        drop(cfg_file);
        if let Some(e) = error {
            bail!("{e}");
        }
        if !status.success() {
            let err = last_err.await.unwrap_or_default();
            bail!("claude exited with {status}: {}", err.trim());
        }
        Ok(Conversation { messages: Vec::new(), session, work: Some(work) })
    })
    .await
}

async fn ask_api(
    hub: Arc<Hub>,
    model: String,
    history: Vec<Value>,
    question: String,
    scope: String,
    mut on_step: impl FnMut(Step) + Send + 'static,
) -> Result<Vec<Value>> {
    let key = api_key().context("no Anthropic API key (add one on the Assistant page)")?;
    let token = agent_token(&hub)?;
    let port = hub.port();
    run_abortable(async move {
        let mcp = Mcp::connect(port, token).await?;
        let tools: Vec<Value> = mcp
            .tools()
            .await?
            .into_iter()
            .map(|t| json!({"name": t["name"], "description": t["description"], "input_schema": t["inputSchema"]}))
            .collect();
        let system = system_prompt(&scope, &[]);
        let mut messages = history;
        messages.push(json!({"role": "user", "content": question}));
        let headers = [("x-api-key", key.as_str()), ("anthropic-version", "2023-06-01")];
        for _ in 0..MAX_STEPS {
            let body =
                json!({"model": model, "max_tokens": 4096, "system": system, "tools": tools, "messages": messages});
            let api = anthropic_url();
            let (_, text) = post(&format!("{api}/v1/messages"), &headers, &body).await?;
            let resp: Value = serde_json::from_str(&text)?;
            let cut_off = resp["stop_reason"] == "max_tokens";
            let content = reply_content(&resp);
            if !content.is_empty() {
                messages.push(json!({"role": "assistant", "content": content}));
            }
            let mut results = Vec::new();
            for block in &content {
                match block["type"].as_str() {
                    Some("text") => on_step(Step::Text(block["text"].as_str().unwrap_or_default().to_string())),
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        on_step(Step::Running { name: name.clone(), args: block["input"].to_string() });
                        let out = mcp.call(&name, block["input"].clone()).await;
                        on_step(Step::Tool { name, args: block["input"].to_string(), result: out.clone() });
                        results.push(json!({"type": "tool_result", "tool_use_id": block["id"], "content": out}));
                    }
                    _ => {}
                }
            }
            if cut_off {
                on_step(Step::Text("(the reply was cut off)".into()));
            }
            if results.is_empty() {
                return Ok(messages);
            }
            messages.push(json!({"role": "user", "content": results}));
        }
        on_step(Step::Text("(stopped after too many tool calls)".into()));
        Ok(messages)
    })
    .await
}

/// The blocks of an API reply worth keeping. Tool calls count only when the reply stopped
/// to run them: one cut off (`max_tokens`) may be incomplete, and is neither run nor kept
/// (a kept call with no result would break every later question).
fn reply_content(resp: &Value) -> Vec<Value> {
    let mut content = resp["content"].as_array().cloned().unwrap_or_default();
    if resp["stop_reason"] != "tool_use" {
        content.retain(|b| b["type"] != "tool_use");
    }
    content
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn cut_off_tool_calls_are_dropped() {
        let text = json!({"type": "text", "text": "Let me look"});
        let call = json!({"type": "tool_use", "id": "t1", "name": "list_resources", "input": {}});
        let reply = |stop: &str| json!({"stop_reason": stop, "content": [text.clone(), call.clone()]});
        assert_eq!(super::reply_content(&reply("tool_use")), vec![text.clone(), call.clone()]);
        assert_eq!(super::reply_content(&reply("max_tokens")), vec![text.clone()]);
        assert!(super::reply_content(&json!({"stop_reason": "max_tokens", "content": [call]})).is_empty());
    }

    #[test]
    fn question_is_never_an_option() {
        let args =
            super::claude_args("--model x -n kube-system", "/tmp/mcp.json".as_ref(), "pods", "", Some("s1"), &[]);
        let n = args.len();
        assert_eq!(args[n - 2], "--");
        assert_eq!(args[n - 1], "--model x -n kube-system");
        assert_eq!(args.iter().filter(|a| *a == "--model").count(), 0);
        assert!(args.windows(2).any(|w| w[0] == "--resume" && w[1] == "s1"));
    }

    #[test]
    fn investigation_names_the_object_and_its_findings() {
        let q = super::investigation("kind-demo", "Pod", Some("shop"), "api-1", "- CrashLoopBackOff");
        assert!(q.contains("Pod api-1 in namespace shop (context kind-demo)"));
        assert!(q.contains("- CrashLoopBackOff"));
        assert!(q.contains("Change nothing"));
        assert!(super::investigation("c", "Node", None, "n1", "").contains("Node n1 (context c)"));
    }

    #[test]
    fn code_folders_are_read_only_and_confined() {
        let none = super::claude_args("q", "/tmp/mcp.json".as_ref(), "pods", "", None, &[]);
        assert!(none.windows(2).any(|w| w[0] == "--tools" && w[1].is_empty()));
        assert!(!none.iter().any(|a| a == "--restricted" || a == "--add-dir"));
        let repos = [PathBuf::from("/src/a"), PathBuf::from("/src/b")];
        let args = super::claude_args("q", "/tmp/mcp.json".as_ref(), "pods", "", None, &repos);
        assert!(args.iter().any(|a| a == "--restricted"));
        assert!(args.windows(2).any(|w| w[0] == "--tools" && w[1] == "Read,Grep,Glob"));
        let dirs: Vec<_> = args.windows(2).filter(|w| w[0] == "--add-dir").map(|w| w[1].clone()).collect();
        assert_eq!(dirs, ["/src/a", "/src/b"]);
        assert!(!args.iter().any(|a| a.to_string_lossy().contains("Bash")));
    }

    #[test]
    fn parses_sse_and_json() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[1]}}\n\n";
        assert_eq!(super::rpc_result(sse).unwrap()["tools"][0], 1);
        assert_eq!(super::rpc_result("{\"result\":{\"x\":2}}").unwrap()["x"], 2);
        assert!(super::rpc_result("{\"error\":{\"message\":\"no\"}}").is_err());
    }
}
