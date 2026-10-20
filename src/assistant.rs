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

use crate::k8s::run;
use crate::mcp::Hub;

/// The one agent: your Claude Code over MCP and the in-app assistant share it.
pub const AGENT_NAME: &str = "Claude";
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";
const MAX_STEPS: usize = 16;

fn dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("recouvrance")
}

fn read_secret(name: &str) -> Option<String> {
    std::fs::read_to_string(dir().join(name)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn write_secret(name: &str, value: &str) -> std::io::Result<()> {
    crate::mcp::write_private(&dir().join(name), value.as_bytes()).map_err(std::io::Error::other)
}

/// The API key: saved in Settings, else ANTHROPIC_API_KEY.
pub fn api_key() -> Option<String> {
    read_secret("anthropic-key").or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()))
}

pub fn save_api_key(key: &str) -> std::io::Result<()> {
    if key.trim().is_empty() {
        let _ = std::fs::remove_file(dir().join("anthropic-key"));
        return Ok(());
    }
    write_secret("anthropic-key", key.trim())
}

/// The main agent (created read-only, never Secrets, the first time) and its token.
/// It is the agent named "Claude" only: another agent's token is never rotated for it.
pub fn main_agent(hub: &Hub) -> Result<(String, String)> {
    let existing = hub.config().agents.iter().find(|a| a.name == AGENT_NAME).map(|a| a.id.clone());
    let saved =
        read_secret("main-agent").and_then(|s| s.split_once(' ').map(|(id, t)| (id.to_string(), t.to_string())));
    match (existing, saved) {
        (Some(id), Some((sid, token))) if id == sid => Ok((id, token)),
        (Some(id), _) => {
            // The token is only stored hashed: issue a new one (Connect Claude Code updates it there).
            let token = hub.rotate_token(&id)?;
            write_secret("main-agent", &format!("{id} {token}"))?;
            Ok((id, token))
        }
        (None, _) => {
            let (agent, token) = hub.create_agent(AGENT_NAME.into())?;
            hub.update_agent(&agent.id, |a| {
                a.policy.rules[0].contexts = vec!["*".into()];
                a.policy.rules[0].note = "Read everything (allow more below)".into();
            })?;
            write_secret("main-agent", &format!("{} {token}", agent.id))?;
            Ok((agent.id, token))
        }
    }
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

/// Where the API key is sent. RECOUVRANCE_ANTHROPIC_URL (a local stand-in for tests)
/// only exists in test and dev builds, so a release never sends the key elsewhere.
fn anthropic_url() -> String {
    #[cfg(any(test, feature = "dev"))]
    if let Ok(url) = std::env::var("RECOUVRANCE_ANTHROPIC_URL") {
        return url;
    }
    "https://api.anthropic.com".into()
}

async fn post(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<(http::HeaderMap, String)> {
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
    let text = String::from_utf8_lossy(&bytes).to_string();
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
            return Ok(String::from_utf8_lossy(&bytes).to_string());
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
        let dir = base_path.rfind('/').map(|i| &base_path[..=i]).unwrap_or("/");
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
struct Mcp {
    url: String,
    token: String,
    session: String,
}

impl Mcp {
    async fn connect(port: u16, token: String) -> Result<Self> {
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
    async fn call(&self, name: &str, args: Value) -> String {
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
    work: Option<Arc<WorkDir>>,
}

/// A private directory (mode 0700, a fresh random name, never a shared one) for Claude
/// Code to run in; removed when the last conversation holding it is dropped.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!("recouvrance-assistant-{}", uuid::Uuid::new_v4().simple()));
        // `create`, not `create_all`: fails rather than reuse a directory someone else made.
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A file deleted when it goes out of scope, whatever happens in between.
struct TempFile(PathBuf);

impl TempFile {
    /// Created new (never an existing file or link), readable by you only.
    fn create(path: PathBuf, contents: &[u8]) -> std::io::Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        let file = Self(path);
        f.write_all(contents)?;
        Ok(file)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// How the assistant reaches Claude.
#[derive(Clone, Debug, PartialEq)]
pub enum Backend {
    /// Your local Claude Code (`claude -p`): your subscription, no API key.
    ClaudeCode(PathBuf),
    /// The Anthropic API with your key.
    Api,
}

/// The `claude` CLI, also where installers put it when PATH misses it (desktop launchers).
pub fn claude_code() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([home.join(".local/bin"), home.join(".claude/local"), PathBuf::from("/usr/local/bin")])
        .map(|d| d.join("claude"))
        .find(|p| p.is_file())
}

/// The configured backend (`assistant_backend`: auto, claude-code or api), if usable.
pub fn backend() -> Option<Backend> {
    let choice = crate::settings::get().assistant_backend;
    let code = || claude_code().map(Backend::ClaudeCode);
    let api = || api_key().map(|_| Backend::Api);
    match choice.as_str() {
        "claude-code" => code(),
        "api" => api(),
        _ => code().or_else(api),
    }
}

/// One question → Claude, calling Recouvrance's tools until it answers.
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

fn system_prompt(scope: &str) -> String {
    format!(
        "You are the assistant inside Recouvrance, a Kubernetes desktop app. Answer from the live clusters using the Recouvrance tools. \
         The user is looking at: {scope}. Start with my_permissions if unsure what you may do. \
         Prefer find_problems, describe_resource, get_events and get_logs to investigate. \
         Never change anything unless the user explicitly asks; writes may need the user's approval in the app. \
         Be concise: short paragraphs, kubectl-style names, and quote the evidence (event or log line)."
    )
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
    run(async move {
        // A private working dir (no project CLAUDE.md) and an MCP config readable only by you.
        let work = match &conv.work {
            Some(w) => w.clone(),
            None => Arc::new(WorkDir::create().context("creating the assistant's private directory")?),
        };
        let cfg = json!({"mcpServers": {"recouvrance": {"type": "http", "url": format!("http://127.0.0.1:{port}/mcp"), "headers": {"Authorization": format!("Bearer {token}")}}}});
        let cfg_file = TempFile::create(work.0.join(format!("mcp-{}.json", uuid::Uuid::new_v4().simple())), cfg.to_string().as_bytes())?;
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.current_dir(&work.0)
            .args(["-p", &question, "--output-format", "stream-json", "--verbose", "--tools", "", "--strict-mcp-config", "--allowedTools", "mcp__recouvrance"])
            .arg("--mcp-config")
            .arg(&cfg_file.0)
            .args(["--append-system-prompt", &system_prompt(&scope)])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if model != DEFAULT_MODEL && !model.is_empty() {
            cmd.args(["--model", &model]);
        }
        if let Some(s) = &conv.session {
            cmd.args(["--resume", s]);
        }
        let mut child = cmd.spawn().context("starting claude")?;
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
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use tokio::io::AsyncReadExt as _;
                let _ = e.read_to_string(&mut err).await;
            }
            bail!("claude exited with {status}: {}", err.lines().last().unwrap_or("").trim());
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
    let key = api_key().context("no Anthropic API key (Settings → Assistant)")?;
    let token = agent_token(&hub)?;
    let port = hub.port();
    run(async move {
        let mcp = Mcp::connect(port, token).await?;
        let tools: Vec<Value> = mcp
            .tools()
            .await?
            .into_iter()
            .map(|t| json!({"name": t["name"], "description": t["description"], "input_schema": t["inputSchema"]}))
            .collect();
        let system = system_prompt(&scope);
        let mut messages = history;
        messages.push(json!({"role": "user", "content": question}));
        let headers = [("x-api-key", key.as_str()), ("anthropic-version", "2023-06-01")];
        for _ in 0..MAX_STEPS {
            let body =
                json!({"model": model, "max_tokens": 4096, "system": system, "tools": tools, "messages": messages});
            let api = anthropic_url();
            let (_, text) = post(&format!("{api}/v1/messages"), &headers, &body).await?;
            let resp: Value = serde_json::from_str(&text)?;
            let content = resp["content"].as_array().cloned().unwrap_or_default();
            messages.push(json!({"role": "assistant", "content": content}));
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
            if resp["stop_reason"] != "tool_use" || results.is_empty() {
                return Ok(messages);
            }
            messages.push(json!({"role": "user", "content": results}));
        }
        on_step(Step::Text("(stopped after too many tool calls)".into()));
        Ok(messages)
    })
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_sse_and_json() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[1]}}\n\n";
        assert_eq!(super::rpc_result(sse).unwrap()["tools"][0], 1);
        assert_eq!(super::rpc_result("{\"result\":{\"x\":2}}").unwrap()["x"], 2);
        assert!(super::rpc_result("{\"error\":{\"message\":\"no\"}}").is_err());
    }

    #[test]
    fn private_work_dir_and_config() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let (a, b) = (super::WorkDir::create().unwrap(), super::WorkDir::create().unwrap());
        assert_ne!(a.0, b.0);
        assert_eq!(mode(&a.0), 0o700);
        let cfg = super::TempFile::create(a.0.join("mcp.json"), b"{}").unwrap();
        assert_eq!(mode(&cfg.0), 0o600);
        // Never written through an existing file.
        assert!(super::TempFile::create(a.0.join("mcp.json"), b"x").is_err());
        let (dir, file) = (a.0.clone(), cfg.0.clone());
        drop(cfg);
        assert!(!file.exists());
        drop(a);
        assert!(!dir.exists());
    }
}
